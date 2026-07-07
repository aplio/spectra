# OSC support

Status of OSC (Operating System Command) escape sequences in spectra's
guest-facing terminal emulation (`src/session/terminal_state/grid.rs`,
`osc_dispatch`), compared against ghostty as the reference terminal.
Sequences are parsed by the `vte` crate; `osc_dispatch` receives the
split parameters and whether the sequence was BEL- or ST-terminated
(query replies mirror the query's terminator).

Two directions exist and must not be confused:

- guest → spectra: sequences emitted by programs inside a pane, handled here
- spectra → host: sequences spectra emits to the attached client's terminal
  (window title OSC 2, notifications OSC 9, clipboard OSC 52, hyperlink
  re-emission OSC 8, cursor color OSC 12/112, progress OSC 9;4)

## supported (guest → spectra)

| OSC | function | notes |
|---|---|---|
| 0 / 2 | window/icon title | both treated as title; empty payload clears; feeds pane/window auto-naming |
| 4 | palette color set/query | per-pane overrides; query answers from override, else the xterm default 256-color palette |
| 7 | working directory | feeds pane cwd (new panes inherit it) |
| 8 | hyperlink | per-cell model; renderer re-emits balanced sequences, raw guest sequence never passed through |
| 9 | desktop notification (iTerm2 style) | forwarded to every attached client's host terminal as OSC 9 |
| 9;4 | progress report (ConEmu) | tracked per pane and re-emitted to attached clients' host terminals |
| 10 / 11 | default fg/bg set/query | set stores a per-pane override applied at render time to default-colored cells; query answers from the override, else colors mirrored from the attached client's host terminal |
| 12 | cursor color set/query | per-pane; the focused pane's override is applied to the host via OSC 12, restored via OSC 112 on removal/focus change; query answered only when an override is set |
| 52 | clipboard write | base64 payload broadcast to attached clients as OSC 52; query (`?`) not answered |
| 104 | palette reset | no parameters resets all overrides, otherwise the listed indices |
| 110 / 111 / 112 | reset default fg / bg / cursor color | clears the per-pane override |
| 133 | semantic prompt (shell integration) | A (prompt start), B (input start), C (output start), D (command end + exit code) tracked per pane |
| 777 | rxvt extension | `notify` only; title/body forwarded as an OSC 9 notification |

Color specs accepted by OSC 4/10/11/12: `rgb:R/G/B` with 1–4 hex digits
per channel (scaled per XParseColor) and `#RGB`/`#RRGGBB`/`#RRRRGGGGBBBB`
(4/8/16 bits per channel, most-significant bits). Named X11 colors are not
accepted; such sets are dropped.

Design notes:

- color overrides are per pane, resolved when cells are read for
  rendering, so a guest changing OSC 4/10/11 recolors only its own pane
  and detaching leaves the host terminal untouched (no repaint/restore
  problem). This differs from a plain terminal, which applies them
  globally; it is the multiplexer-correct interpretation.
- OSC 4 queries for indices the guest never set answer with the xterm
  default palette, not the host terminal's palette (which spectra cannot
  know). OSC 10/11 queries fall back to the fg/bg mirrored from the most
  recently attached client (Hello handshake), and stay unanswered when
  nothing is cached.
- OSC 9 ConEmu subcommands are distinguished from iTerm2-style
  notifications the way ghostty does it: a first argument of 1–12 selects
  the ConEmu namespace; only `9;4` (progress) is handled, the rest are
  dropped. Anything else is a notification.
- progress reports from any pane are forwarded (most recent wins), so a
  finishing command's remove (`9;4;0`) always reaches the host.
  Concurrent progress in multiple panes can interleave.

## known gaps (ghostty supports, spectra does not)

- OSC 1 (icon name): ghostty also only parses and ignores it
- OSC 5 / 105 (special colors), OSC 13–19 / 113–119 (pointer, Tektronix,
  highlight colors): niche; queries go unanswered
- OSC 21 (kitty color protocol)
- OSC 22 (mouse pointer shape)
- OSC 52 query (`?`): intentionally unanswered (clipboard read is a
  security-sensitive surface; would need an allow/prompt mechanism)
- OSC 133 P/I subcommand variants (kitty extensions)
- OSC 1337 (iTerm2): ghostty implements only `Copy=` and `CurrentDir=`;
  spectra implements neither. `CurrentDir` would be a cheap alias for the
  existing OSC 7 path if ever needed
- ConEmu OSC 9;9 (cwd report) and 9;12 (prompt mark): cheap aliases for
  OSC 7 / 133;A if ever needed
- kitty OSC 66 (text sizing) and 5522 (clipboard): ghostty parses but does
  not implement them either

## history

- 2026-07: gap analysis against ghostty; added OSC 4/104, 10/11/12 set +
  110/111/112, OSC 9 + 777 notifications, ConEmu 9;4 progress, and
  OSC 133 B/C/D on top of the previously supported 0/2, 7, 8, 10/11
  query, 52 write, 133;A.
