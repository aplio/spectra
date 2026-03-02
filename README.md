# spectra

spectra is a tmux-like terminal session manager written in Rust.

Current POC scope:
- pane splits and directional focus
- connected pane borders with box-drawing characters
- window cycling, selection list, swap, and resize
- socket client/server runtime with persistent sessions
- shell PTYs per pane
- terminal parsing with vte
- OSC title parsing (`OSC 0`/`OSC 2`) and cwd fallback parsing (`OSC 7`) for pane/window naming
- host terminal title sync via OSC 2 from focused pane auto-name (`OSC 0`/`OSC 2`, fallback `OSC 7`)
- interactive bash/zsh startup installs a spectra prompt hook that emits OSC 2 (title) and OSC 7 (cwd) each prompt
- LF newline normalization to CRLF-style prompt movement
- interactive panes start shells in login mode for tmux-compatible init behavior
- status line and command prompts
- XDG-based config and data persistence

## Run

```bash
cargo run
```

Default behavior is attach-or-create:
- if a spectra server is already running, the client attaches to it
- otherwise spectra starts a background server and attaches automatically

When attaching to an already-running server, startup options (`--cwd`, `--shell`, startup command)
are ignored.

P0 command surface:

```bash
cargo run -- attach-session [TARGET]
cargo run -- new-session
cargo run -- ls
cargo run -- kill-session [--target SESSION]
cargo run -- new-window [--target session[:window[.pane]]]
cargo run -- split-window [--horizontal|--vertical] [--target session[:window[.pane]]]
cargo run -- select-session [--target SESSION]
cargo run -- select-window <WINDOW> [--target SESSION]
cargo run -- select-pane <PANE> [--target SESSION]
cargo run -- send-keys [--target session[:window[.pane]] | --all] <TEXT...>
cargo run -- source-file [PATH]
```

Command mode behavior:
- command invocations auto-start the server when missing
- `new-session` is detached-only
- if `new-session` bootstraps a new server, it reuses the bootstrap session (no duplicate second session)
- `ls` prints one stable human-readable line per session
- `kill-session` on the last session shuts down the server
- `send-keys` sends raw text bytes to selected panes without changing focus

`send-keys` targeting:
- default (no selector): active focused pane
- `--target session`: all panes in that session
- `--target session:window`: all panes in that window
- `--target session:window.pane`: one pane
- `--all`: all panes in all sessions
- `--target` and `--all` are mutually exclusive
- empty payload is rejected with an explicit error

Attach directly to a target (session, window, pane):

```bash
cargo run -- --attach s1:w1.p1
cargo run -- --attach s1:w1.i1
cargo run -- --attach main:2.3
cargo run -- --attach dev
cargo run -- attach-session s1:w1.p1
```

Attach target grammar:
- `session`
- `session:window`
- `session:window.pane`

Session selector supports:
- exact runtime `session_id` (for example `main-1`)
- `sN` alias (for example `s2`)
- exact session name (case-sensitive)

Window and pane segments accept bare numbers or prefixed numbers:
- window: `1` or `w1`
- pane id: `3` or `p3`
- pane index in the selected window: `i1`, `i2`, ...

Invalid or missing attach targets fail the client attach attempt with an explicit error.

Start from a specific directory:

```bash
cargo run -- --cwd /path/to/project
```

Use a specific shell:

```bash
cargo run -- --shell /bin/zsh
```

Run a startup command through the shell:

```bash
cargo run -- -- "echo ready"
```

## Config

Config file path:
- `$XDG_CONFIG_HOME/spectra/config.toml`
- fallback: `~/.config/spectra/config.toml`

Example:

```toml
prefix = "C-j"
session_name = "main"
initial_command = "echo hello"
editor = "vim"

[shell]
suppress_prompt_eol_marker = true

[mouse]
enabled = false

[terminal]
allow_passthrough = true

[status]
format = "session {session_index}/{session_count}:{session_name} | window {window_index}/{window_count} | pane {pane_index}/{pane_count} | prefix {prefix}{lock}{zoom}{sync}{mouse}{message}"
background = "#2E3440"
foreground = "#D8DEE9"

[hooks]
session_created = ""
session_killed = ""
window_created = ""
pane_split = ""
pane_closed = ""
config_reloaded = ""

[prefix_bindings]
w = "window-tree"
"C-Left" = "resize-left"
c = "new-window"
d = "detach-client"
n = "new-session"

[global_bindings]
"C-w" = "window-tree"
```

Custom action name for bindings/command palette:
- `open-current-pane-buffer-in-editor` (also accepts `open-current-pane-buffef-in-editor`)
- `focus-next-pane` / `focus-prev-pane` (also accepts `focus-previous-pane`)
- `enter-cursor-mode` / `leave-cursor-mode` (`copy-mode` remains an alias of `enter-cursor-mode`)
- `side-window-tree` / `toggle-side-window-tree`
- `next-window` / `prev-window` remain available for custom bindings and command palette usage
- if `editor` is unset or blank, spectra uses `$EDITOR`; if both are missing, it defaults to `vi`
- `[terminal].allow_passthrough` defaults to `true` and enables tmux-style DCS pass-through (`ESC Ptmux;...ESC \\`) plus direct OSC 8 hyperlink control-sequence passthrough to the outer terminal

## Data storage

Runtime data path:
- `$XDG_DATA_HOME/spectra`
- fallback: `~/.local/share/spectra`

Per session directory stores:
- `session-info.json`
- `session.log`
- `layouts/latest-layout.json`
- `scrollback/pane-<id>-<timestamp>.txt`

Global data files:
- `command-history.db` (SQLite command palette history; used to prioritize recently run commands)

## Default keybindings

- `Alt+Arrow` focus pane in direction (global, no prefix needed)
  - **macOS note**: most terminals intercept Alt+Left/Right. On Ghostty, add to your config:
    `keybind = alt+left=text:\x1b[1;3D` and `keybind = alt+right=text:\x1b[1;3C`
- `Ctrl+j` enter prefix mode
- prefix `|` split vertical
- prefix `"` split horizontal
- prefix arrows move focus
- prefix `[` enter cursor mode (frozen in-pane snapshot + scrollback selection)
- prefix `c` create new window
- prefix `n` create new session
- prefix `p` open command palette (fzf-style command filter + Enter to execute)
- prefix `r` reload config (`source-file` default path)
- prefix `z` toggle zoom for active pane in current window
- prefix `S` toggle synchronize-panes for current window
- prefix `o` next pane in focus history
- prefix `O` previous pane in focus history
- prefix `w` or prefix `Tab` open tree popup (session -> window -> pane)
- prefix `e` toggle side window tree split view on the left (window list for the current session)
- prefix `Ctrl+Arrow` resize focused pane
- prefix `{` / `}` swap window with prev/next
- prefix `(` / `)` previous/next session
- prefix `s` open tree popup
- side window tree keys: `Up`/`Down` or `k`/`j` move selection, `Enter`/`Right`/`l` focuses selected window, `Esc` closes the sidebar
- side window tree state is per window, and the current selected window row is rendered with `>` + reverse highlight
- tree popup keys: `/` enters query edit focus; in query focus use `Left`/`Right`, `Ctrl+f/b/a/e`, `Ctrl+Left/Right`, `Ctrl+w/k/u` to edit query
- tree popup keys: `Down` (or `Ctrl+n`/`Ctrl+j`) from query focus enters candidate focus at the first match; `Ctrl+p` also switches to candidate focus and moves selection up
- tree popup keys: in candidate focus, `Up`/`Down` move selection, `Left`/`Right` (or `Shift+Tab`/`Tab`) collapse-expand, `Up` on first candidate returns to query focus
- tree popup keys: `Enter` select, `r` rename selected session/window/pane, `Esc` leaves query focus first and then closes popup
- prefix `$` rename current session
- prefix `Ctrl+s` save current layout
- prefix `l` append a manual log line
- prefix `P` write focused pane scrollback
- cursor mode keys: `h/j/k/l` or arrows move (clears anchor), `w/b/e` word motions (set anchor), `0/$` start/end of line (clear anchor), `v` toggles selection anchor, `x` selects current line then extends linewise downward on repeat, `y` copies to clipboard, `Esc`/`q` leaves mode
- cursor mode `w/b/e` follows gargo-like classes (`word`, `punctuation`, `whitespace`): punctuation blocks such as `@` are stepped through distinctly and motion can cross line boundaries
- command palette `Open current pane buffer in editor` writes focused pane scrollback, opens a new window running `${editor} <scrollback-file>`, and auto-closes that window when the editor exits
- command palette action execution uses each action’s own mode transition, so selecting `session.peek_all_windows` now enters peek mode directly from the palette instead of staying in normal mode
- prefix `d` detach current client (server keeps running)
- prefix `x` close focused pane
- prefix `q` quit server/session for all attached clients
- lock mode can be entered via command palette; while locked, plain `Esc` exits lock mode (press `Esc` again to send `Esc` to the pane)
- pane auto names follow OSC title updates (`OSC 0`/`OSC 2`), with OSC 7 cwd used as fallback when no explicit title is active
- window auto names follow the latest pane auto-name change in each window
- manual pane/window rename still overrides auto naming; clearing a manual name returns to OSC-driven naming
- spectra emits OSC 2 for the focused pane auto-name so host terminal/tab title follows that value; when no OSC-derived title/cwd is available, spectra leaves the previous host title unchanged
- when pane process exits (for example `exit`) or a write hits a closed pane IO channel, spectra closes that pane, or quits if it is the last pane
- when `[mouse].enabled = true`: left-click focuses panes, and left-drag on dividers resizes panes

Status format tokens:
- `{session_index}` `{session_count}` `{session_id}` `{session_name}`
- `{window_index}` `{window_count}` `{window_id}`
- `{pane_id}` `{pane_index}` `{pane_count}`
- `{prefix}` `{lock}` `{zoom}` `{sync}` `{mouse}` `{message}`

Status style defaults:
- `[status].background = "#2E3440"`
- `[status].foreground = "#D8DEE9"`

Hook events (`[hooks]`) run via `/bin/sh -lc` with env context:
- `SPECTRA_HOOK_EVENT`
- `SPECTRA_SESSION_ID` `SPECTRA_SESSION_NAME`
- `SPECTRA_WINDOW_ID` `SPECTRA_WINDOW_NUMBER`
- `SPECTRA_PANE_ID`

## Development checks

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
