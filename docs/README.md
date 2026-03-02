# spectra architecture

spectra is a terminal multiplexer style app.

## runtime model

- server process owns app/session state and pane PTYs continuously
- clients connect over a unix domain socket
- each attached client polls crossterm events locally and forwards input/resize to server
- attach client loop is adaptive instead of fixed 60fps polling: it drains local input events, drains server socket messages, batches terminal writes, and sleeps for 1ms only when idle
- mouse events are forwarded over IPC and can focus/resize panes when enabled
- server broadcasts rendered frames to all attached clients
- server runtime loop is adaptive instead of fixed 60fps sleeping: it continuously drains accepts/reads/queues/flushes and sleeps for 1ms only when no work occurred in an iteration
- clipboard copy for attached clients is delivered as a per-client OSC 52 control message over IPC, so copy operations target the requesting terminal (including SSH-attached clients)
- one-shot command clients send command requests over the same socket and receive a single response without entering TUI mode
- startup options (`--cwd`, `--shell`, startup command) apply only when bootstrapping a new server
- interactive attach-or-create launch is blocked when `$SPECTRA` is already set, printing `sessions should be nested with care, unset $SPECTRA to force`
- client hello can include `--attach session[:window[.pane]]` target selection by session id/name/alias (pane supports id `pN`/`N` or window-local index `iN`)
- command mode auto-starts the server when needed; `new-session` reuses bootstrap session when it started that server
- invalid or missing attach targets return a server error and disconnect only that client
- key mapper supports tmux-compatible defaults and config overrides
- prefix mode exits on any keypress: bound keys execute then exit (unless sticky), unbound keys pass through and exit. `window-tree` and `side-window-tree` are explicit sticky overrides that always exit to support popup/sidebar workflows.
- lock mode forwards input directly to the pane, but plain `Esc` is reserved to leave lock mode. Press `Esc` again after unlock when you need to send escape to the pane.
- peek all windows mode tiles all panes from the active session into an equal grid view; any key exits and restores the previously focused pane/window
- mouse wheel scroll moves the focused pane history viewport; pressing any key returns the focused pane to follow mode at the live cursor. ANSI foreground/background styles are preserved while viewing scrolled history, and cursor mode snapshots keep those styles too. cursor mode is available via `prefix+[`/`enter-cursor-mode` (`copy-mode` remains an alias), uses `v` to toggle anchor, `x` for linewise select/extend, `w/b/e` to set anchor while word-moving, plain cursor motions clear anchor, and `w/b/e` use gargo-like `word/other/whitespace` classes across line boundaries
- mouse drag text selection tracks absolute buffer rows (not only viewport-local rows), so anchor remains stable while wheel-scrolling and continuing drag; copy spans rows outside the currently visible viewport
- when a pane viewport is manually scrolled away from follow mode, new output no longer drifts that viewport toward the tail; the scrolled view stays pinned until follow mode is explicitly restored
- plain pass-through pane key input no longer forces an app render when UI state is unchanged (for example typing in normal mode while already following live output); render invalidation is now conditional on real UI state changes such as prefix/status transitions, mode handling, side window tree handling, or manual-scroll reset to follow mode
- runtime `source-file` reload updates keymaps and runtime toggles without restarting sessions
- app manages multiple in-process sessions and mode-specific prompts (rename and system tree popup)
- tree popup supports renaming selected session/window/pane; manual pane/window rename is an explicit override layer for display names
- pane/window display names also support automatic OSC-driven naming:
  - OSC `0`/`2` updates a pane terminal title source (empty payload clears it)
  - OSC `7` updates a pane cwd fallback source
  - pane auto-name resolves from terminal title first, then cwd fallback
  - window auto-name follows the latest pane auto-name change in that window
  - manual pane/window rename wins over auto-name until cleared
- render output now prefixes OSC `2` host-title updates from the focused pane auto-name (`OSC 0/2` first, `OSC 7` fallback); if neither source exists, spectra emits no title update
- tree popup rename (`r`) now keeps input inline inside the popup candidate list (with cursor tracked on the selected row) instead of mirroring the live rename buffer in the status line
- tree popup opens with sessions expanded and window pane lists collapsed by default; use `Right`/`Tab` on a window row to expand panes
- `prefix+e` toggles a left side window tree as app state. when active it renders as a split view: left sidebar with session window list, right workspace with panes.
- side window tree does not capture keyboard focus; normal key handling continues to target the active pane.
- side window tree highlights the currently focused window row with reverse style and a leading `>` marker (gargo-style). clicking a row in the sidebar focuses that window.
- pane backend sizing is sidebar-aware: effective pane width is terminal width minus visible sidebar width.
- backend resize in multi-client mode uses the maximum effective pane viewport across connected clients.
- resize reflows visible and scrollback content at the new width using `RowBoundary` metadata (soft-wrap rows are joined into logical lines and re-wrapped, hard newlines are preserved). this matches tmux reflow behavior. alternate screen content (vim, less, etc.) is not reflowed; it uses a naive top-left copy since fullscreen apps manage their own layout.
- session manager owns pane processes and split layout tree
- each pane runs a shell process inside a portable_pty-backed PTY
- pane shell process environment includes `SPECTRA=1` so nested interactive launches can be detected
- interactive panes use login-shell startup and add prompt hooks for bash/zsh so each prompt emits OSC `2` (title) and OSC `7` (cwd)
- pane output bytes are parsed by vte into a styled terminal grid
- optional pass-through forwards tmux-style wrapped payloads (`DCS tmux;...ST`) and OSC 8 hyperlink control sequences directly to attached client terminals; config key `[terminal].allow_passthrough` defaults to `true`
- terminal grid keeps visible content on resize and captures scrollback lines
- terminal LF is normalized to CRLF behavior for expected prompt positioning
- terminal grid handles CSI `K` erase-line modes (`0`,`1`,`2`) used by shell prompt repaint flows
- terminal grid handles CSI `J` mode `3` to clear scrollback (`\x1b[3J`)
- terminal parser handles OSC `0`/`2` title updates and OSC `7` cwd updates, sanitizes display text (UTF-8 only, control chars stripped, max 256 bytes), and emits terminal events consumed by app naming logic
- pane follow mode now clamps to the live screen origin, so clear-screen redraws (`Ctrl+L`) do not pull stale scrollback into view
- terminal responds to device queries (DSR `6n`/`?6n`, DA `c`/`>c`, XTWINOPS `18t`) so inner programs can read cursor position and terminal size
- renderer composes a full frame, keeps a back buffer, rewrites each changed row from the first changed cell through row end on incremental renders (prevents stale tail cells during scroll), hides cursor during paint, draws connected box-drawing pane dividers, always draws a status line, can draw centered system overlays, supports query-cursor and selected-row inline-cursor placement in overlays, still supports explicit full clears, and renders cursor mode directly in the focused pane from a frozen snapshot
- pane row clipping in renderer must respect Unicode display width (especially double-width CJK cells) and avoid splitting a wide glyph from its `\0` continuation cell; splitting can cause terminal auto-wrap artifacts that appear as sidebar bleed
- rendered `http://` and `https://` text is emitted as OSC 8 hyperlinks, so supporting terminals can click and open URLs in a browser; terminals without OSC 8 support still show plain text
- xdg storage writes session info, logs, layouts, and scrollback artifacts
- closed-pane write errors and process exits are treated as pane closure events instead of fatal app errors
- when the active session loses its final pane, spectra switches to another existing session if available; only the final remaining session triggers shutdown
- socket e2e tests should set isolated `XDG_RUNTIME_DIR`, `XDG_DATA_HOME`, and `XDG_CONFIG_HOME` so local user config does not affect command/key behavior

## module map

- `src/main.rs`: unix runtime bootstrap (`--server` daemon mode, interactive attach mode, and command-mode dispatch)
- `src/app.rs`: server-side app logic, key dispatch, ticking, mode handling, render snapshots
- `src/attach_target.rs`: attach target parser (`session[:window[.pane]]`) and shared target type
- `src/cli.rs`: CLI options (`--attach`, subcommand command surface, `--cwd`, `--shell`, optional startup command, hidden `--server`)
- `src/runtime/server.rs`: unix socket listener, interactive client handling, and one-shot command execution responses
- `src/runtime/client.rs`: terminal attach loop, one-shot command client, and auto-server bootstrap
- `src/ipc/protocol.rs`: socket message protocol for interactive traffic plus command request/response payloads
- `src/ipc/codec.rs`: newline-delimited JSON framing helpers
- `src/ipc/socket_path.rs`: default socket-path resolution and stale-socket handling
- `src/config.rs`: config parsing from `$XDG_CONFIG_HOME/spectra/config.toml` (keymaps, shell/mouse toggles, status format/style, hooks)
- `src/xdg.rs`: config/data directory resolution with xdg fallback rules
- `src/storage.rs`: session-info/log/layout/scrollback persistence under xdg data path
- `src/input/keymap.rs`: prefix/global key mapping, tmux-like defaults, override parsing
- `src/session/manager.rs`: pane lifecycle, split/focus/resize/swap, window/session metadata, layout snapshots
- `src/session/pty_backend.rs`: process backend for pane I/O streams
- `src/session/terminal_state.rs`: vte parser, styled cell grid, scrollback tracking
- `src/ui/window_manager.rs`: split tree, directional focus, window ordering, ratio-based pane resize
- `src/ui/render.rs`: terminal frame rendering, status line placement, and system overlay window rendering

## persistence layout

- config path: `$XDG_CONFIG_HOME/spectra/config.toml` or `~/.config/spectra/config.toml`
- sample config: `docs/config.example.toml`
- data path: `$XDG_DATA_HOME/spectra` or `~/.local/share/spectra`
- per-session artifacts:
  - `session-info.json`
  - `session.log`
  - `layouts/latest-layout.json`
  - `scrollback/pane-<id>-<timestamp>.txt`

## current limitations

- no full tmux command language or command prompt parser
- terminal emulation remains intentionally minimal for this project stage

## latency e2e benchmark

- test file: `tests/socket_latency_e2e.rs`
- command:

```bash
cargo test -p spectra --test socket_latency_e2e -- --nocapture
```

- environment knobs:
  - `SPECTRA_LATENCY_WARMUP` (default `10`)
  - `SPECTRA_LATENCY_SAMPLES` (default `80`)
- output format:
  - `LATENCY_RESULT scenario=<paste|key> samples=<n> warmup=<n> measured=<n> mean_ms=<f> p50_ms=<f> p95_ms=<f> max_ms=<f>`
- measured results on this branch (same local machine, same test harness):
  - baseline (before adaptive loops + conditional key render invalidation):
    - paste: `mean_ms=35.988 p50_ms=41.844 p95_ms=41.887 max_ms=41.895`
    - key: `mean_ms=38.125 p50_ms=42.003 p95_ms=52.075 max_ms=52.093`
  - post-change:
    - paste: `mean_ms=11.253 p50_ms=11.416 p95_ms=11.838 max_ms=11.847`
    - key: `mean_ms=11.704 p50_ms=11.704 p95_ms=11.715 max_ms=11.718`
