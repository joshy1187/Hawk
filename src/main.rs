use anyhow::Context;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    backend::CrosstermBackend,
    prelude::*,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    fs,
    io::{self, Read, Stdout, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

    // No mouse capture => best chance of right-click menus working.
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.hide_cursor()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
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

        let parser = vt100::Parser::new(rows.max(5), cols.max(10), 60_000);

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
        const KEEP: usize = 128;
        self.esc_tail.extend_from_slice(chunk);
        if self.esc_tail.len() > KEEP {
            let start = self.esc_tail.len() - KEEP;
            self.esc_tail = self.esc_tail[start..].to_vec();
        }
        let buf = self.esc_tail.as_slice();

        // Enter alt-screen
        if contains_seq(buf, b"\x1b[?1049h")
            || contains_seq(buf, b"\x1b[?47h")
            || contains_seq(buf, b"\x1b[?1047h")
        {
            self.in_alt_screen = true;
        }

        // Exit alt-screen
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Ctrl+C ALWAYS to shell
        if key.code == KeyCode::Char('c') && ctrl && !alt {
            self.shell.send(&[0x03])?;
            return Ok(());
        }

        // Quit Hawk
        if key.code == KeyCode::Char('e') && ctrl && !alt {
            self.should_quit = true;
            return Ok(());
        }

        // Copy/Paste without colliding with typical terminal bindings:
        // Ctrl+Alt+C => copy visible screen
        // Ctrl+Alt+V => paste clipboard into shell
        if ctrl && alt {
            match key.code {
                KeyCode::Char('c') => {
                    let txt = self.shell.parser.screen().contents();
                    clipboard_copy(&txt)?;
                    return Ok(());
                }
                KeyCode::Char('v') => {
                    let clip = clipboard_paste().unwrap_or_default();
                    if !clip.is_empty() {
                        self.shell.send(clip.as_bytes())?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }

        // In fullscreen apps, don't intercept anything else
        if self.shell.in_alt_screen {
            if let Some(bytes) = key_to_bytes(&key) {
                self.shell.send(&bytes)?;
            }
            return Ok(());
        }

        // Toggle hidden
        if key.code == KeyCode::Char('h') && ctrl && !alt {
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

        let shell_text = render_shell_colored(&app.shell);
        f.render_widget(Paragraph::new(shell_text).wrap(Wrap { trim: false }), inner);

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

    let shell_text = render_shell_colored(&app.shell);
    f.render_widget(Paragraph::new(shell_text).wrap(Wrap { trim: false }), shell_inner);

    let (crow, ccol) = app.shell.parser.screen().cursor_position();
    let x = shell_inner
        .x
        .saturating_add(ccol.min(shell_inner.width.saturating_sub(1)));
    let y = shell_inner
        .y
        .saturating_add(crow.min(shell_inner.height.saturating_sub(1)));
    f.set_cursor_position((x, y));

    let hidden = if app.show_hidden { "ON" } else { "OFF" };
    let footer = format!(
        "Ctrl+E quit | F5 refresh | Ctrl+H hidden: {} | Ctrl+Alt+C copy | Ctrl+Alt+V paste",
        hidden
    );
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::LightYellow)),
        outer[3],
    );
}

/* ----------------------------- Colored shell rendering ----------------------------- */

fn render_shell_colored(shell: &Shell) -> Text<'static> {
    let screen = shell.parser.screen();
    let rows = shell.rows as usize;
    let cols = shell.cols as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(rows);

    for r in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut run_text = String::new();
        let mut run_style = Style::default();

        for c in 0..cols {
            let cell_opt = screen.cell(r as u16, c as u16);
            let (ch, style) = if let Some(cell) = cell_opt {
                let s = cell.contents();
                let ch = s.chars().next().unwrap_or(' ');

                let mut st = Style::default()
                    .fg(map_vt100_color(cell.fgcolor()))
                    .bg(map_vt100_color(cell.bgcolor()));

                if cell.bold() {
                    st = st.add_modifier(Modifier::BOLD);
                }
                if cell.underline() {
                    st = st.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    // swap fg/bg
                    let fg = st.fg;
                    let bg = st.bg;
                    st.fg = bg;
                    st.bg = fg;
                }

                (ch, st)
            } else {
                (' ', Style::default())
            };

            if run_text.is_empty() {
                run_text.push(ch);
                run_style = style;
            } else if style == run_style {
                run_text.push(ch);
            } else {
                spans.push(Span::styled(run_text.clone(), run_style));
                run_text.clear();
                run_text.push(ch);
                run_style = style;
            }
        }

        if !run_text.is_empty() {
            spans.push(Span::styled(run_text.clone(), run_style));
        }

        lines.push(Line::from(spans));
    }

    Text::from(lines)
}

fn map_vt100_color(c: vt100::Color) -> Color {
    // vt100 0.15.x:
    // Color::Default | Color::Idx(u8) | Color::Rgb(u8,u8,u8)
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/* ----------------------------- Clipboard ----------------------------- */

fn clipboard_copy(text: &str) -> anyhow::Result<()> {
    if command_exists("wl-copy") {
        let mut child = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn wl-copy")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let _ = child.wait();
        return Ok(());
    }

    if command_exists("xclip") {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn xclip")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let _ = child.wait();
        return Ok(());
    }

    if command_exists("xsel") {
        let mut child = Command::new("xsel")
            .args(["--clipboard", "--input"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn xsel")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let _ = child.wait();
        return Ok(());
    }

    anyhow::bail!("No clipboard tool found (install wl-clipboard or xclip/xsel)")
}

fn clipboard_paste() -> anyhow::Result<String> {
    if command_exists("wl-paste") {
        let out = Command::new("wl-paste")
            .args(["-n"])
            .output()
            .context("run wl-paste")?;
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }

    if command_exists("xclip") {
        let out = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output()
            .context("run xclip")?;
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }

    if command_exists("xsel") {
        let out = Command::new("xsel")
            .args(["--clipboard", "--output"])
            .output()
            .context("run xsel")?;
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }

    anyhow::bail!("No clipboard tool found (install wl-clipboard or xclip/xsel)")
}

fn command_exists(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {} >/dev/null 2>&1", cmd))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
