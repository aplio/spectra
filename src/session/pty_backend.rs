use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::session::pane::PaneBackend;

#[derive(Debug, Clone)]
pub struct PaneSpawnConfig {
    pub shell: String,
    pub cwd: Option<PathBuf>,
    pub command: Vec<String>,
    pub suppress_prompt_eol_marker: bool,
    pub cols: u16,
    pub rows: u16,
    /// Pane id assigned by the session manager, exported as SPECTRA_PANE_ID.
    pub pane_id: usize,
    /// API-level session id (name-ordinal), exported as SPECTRA_SESSION_ID.
    pub session_id: String,
}

pub trait PaneFactory: Send + Sync {
    fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>>;
}

#[derive(Default)]
pub struct PtyPaneFactory;

impl PaneFactory for PtyPaneFactory {
    fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        let backend = PtyPaneBackend::spawn(config)?;
        Ok(Box::new(backend))
    }
}

pub struct PtyPaneBackend {
    master: Box<dyn MasterPty + Send>,
    /// `None` after a handoff disarm: the portable-pty writer sends
    /// `\n`+VEOF (ctrl-d) into the PTY when dropped, which would make the
    /// pane program read EOF and exit right as the old server shuts down —
    /// so the disarm path leaks the writer instead of dropping it.
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    output_pipe: Arc<OutputPipe>,
    output_channel_open: bool,
    exited: bool,
    /// Cleared during a live server handoff so process exit leaves the
    /// child running for the successor server.
    kill_child_on_drop: bool,
}

impl PtyPaneBackend {
    fn spawn(config: &PaneSpawnConfig) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: config.rows.max(1),
                cols: config.cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(map_pty_error)?;

        let command = build_command(config);
        let child = pair.slave.spawn_command(command).map_err(map_pty_error)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(map_pty_error)?;
        let writer = pair.master.take_writer().map_err(map_pty_error)?;

        let output_pipe = OutputPipe::new();
        let pipe = Arc::clone(&output_pipe);
        thread::spawn(move || {
            pump_reader(&mut *reader, &pipe);
        });

        Ok(Self {
            master: pair.master,
            writer: Some(writer),
            child,
            output_pipe,
            output_channel_open: true,
            exited: false,
            kill_child_on_drop: true,
        })
    }
}

fn build_command(config: &PaneSpawnConfig) -> CommandBuilder {
    let mut command = CommandBuilder::new(&config.shell);
    // Panes talk to spectra's own emulator (xterm-compatible with 24-bit
    // color), so TERM must describe it rather than be inherited: when the
    // server is auto-spawned without a TTY (e.g. via `--remote` over
    // `ssh -T`), TERM is unset and pagers like less warn "terminal is not
    // fully functional".
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    // SPECTRA is the nested-session detection marker; the SPECTRA_* triple
    // lets programs inside the pane (e.g. the Claude Code hook script) send
    // semantic state to the JSON-RPC API socket via `agent.report`.
    command.env("SPECTRA", "1");
    command.env("SPECTRA_PANE_ID", config.pane_id.to_string());
    command.env("SPECTRA_SESSION_ID", &config.session_id);
    #[cfg(unix)]
    command.env(
        "SPECTRA_API_SOCKET",
        crate::ipc::socket_path::api_socket_path(),
    );
    if config.command.is_empty() {
        configure_interactive_shell(&mut command, config);
    } else {
        command.arg("-lc");
        command.arg(config.command.join(" "));
    }
    if let Some(cwd) = &config.cwd {
        command.cwd(cwd);
    }
    command
}

fn configure_interactive_shell(command: &mut CommandBuilder, config: &PaneSpawnConfig) {
    if is_bash_shell(&config.shell)
        && let Some(rcfile) = ensure_bash_integration_rcfile()
    {
        // Deliberately not --login: a login bash reads only the profile
        // files and silently ignores --rcfile (see INVOCATION in bash(1)),
        // which would drop the prompt integration entirely. The rcfile
        // replays the login startup sequence itself instead.
        command.arg("--rcfile");
        command.arg(rcfile);
        command.arg("-i");
        return;
    }

    if is_zsh_shell(&config.shell) {
        if config.suppress_prompt_eol_marker {
            command.env("PROMPT_EOL_MARK", "");
            command.arg("+o");
            command.arg("prompt_sp");
        }
        if let Some(zdotdir) = ensure_zsh_integration_zdotdir() {
            // The integration .zshenv restores this before zsh loads the
            // remaining startup files, so users who keep their config under
            // a custom ZDOTDIR still get it.
            if let Some(original) = std::env::var_os("ZDOTDIR") {
                command.env("SPECTRA_ZSH_ZDOTDIR", original);
            }
            command.env("ZDOTDIR", zdotdir);
        }
    }

    command.arg("-l");
}

fn ensure_zsh_integration_zdotdir() -> Option<PathBuf> {
    let dir = shell_integration_base_dir().join("zsh");
    std::fs::create_dir_all(&dir).ok()?;
    // Earlier versions shimmed every startup file with $HOME hardcoded,
    // which lost the config of users keeping theirs under a custom ZDOTDIR.
    // .zshenv now restores the real ZDOTDIR so zsh loads the remaining
    // startup files from the right place itself; drop the stale shims so
    // nothing shadows that.
    for stale in [".zprofile", ".zshrc", ".zlogin"] {
        let _ = std::fs::remove_file(dir.join(stale));
    }
    write_if_changed(
        &dir.join(".zshenv"),
        r#"# Spectra points ZDOTDIR here so this file runs first. Restore the
# user's ZDOTDIR immediately: zsh resolves each remaining startup file
# (.zprofile/.zshrc/.zlogin) against $ZDOTDIR at load time, so after the
# restore they come from the user's real location.
if [[ -n "${SPECTRA_ZSH_ZDOTDIR+X}" ]]; then
  ZDOTDIR="$SPECTRA_ZSH_ZDOTDIR"
  unset SPECTRA_ZSH_ZDOTDIR
else
  unset ZDOTDIR
fi
if [ -r "${ZDOTDIR:-$HOME}/.zshenv" ]; then
  source "${ZDOTDIR:-$HOME}/.zshenv"
fi

if [[ -o interactive && -z "${_SPECTRA_TITLE_HOOK_INSTALLED:-}" ]]; then
  typeset -g _SPECTRA_TITLE_HOOK_INSTALLED=1
  _spectra_precmd() {
    print -Pn '\e]2;%~\a'
    # printf, not `print -P`: prompt expansion only substitutes ${PWD}
    # under PROMPT_SUBST, and would mangle paths containing `%`.
    printf '\033]7;file://%s%s\007' "${HOST:-localhost}" "$PWD"
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd _spectra_precmd
fi
"#,
    )
    .ok()?;
    Some(dir)
}

fn ensure_bash_integration_rcfile() -> Option<PathBuf> {
    let dir = shell_integration_base_dir().join("bash");
    std::fs::create_dir_all(&dir).ok()?;
    let rcfile = dir.join("bashrc");
    write_if_changed(
        &rcfile,
        r#"# Sourced by an interactive non-login bash (--rcfile). The pane is meant
# to behave like a login shell, but bash ignores --rcfile when started with
# --login, so the login startup sequence (see INVOCATION in bash(1)) is
# replayed here instead, before the prompt hook is installed.
if [ -r /etc/profile ]; then
  . /etc/profile
fi
for _spectra_profile in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
  if [ -r "$_spectra_profile" ]; then
    . "$_spectra_profile"
    break
  fi
done
unset _spectra_profile

if [ -z "${_SPECTRA_TITLE_HOOK_INSTALLED:-}" ]; then
  _SPECTRA_TITLE_HOOK_INSTALLED=1
  __spectra_prompt_command() {
    local spectra_title="${PWD/#$HOME/~}"
    printf '\033]2;%s\007' "$spectra_title"
    printf '\033]7;file://%s%s\007' "${HOSTNAME:-localhost}" "$PWD"
  }
  if [ -n "${PROMPT_COMMAND:-}" ]; then
    PROMPT_COMMAND="__spectra_prompt_command;${PROMPT_COMMAND}"
  else
    PROMPT_COMMAND="__spectra_prompt_command"
  fi
fi
"#,
    )
    .ok()?;
    Some(rcfile)
}

fn shell_integration_base_dir() -> PathBuf {
    std::env::temp_dir().join("spectra-shell-integration")
}

fn write_if_changed(path: &Path, contents: &str) -> io::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    std::fs::write(path, contents)
}

fn is_bash_shell(path: &str) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case("bash"))
        .unwrap_or(false)
}

fn is_zsh_shell(path: &str) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case("zsh"))
        .unwrap_or(false)
}

fn map_pty_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

/// Wake the server event loop (if this process runs one) so freshly queued
/// PTY output is consumed immediately instead of on the next poll timeout.
fn notify_server_loop() {
    #[cfg(unix)]
    crate::runtime::wake::notify();
}

/// Ceiling on bytes buffered between a pane's reader thread and the server
/// loop. Once reached the reader blocks, the kernel pty queue fills, and
/// the guest's writes stall — flow control reaches the child instead of
/// this process growing without bound.
const PENDING_CAP_BYTES: usize = 1024 * 1024;

/// Coalescing byte pipe between a pane's reader thread (producer) and the
/// server loop (consumer). Reads accumulate into one pending buffer, so the
/// consumer pays its per-poll costs per batch instead of per kernel read,
/// and the loop is woken only when the buffer transitions empty→non-empty
/// instead of once per read. Replaces an unbounded mpsc of per-read `Vec`s
/// (one allocation and one wakeup per ≤8 KiB read, no backpressure).
pub(crate) struct OutputPipe {
    state: Mutex<PipeState>,
    drained: Condvar,
}

struct PipeState {
    pending: Vec<u8>,
    producer_done: bool,
    consumer_gone: bool,
}

/// Result of one consumer poll of an [`OutputPipe`].
pub(crate) enum PipePoll {
    Data(Vec<u8>),
    Empty,
    /// The reader thread has exited and every byte has been consumed.
    Closed,
}

impl OutputPipe {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PipeState {
                pending: Vec::new(),
                producer_done: false,
                consumer_gone: false,
            }),
            drained: Condvar::new(),
        })
    }

    /// Recover the guard from a poisoned lock: the state is plain bytes and
    /// flags, still safe to use after a panic elsewhere, and panicking here
    /// would take down whichever of the two threads survived.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, PipeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Consumer side: take everything gathered since the last poll.
    pub(crate) fn poll(&self) -> PipePoll {
        let mut state = self.lock_state();
        if state.pending.is_empty() {
            return if state.producer_done {
                PipePoll::Closed
            } else {
                PipePoll::Empty
            };
        }
        let batch = std::mem::take(&mut state.pending);
        drop(state);
        // Unblock a producer waiting on the byte cap.
        self.drained.notify_one();
        PipePoll::Data(batch)
    }

    /// Consumer side: called when the backend drops so a producer blocked
    /// on the byte cap exits instead of waiting forever.
    pub(crate) fn close_consumer(&self) {
        self.lock_state().consumer_gone = true;
        self.drained.notify_one();
    }

    /// Producer side: append one read's bytes, blocking on the byte cap.
    /// Returns false when the consumer is gone and the reader should exit.
    fn push(&self, bytes: &[u8]) -> bool {
        let mut state = self.lock_state();
        while state.pending.len() >= PENDING_CAP_BYTES {
            if state.consumer_gone {
                return false;
            }
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.consumer_gone {
            return false;
        }
        let was_empty = state.pending.is_empty();
        if was_empty && state.pending.capacity() == 0 {
            // mem::take in poll() hands the whole buffer to the consumer;
            // reserve a fresh batch up front instead of growing through
            // several reallocations per batch.
            state.pending.reserve(64 * 1024);
        }
        state.pending.extend_from_slice(bytes);
        drop(state);
        if was_empty {
            notify_server_loop();
        }
        true
    }

    fn finish_producer(&self) {
        self.lock_state().producer_done = true;
        // EOF or read error: wake the loop so pane cleanup runs promptly.
        notify_server_loop();
    }
}

pub(crate) fn pump_reader<R: Read + ?Sized>(reader: &mut R, pipe: &OutputPipe) {
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if !pipe.push(&buf[..n]) {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    pipe.finish_producer();
}

impl PaneBackend for PtyPaneBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pane writer detached for server handoff",
            ));
        };
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(map_pty_error)
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        match self.output_pipe.poll() {
            PipePoll::Data(batch) => vec![batch],
            PipePoll::Empty => Vec::new(),
            PipePoll::Closed => {
                self.output_channel_open = false;
                Vec::new()
            }
        }
    }

    fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    fn is_closed(&mut self) -> bool {
        if self.exited {
            return true;
        }
        let child_exited = matches!(self.child.try_wait(), Ok(Some(_status)));
        if child_exited {
            self.exited = true;
            return true;
        }
        if !self.output_channel_open {
            self.exited = true;
            return true;
        }
        false
    }

    #[cfg(unix)]
    fn handoff_master_fd(&self) -> Option<std::os::fd::RawFd> {
        self.master.as_raw_fd()
    }

    fn disarm_child_kill(&mut self) {
        self.kill_child_on_drop = false;
        // Leak the writer: dropping it would send `\n`+VEOF into the PTY
        // and terminate the very child the handoff is keeping alive. The
        // fd is reclaimed when this (exiting) process closes its fd table.
        if let Some(writer) = self.writer.take() {
            std::mem::forget(writer);
        }
    }
}

impl Drop for PtyPaneBackend {
    fn drop(&mut self) {
        // Unblock a reader thread waiting on the pipe's byte cap; killing
        // the child only unblocks one waiting in read().
        self.output_pipe.close_consumer();
        if self.kill_child_on_drop {
            let _ = self.child.kill();
            let _ = self.child.try_wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneSpawnConfig, build_command, ensure_zsh_integration_zdotdir};

    fn argv(config: &PaneSpawnConfig) -> Vec<String> {
        build_command(config)
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn interactive_shell_uses_login_mode() {
        let config = PaneSpawnConfig {
            shell: "/bin/zsh".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        assert_eq!(argv(&config), vec!["/bin/zsh", "-l"]);
    }

    #[test]
    fn interactive_zsh_can_disable_prompt_sp() {
        let config = PaneSpawnConfig {
            shell: "/bin/zsh".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: true,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        let command = build_command(&config);
        let argv = command
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(argv, vec!["/bin/zsh", "+o", "prompt_sp", "-l"]);
        assert_eq!(
            command.get_env("PROMPT_EOL_MARK").and_then(|v| v.to_str()),
            Some("")
        );
    }

    fn find_zsh() -> Option<&'static str> {
        ["/bin/zsh", "/usr/bin/zsh"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
    }

    /// The zsh hook must emit the pane's real cwd with zsh's default options,
    /// where PROMPT_SUBST is unset. It used `print -P` with a `${PWD}` payload,
    /// which prompt expansion only substitutes under PROMPT_SUBST — stock zsh
    /// setups (e.g. macOS defaults) sent the literal string `${PWD}` and the
    /// server never learned the cwd, breaking split/new-window cwd inheritance.
    #[test]
    fn zsh_integration_emits_osc7_without_prompt_subst() {
        let Some(zsh) = find_zsh() else {
            return; // no zsh on this machine (e.g. minimal CI image)
        };
        let zdotdir = ensure_zsh_integration_zdotdir().expect("zdotdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let canonical = std::fs::canonicalize(cwd.path()).expect("canonicalize cwd");

        // `-f` skips all rc files, so the integration .zshenv is sourced
        // explicitly and nothing can enable PROMPT_SUBST behind our back;
        // `-i` is required because the hook only installs in interactive
        // shells; HOME points at an empty dir so no user config is pulled in.
        let output = std::process::Command::new(zsh)
            .arg("-fic")
            .arg("source \"$ZDOTDIR/.zshenv\"; _spectra_precmd")
            .env("ZDOTDIR", &zdotdir)
            .env("HOME", home.path())
            .current_dir(&canonical)
            .output()
            .expect("run zsh");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(
            stdout.contains("\x1b]7;file://"),
            "hook should emit an OSC 7, saw: {stdout:?}"
        );
        assert!(
            stdout.contains(&format!("{}\x07", canonical.display())),
            "OSC 7 should carry the expanded cwd, saw: {stdout:?}"
        );
    }

    /// The integration .zshenv must hand back the user's ZDOTDIR (stashed in
    /// SPECTRA_ZSH_ZDOTDIR by the spawn path) before zsh loads the remaining
    /// startup files, so custom-ZDOTDIR configs keep working; without a stash
    /// it must unset the integration dir so zsh falls back to $HOME.
    #[test]
    fn zsh_integration_restores_user_zdotdir() {
        let Some(zsh) = find_zsh() else {
            return; // no zsh on this machine (e.g. minimal CI image)
        };
        let zdotdir = ensure_zsh_integration_zdotdir().expect("zdotdir");
        let home = tempfile::tempdir().expect("home tempdir");

        let run = |stash: Option<&str>| {
            let mut command = std::process::Command::new(zsh);
            command
                .arg("-fc")
                .arg("source \"$ZDOTDIR/.zshenv\"; print -r -- \"restored=[${ZDOTDIR:-}]\"")
                .env("ZDOTDIR", &zdotdir)
                .env("HOME", home.path());
            match stash {
                Some(value) => command.env("SPECTRA_ZSH_ZDOTDIR", value),
                None => command.env_remove("SPECTRA_ZSH_ZDOTDIR"),
            };
            let output = command.output().expect("run zsh");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };

        let restored = run(Some("/custom/zdotdir"));
        assert!(
            restored.contains("restored=[/custom/zdotdir]"),
            "stashed ZDOTDIR should be restored, saw: {restored:?}"
        );
        let unset = run(None);
        assert!(
            unset.contains("restored=[]"),
            "without a stash ZDOTDIR should be unset, saw: {unset:?}"
        );
    }

    /// Older binaries shimmed .zprofile/.zshrc/.zlogin with $HOME hardcoded;
    /// they must be cleaned up or they would shadow the user's real startup
    /// files now that .zshenv restores ZDOTDIR before zsh resolves them.
    #[test]
    fn zsh_integration_removes_stale_startup_shims() {
        let dir = super::shell_integration_base_dir().join("zsh");
        std::fs::create_dir_all(&dir).expect("create zsh integration dir");
        for stale in [".zprofile", ".zshrc", ".zlogin"] {
            std::fs::write(dir.join(stale), "# stale shim\n").expect("write stale shim");
        }

        let ensured = ensure_zsh_integration_zdotdir().expect("zdotdir");

        assert_eq!(ensured, dir);
        for stale in [".zprofile", ".zshrc", ".zlogin"] {
            assert!(
                !dir.join(stale).exists(),
                "stale {stale} shim should be removed"
            );
        }
        assert!(dir.join(".zshenv").exists(), ".zshenv shim should exist");
    }

    #[test]
    fn command_mode_keeps_lc_execution() {
        let config = PaneSpawnConfig {
            shell: "/bin/zsh".to_string(),
            cwd: None,
            command: vec!["echo hi".to_string()],
            suppress_prompt_eol_marker: true,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        assert_eq!(argv(&config), vec!["/bin/zsh", "-lc", "echo hi"]);
    }

    #[test]
    fn pane_command_marks_spectra_env_for_nested_detection() {
        let config = PaneSpawnConfig {
            shell: "/bin/bash".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        let command = build_command(&config);
        assert_eq!(
            command.get_env("SPECTRA").and_then(|value| value.to_str()),
            Some("1")
        );
    }

    #[test]
    fn pane_command_pins_term_to_emulator_capabilities() {
        let config = PaneSpawnConfig {
            shell: "/bin/bash".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        let command = build_command(&config);
        assert_eq!(
            command.get_env("TERM").and_then(|value| value.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            command
                .get_env("COLORTERM")
                .and_then(|value| value.to_str()),
            Some("truecolor")
        );
    }

    #[test]
    fn pane_command_exports_agent_integration_env() {
        let config = PaneSpawnConfig {
            shell: "/bin/bash".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 42,
            session_id: "dev-3".to_string(),
        };

        let command = build_command(&config);
        assert_eq!(
            command
                .get_env("SPECTRA_PANE_ID")
                .and_then(|value| value.to_str()),
            Some("42")
        );
        assert_eq!(
            command
                .get_env("SPECTRA_SESSION_ID")
                .and_then(|value| value.to_str()),
            Some("dev-3")
        );
        let socket = command
            .get_env("SPECTRA_API_SOCKET")
            .and_then(|value| value.to_str())
            .expect("SPECTRA_API_SOCKET must be exported");
        assert!(
            socket.ends_with("spectra-api.sock"),
            "unexpected socket path: {socket}"
        );
    }

    #[test]
    fn interactive_zsh_sets_zdotdir_for_shell_integration() {
        let config = PaneSpawnConfig {
            shell: "/bin/zsh".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        let command = build_command(&config);
        assert!(command.get_env("ZDOTDIR").is_some());
    }

    /// Bash must NOT be spawned with --login: a login bash reads only the
    /// profile files and silently ignores --rcfile (INVOCATION in bash(1)),
    /// so the integration rcfile — and with it the OSC 7 cwd hook — never
    /// loaded, breaking split/new-window cwd inheritance.
    #[test]
    fn interactive_bash_uses_rcfile_for_prompt_integration() {
        let config = PaneSpawnConfig {
            shell: "/bin/bash".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 7,
            session_id: "main-1".to_string(),
        };

        let argv = argv(&config);
        assert_eq!(argv[0], "/bin/bash");
        assert_eq!(argv[1], "--rcfile");
        assert!(
            argv[2].contains("spectra-shell-integration"),
            "expected integration rcfile path, got {}",
            argv[2]
        );
        assert_eq!(argv[3], "-i");
        assert!(
            !argv.contains(&"--login".to_string()),
            "--login makes bash ignore --rcfile, argv: {argv:?}"
        );
    }

    /// Spawn a real interactive bash exactly the way a pane does (`--rcfile
    /// <integration> -i`) and assert the prompt hook emits an OSC 7 carrying
    /// the actual cwd. This is the emission half the cwd-inheritance e2e
    /// tests don't cover (they emit OSC 7 by hand): it fails if --login is
    /// ever reintroduced (bash would ignore --rcfile) and if the hook stops
    /// reporting the real $PWD.
    #[test]
    fn bash_integration_emits_osc7_from_prompt_hook() {
        use std::io::Write as _;

        let Some(bash) = ["/bin/bash", "/usr/bin/bash"]
            .iter()
            .find(|path| std::path::Path::new(path).exists())
        else {
            return; // no bash on this machine (e.g. minimal CI image)
        };
        let rcfile = super::ensure_bash_integration_rcfile().expect("rcfile");
        let home = tempfile::tempdir().expect("home tempdir");
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let canonical = std::fs::canonicalize(cwd.path()).expect("canonicalize cwd");

        // HOME points at an empty dir so no user profile interferes; the
        // prompt hook's printf goes to stdout, prompts/noise go to stderr.
        let mut child = std::process::Command::new(bash)
            .arg("--rcfile")
            .arg(&rcfile)
            .arg("-i")
            .env("HOME", home.path())
            .env_remove("PROMPT_COMMAND")
            .current_dir(&canonical)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn bash");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"exit\n")
            .expect("write exit");
        let output = child.wait_with_output().expect("wait for bash");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(
            stdout.contains("\x1b]7;file://"),
            "hook should emit an OSC 7, saw: {stdout:?}"
        );
        assert!(
            stdout.contains(&format!("{}\x07", canonical.display())),
            "OSC 7 should carry the pane cwd, saw: {stdout:?}"
        );
    }
}
