#!/bin/sh
# spectra <-> Claude Code integration hook.
#
# Installed by `spectra integration install claude` and registered in
# ~/.claude/settings.json to run as `spectra-agent-state.sh <EventName>` on
# Claude Code hook events. It sends one `agent.report` JSON-RPC line to the
# spectra API socket; the reported state overrides screen-based detection
# for that pane for ~30 seconds per report.
#
# Event -> state mapping (conservative):
#   Stop                        -> idle     turn finished, waiting for the user
#   Notification                -> blocked  permission requests need the user;
#                                           "waiting for your input" idle
#                                           prompts map to idle instead
#   UserPromptSubmit, PreToolUse,
#   PostToolUse, SubagentStop   -> working  actively processing
#   SessionEnd                  -> unknown  agent gone; let detection resume
#   anything else               -> ignored
#
# Outside spectra (SPECTRA_* env unset) and on any transport failure this
# script exits 0 silently and never blocks Claude Code (~1s total budget).

[ -n "${SPECTRA_API_SOCKET:-}" ] || exit 0
[ -n "${SPECTRA_PANE_ID:-}" ] || exit 0
[ -S "$SPECTRA_API_SOCKET" ] || exit 0

# pane_id is interpolated into JSON as a number: digits only.
case "$SPECTRA_PANE_ID" in
    *[!0-9]*) exit 0 ;;
esac

event="${1:-}"
state=""
case "$event" in
    Stop)
        state="idle"
        ;;
    Notification)
        # The Notification payload (stdin JSON) distinguishes permission
        # requests from idle prompts; grep is enough, no jq dependency.
        payload="$(head -c 4096 2>/dev/null || true)"
        if printf '%s' "$payload" | grep -qi "waiting for your input"; then
            state="idle"
        else
            state="blocked"
        fi
        ;;
    UserPromptSubmit | PreToolUse | PostToolUse | SubagentStop)
        state="working"
        ;;
    SessionEnd)
        state="unknown"
        ;;
    *)
        exit 0
        ;;
esac

# session_id lands inside a JSON string: keep only clearly safe characters.
session_id="$(printf '%s' "${SPECTRA_SESSION_ID:-}" | tr -cd 'A-Za-z0-9._-')"
if [ -n "$session_id" ]; then
    request="{\"id\":1,\"method\":\"agent.report\",\"params\":{\"pane_id\":$SPECTRA_PANE_ID,\"session_id\":\"$session_id\",\"kind\":\"claude\",\"state\":\"$state\"}}"
else
    request="{\"id\":1,\"method\":\"agent.report\",\"params\":{\"pane_id\":$SPECTRA_PANE_ID,\"kind\":\"claude\",\"state\":\"$state\"}}"
fi

# Transport fallback chain: nc with unix-socket support (OpenBSD nc / ncat),
# then python3 stdlib sockets, else give up silently.
if command -v nc >/dev/null 2>&1; then
    if printf '%s\n' "$request" | nc -U -w 1 "$SPECTRA_API_SOCKET" >/dev/null 2>&1; then
        exit 0
    fi
fi
if command -v python3 >/dev/null 2>&1; then
    printf '%s\n' "$request" | python3 -c '
import socket, sys
try:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(1.0)
    sock.connect(sys.argv[1])
    sock.sendall(sys.stdin.buffer.read())
    sock.recv(4096)
except Exception:
    pass
' "$SPECTRA_API_SOCKET" >/dev/null 2>&1
fi
exit 0
