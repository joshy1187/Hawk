# HAWK

A lightweight terminal HUD that keeps you oriented.

HAWK runs an **interactive shell** in the bottom pane and shows a **3-column directory listing** of your current working directory in the top pane. As you `cd` around, the HUD tracks you automatically.

## Features

- **Embedded interactive shell** (PTY)
- **3-column directory HUD** for the current directory
- **Auto-updates on `cd`** (tracks `$PWD` via OSC 7)
- **Hidden files toggle** (dotfiles)

## Keybinds (Non-standard)

Shown in the footer while running:

- `Ctrl+E` — Quit HAWK
- `F5` — Refresh HUD
- `Ctrl+H` — Toggle hidden files (dotfiles)

Everything else behaves like a normal shell, including `Enter`, `Ctrl+C`, etc.

## Install (from source)

### Requirements
- Rust (stable)
- Linux or MacOS terminal environment

### Build + run

```bash
git clone <your-repo-url>
cd hawk
cargo build --release
./target/release/hawk
install locally 
cargo install --path . --force
hawk
