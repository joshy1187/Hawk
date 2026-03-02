use anyhow::Context;
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    backend::CrosstermBackend,
    prelude::*,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::{
    fs,
    io::{self, Read, Stdout, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn main() -> anyhow::Result<()> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new().context("init app")?;

    let res = run_app(&mut terminal, &mut app);

    restore_terminal(&mut terminal)?;
    res
}

/* ----------------------------- Terminal setup ----------------------------- */

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();

    // No mouse capture => keeps selection + right-click menu working.
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.hide_cursor()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste)?;
    terminal.show_cursor()?;
    Ok(())
}

/* ----------------------------- Shell ----------------------------- */

struct Shell {
    master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    _child: Box<dyn portable_pty::Child + Send>,
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    in_alt_screen: bool,
    esc_tail: Vec<u8>,
}

impl Shell {
    fn spawn(cwd: &Path, rows: u16, cols: u16) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(5),
            cols: cols.max(10),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell_path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let shell_name = Path::new(&shell_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("bash")
            .to_lowercase();

        let (mut cmd, extra_env) = build_shell_command(&shell_path, &shell_name)?;
        cmd.cwd(cwd);
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        );
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = tx.send(buf[..n].to_vec());
                    }
                    Err(_) => break,
                }
            }
        });

        let parser = vt100::Parser::new(rows.max(5), cols.max(10), 20_000);

        Ok(Self {
            master: pair.master,
            writer,
            rx,
            _child: child,
            parser,
            rows: rows.max(5),
            cols: cols.max(10),
            in_alt_screen: false,
            esc_tail: Vec::with_capacity(128),
        })
    }

    fn set_size(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(5);
        let cols = cols.max(10);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;

        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });

        self.parser.set_size(rows, cols);
    }

    fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn observe_alt_screen(&mut self, chunk: &[u8]) {
        const KEEP: usize = 96;
        self.esc_tail.extend_from_slice(chunk);
        if self.esc_tail.len() > KEEP {
            let start = self.esc_tail.len() - KEEP;
            self.esc_tail = self.esc_tail[start..].to_vec();
        }
        let buf = self.esc_tail.as_slice();

        if contains_seq(buf, b"\x1b[?1049h")
            || contains_seq(buf, b"\x1b[?47h")
            || contains_seq(buf, b"\x1b[?1047h")
        {
            self.in_alt_screen = true;
        }
        if contains_seq(buf, b"\x1b[?1049l")
            || contains_seq(buf, b"\x1b[?47l")
            || contains_seq(buf, b"\x1b[?1047l")
        {
            self.in_alt_screen = false;
        }
    }
}

fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/* ----------------------------- App ----------------------------- */

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    is_dir: bool,
}

struct App {
    cwd: PathBuf,
    shell: Shell,

    items: Vec<Entry>,
    show_hidden: bool,
    hud_dirty: bool,
    last_hud_build: Instant,

    should_quit: bool,
}

impl App {
    fn new() -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()?;
        let shell = Shell::spawn(&cwd, 24, 120)?;

        let mut app = Self {
            cwd,
            shell,
            items: vec![],
            show_hidden: false,
            hud_dirty: true,
            last_hud_build: Instant::now() - Duration::from_secs(10),
            should_quit: false,
        };

        app.refresh_hud()?;
        Ok(app)
    }

    fn refresh_hud(&mut self) -> anyhow::Result<()> {
        self.items = read_dir_items(&self.cwd, self.show_hidden)?;
        sort_items(&mut self.items);
        self.hud_dirty = false;
        self.last_hud_build = Instant::now();
        Ok(())
    }

    fn set_cwd(&mut self, new_cwd: PathBuf) -> anyhow::Result<()> {
        if new_cwd == self.cwd {
            return Ok(());
        }
        self.cwd = new_cwd;
        self.hud_dirty = true;
        self.refresh_hud()?;
        Ok(())
    }

    fn toggle_hidden(&mut self) -> anyhow::Result<()> {
        self.show_hidden = !self.show_hidden;
        self.hud_dirty = true;
        self.refresh_hud()?;
        Ok(())
    }

    fn tick(&mut self) -> anyhow::Result<()> {
        while let Ok(chunk) = self.shell.rx.try_recv() {
            self.shell.observe_alt_screen(&chunk);

            if let Some(p) = last_osc7_path(&chunk) {
                let _ = self.set_cwd(p);
            }

            self.shell.parser.process(&chunk);
        }

        if self.hud_dirty && self.last_hud_build.elapsed() > Duration::from_millis(100) {
            self.refresh_hud()?;
        }

        Ok(())
    }

    fn handle_event(&mut self, ev: Event) -> anyhow::Result<()> {
        match ev {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(s) => {
                self.shell.send(s.as_bytes())?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        // HARD RULE: Ctrl+C must ALWAYS go to the shell
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.shell.send(&[0x03])?; // SIGINT
            return Ok(());
        }

        // Quit Hawk (nonstandard)
        if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        // In fullscreen apps, do not intercept anything else
        if self.shell.in_alt_screen {
            if let Some(bytes) = key_to_bytes(&key) {
                self.shell.send(&bytes)?;
            }
            return Ok(());
        }

        // Toggle hidden
        if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_hidden()?;
            return Ok(());
        }

        // Refresh HUD
        if key.code == KeyCode::F(5) {
            self.refresh_hud()?;
            return Ok(());
        }

        // Forward everything else
        if let Some(bytes) = key_to_bytes(&key) {
            self.shell.send(&bytes)?;
        }

        Ok(())
    }
}

/* ----------------------------- Main loop ----------------------------- */

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> anyhow::Result<()> {
    let tick = Duration::from_millis(16);

    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.should_quit {
            break;
        }

        if event::poll(tick)? {
            let ev = event::read()?;
            if !matches!(ev, Event::Resize(_, _)) {
                app.handle_event(ev)?;
            }
        }

        app.tick()?;
    }

    Ok(())
}

/* ----------------------------- UI ----------------------------- */

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();

    if app.shell.in_alt_screen {
        let shell_block = Block::default().title("Shell").borders(Borders::ALL);
        let inner = shell_block.inner(area);
        f.render_widget(shell_block, area);

        app.shell.set_size(inner.height, inner.width);

        let contents = app.shell.parser.screen().contents();
        f.render_widget(Paragraph::new(contents), inner);

        let (crow, ccol) = app.shell.parser.screen().cursor_position();
        let x = inner.x.saturating_add(ccol.min(inner.width.saturating_sub(1)));
        let y = inner.y.saturating_add(crow.min(inner.height.saturating_sub(1)));
        f.set_cursor_position((x, y));
        return;
    }

    let header_h = 2u16;
    let footer_h = 1u16;

    let tree_h = (area.height.saturating_mul(3) / 7)
        .max(8)
        .min(area.height.saturating_sub(8));

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(tree_h),
            Constraint::Min(5),
            Constraint::Length(footer_h),
        ])
        .split(area);

    let header = vec![
        Line::from(vec![Span::styled(
            "HAWK",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![
            Span::styled(
                "Current Directory: ",
                Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.cwd.display().to_string(), Style::default().fg(Color::White)),
        ]),
    ];
    f.render_widget(Paragraph::new(header), outer[0]);

    let tree_text = render_3col_listing(
        &app.items,
        outer[1].width.saturating_sub(2) as usize,
        outer[1].height.saturating_sub(2) as usize,
    );
    f.render_widget(
        Paragraph::new(tree_text).block(Block::default().title("Tree").borders(Borders::ALL)),
        outer[1],
    );

    let shell_block = Block::default().title("Shell").borders(Borders::ALL);
    let shell_inner = shell_block.inner(outer[2]);
    f.render_widget(shell_block, outer[2]);

    app.shell.set_size(shell_inner.height, shell_inner.width);

    let contents = app.shell.parser.screen().contents();
    f.render_widget(Paragraph::new(contents), shell_inner);

    let (crow, ccol) = app.shell.parser.screen().cursor_position();
    let x = shell_inner
        .x
        .saturating_add(ccol.min(shell_inner.width.saturating_sub(1)));
    let y = shell_inner
        .y
        .saturating_add(crow.min(shell_inner.height.saturating_sub(1)));
    f.set_cursor_position((x, y));

    let hidden = if app.show_hidden { "ON" } else { "OFF" };
    let footer = format!("Ctrl+E quit | F5 refresh | Ctrl+H hidden: {}", hidden);
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::LightYellow)),
        outer[3],
    );
}

/* ----------------------------- Listing ----------------------------- */

fn read_dir_items(dir: &Path, show_hidden: bool) -> anyhow::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let md = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(Entry {
            name,
            is_dir: md.is_dir(),
        });
    }
    Ok(out)
}

fn sort_items(items: &mut [Entry]) {
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn render_3col_listing(items: &[Entry], width: usize, height: usize) -> String {
    if items.is_empty() {
        return "[empty]".into();
    }

    let cols = 3usize;
    let col_w = (width / cols).max(12);

    let total = items.len();
    let rows = (total + cols - 1) / cols;
    let max_rows = height.min(rows);

    let mut out = String::new();

    for r in 0..max_rows {
        let mut line = String::new();
        for c in 0..cols {
            let idx = r * cols + c;
            if idx >= total {
                break;
            }

            let it = &items[idx];
            let prefix = if it.is_dir { "▸" } else { " " };
            let mut cell = format!("{prefix} {}", it.name);
            cell = truncate(&cell, col_w.saturating_sub(1));

            if cell.len() < col_w {
                cell.push_str(&" ".repeat(col_w - cell.len()));
            }
            line.push_str(&cell);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    if max_rows < rows {
        out.push_str("… (truncated)\n");
    }

    out
}

/* ----------------------------- Key forwarding ----------------------------- */

fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Char(c) => {
            if ctrl {
                let uc = c.to_ascii_uppercase() as u8;
                if (b'@'..=b'_').contains(&uc) {
                    Some(vec![uc - b'@'])
                } else {
                    None
                }
            } else if alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(c.to_string().as_bytes());
                Some(v)
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        _ => None,
    }
}

/* ----------------------------- OSC7 PWD tracking ----------------------------- */

fn last_osc7_path(bytes: &[u8]) -> Option<PathBuf> {
    let mut out: Option<PathBuf> = None;
    let mut i = 0usize;

    while i + 4 < bytes.len() {
        if bytes[i] == 0x1b
            && bytes[i + 1] == b']'
            && bytes[i + 2] == b'7'
            && bytes[i + 3] == b';'
        {
            let start = i + 4;

            let mut end = None;
            let mut j = start;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    end = Some(j);
                    break;
                }
                if j + 1 < bytes.len() && bytes[j] == 0x1b && bytes[j + 1] == b'\\' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }

            if let Some(end_idx) = end {
                if let Ok(s) = std::str::from_utf8(&bytes[start..end_idx]) {
                    if let Some(rest) = s.strip_prefix("file://") {
                        if let Some(slash) = rest.find('/') {
                            let path_part = &rest[slash..];
                            let decoded = percent_decode(path_part.as_bytes());
                            if let Ok(path_str) = String::from_utf8(decoded) {
                                out = Some(PathBuf::from(path_str));
                            }
                        }
                    }
                }
                i = end_idx + 1;
                continue;
            }
        }
        i += 1;
    }

    out
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            let h1 = from_hex(input[i + 1]);
            let h2 = from_hex(input[i + 2]);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/* ----------------------------- Shell injection (OSC7) ----------------------------- */

fn build_shell_command(
    shell_path: &str,
    shell_name: &str,
) -> anyhow::Result<(CommandBuilder, Vec<(String, String)>)> {
    if shell_name.contains("bash") {
        let rcfile = ensure_bash_rcfile()?;
        let mut cmd = CommandBuilder::new(shell_path);
        cmd.arg("--rcfile");
        cmd.arg(rcfile);
        cmd.arg("-i");
        return Ok((cmd, vec![]));
    }

    if shell_name.contains("zsh") {
        let zdotdir = ensure_zsh_zdotdir()?;
        let mut cmd = CommandBuilder::new(shell_path);
        cmd.arg("-i");
        return Ok((cmd, vec![("ZDOTDIR".into(), zdotdir.display().to_string())]));
    }

    let mut cmd = CommandBuilder::new(shell_path);
    cmd.arg("-i");
    Ok((cmd, vec![]))
}

fn ensure_bash_rcfile() -> anyhow::Result<String> {
    let dir = config_path().join("hawk");
    fs::create_dir_all(&dir)?;
    let path = dir.join("hawk.bashrc");

    let content = r#"
# --- generated by hawk ---
if [ -f "$HOME/.bashrc" ]; then
  source "$HOME/.bashrc"
fi

__hawk_osc7() {
  printf '\e]7;file://%s%s\a' "${HOSTNAME:-localhost}" "$PWD"
}

if [ -n "$PROMPT_COMMAND" ]; then
  PROMPT_COMMAND="$PROMPT_COMMAND; __hawk_osc7"
else
  PROMPT_COMMAND="__hawk_osc7"
fi
"#;

    fs::write(&path, content)?;
    Ok(path.display().to_string())
}

fn ensure_zsh_zdotdir() -> anyhow::Result<PathBuf> {
    let dir = config_path().join("hawk").join("zdotdir");
    fs::create_dir_all(&dir)?;
    let zshrc = dir.join(".zshrc");

    let content = r#"
# --- generated by hawk ---
if [ -f "$HOME/.zshrc" ]; then
  source "$HOME/.zshrc"
fi

__hawk_osc7() {
  printf '\e]7;file://%s%s\a' "${HOST:-localhost}" "$PWD"
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __hawk_osc7
"#;

    fs::write(&zshrc, content)?;
    Ok(dir)
}

fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".")
}

/* ----------------------------- Small util ----------------------------- */

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i + 1 >= max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}
