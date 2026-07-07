# spectra

A tmux-like terminal multiplexer written in Rust.

A background server owns sessions, windows, and panes; clients attach to it over a
Unix socket, so sessions survive detach and terminal restarts. Panes run real shell
PTYs with vte-based terminal emulation, scrollback, mouse selection, a cursor
(copy) mode, a status line, and a window-tree sidebar that spans all sessions.

## Install

```bash
curl -fsSL https://github.com/aplio/spectra/raw/refs/heads/master/install.sh | sh
```

- pin a version: `SPECTRA_VERSION=v0.2.0 sh`
- custom directory: `SPECTRA_BIN_DIR=$HOME/.bin sh`
- upgrade later with `spectra --update`; then `spectra server-handoff` moves a
  running server onto the new binary without killing any pane process
  (see [docs/upgrade.md](docs/upgrade.md))

Or build from source: `cargo install --path .`

## Usage

```bash
spectra                       # attach if a server is running, otherwise create one
spectra --attach dev          # attach to a target: session[:window[.pane]]
spectra --cwd /path --shell /bin/zsh
spectra --remote user@host    # attach to a server on another machine over ssh (experimental)
```

Session management from the shell (`spectra --help` for the full list):

```bash
spectra ls
spectra new-session
spectra kill-session --target dev
spectra new-window   --target dev
spectra split-window --vertical --target dev:1
spectra send-keys    --target dev:1.2 "make test"
```

### Default keybindings

The prefix is `Ctrl+j`. Press `prefix ?` for the full searchable cheat sheet.

| Key | Action |
| --- | --- |
| `Alt+Arrow` | focus pane in direction (no prefix; crosses windows/sessions at edges) |
| `prefix \|` / `prefix "` | split vertical / horizontal |
| `prefix c` / `prefix n` | new window / new session |
| `prefix w` | tree popup (session → window → pane, `/` filters, `r` renames) |
| `prefix e` | toggle the window-tree sidebar |
| `prefix p` | command palette |
| `prefix z` | zoom active pane |
| `prefix H/J/K/L` (or `Shift+Arrow`) | swap pane in direction (split shape stays) |
| `prefix !` | break the focused pane out into a new window |
| `prefix R` | sticky resize mode (arrows/hjkl resize, Esc exits) |
| `prefix [` | cursor mode (vi-style movement, `g` chords `gg`/`ge`/`gh`/`gl`, `v`/`y` select and copy) |
| `prefix y` | copy the mouse drag selection |
| `prefix d` | detach (server keeps running) |
| `prefix x` | close focused pane (closes the window, then the session, when it's the last one) |
| `prefix u` | restore the last closed pane (kept alive for `[pane].undo_close_seconds`, default 10s) |
| `prefix q` | quit (press again within 3s to confirm) |

With `[mouse].enabled = true`, click focuses panes, dragging on dividers resizes,
and dragging over text selects (double/triple click expands word → run → line).

## Config

`~/.config/spectra/config.toml` (or `$XDG_CONFIG_HOME/spectra/config.toml`).
All keys are optional; see [docs/config.example.toml](docs/config.example.toml)
for the annotated full reference.

```toml
prefix = "C-j"

[mouse]
enabled = true

[prefix_bindings]
N = "run: notify-send 'hello from spectra'"

[global_bindings]
"C-w" = "window-tree"
```

Reload at runtime with `prefix r`. Runtime data (session logs, layouts, scrollback
dumps) lives under `~/.local/share/spectra`.

## Scripting API

A running server serves a JSON-RPC API over a second Unix socket. `spectra api`
sends one request and prints the result:

```bash
spectra api session.list
spectra api pane.read '{"pane_id":1,"lines":50}'
spectra api pane.send_keys '{"pane_id":1,"text":"ls\n"}'
spectra api pane.swap '{"direction":"left"}'
spectra api pane.move '{"pane_id":2,"to_window":1}'   # or new_window / to_session
spectra api layout.export
spectra api layout.apply '{"layout":{"type":"split","axis":"vertical","ratio_percent":70,"first":{"type":"leaf","pane_id":1},"second":{"type":"leaf","pane_id":2}}}'
spectra api layout.set_split_ratio '{"pane_id":1,"ratio":70}'
spectra api --follow events.subscribe '{"events":["agent.changed"]}'
```

On top of this sit two extension points:

- **Plugins** — a directory with a `spectra-plugin.toml` manifest (any language,
  no SDK) can react to events, run a supervised background service, and ship
  agent-detection manifests. Place them under `~/.config/spectra/plugins/<name>/`;
  `spectra api plugin.list` shows what loaded.
- **Claude Code integration** — `spectra integration install claude` wires Claude
  Code's hooks to spectra's agent-state detection, so the sidebar and status line
  show whether an agent in a pane is working, blocked, or done.
  Uninstall with `spectra integration uninstall claude`.

## Development

```bash
cargo run          # run from source (attach-or-create)
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Architecture notes and a module map live in [docs/README.md](docs/README.md).
See [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) for contribution guidelines.

Releases: bump `version` in `Cargo.toml`, tag `vX.Y.Z`, and push the tag — CI
builds Linux/macOS binaries and publishes them with checksums to a GitHub release.
