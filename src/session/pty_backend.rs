use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
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
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output_rx: Receiver<Vec<u8>>,
    output_channel_open: bool,
    exited: bool,
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

        let (tx, output_rx) = mpsc::channel();
        thread::spawn(move || {
            pump_reader(&mut *reader, tx);
        });

        Ok(Self {
            master: pair.master,
            writer,
            child,
            output_rx,
            output_channel_open: true,
            exited: false,
        })
    }
}

fn build_command(config: &PaneSpawnConfig) -> CommandBuilder {
    let mut command = CommandBuilder::new(&config.shell);
    command.env("SPECTRA", "1");
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
        command.arg("--login");
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
            command.env("ZDOTDIR", zdotdir);
        }
    }

    command.arg("-l");
}

fn ensure_zsh_integration_zdotdir() -> Option<PathBuf> {
    let dir = shell_integration_base_dir().join("zsh");
    std::fs::create_dir_all(&dir).ok()?;
    write_if_changed(
        &dir.join(".zshenv"),
        "if [ -r \"$HOME/.zshenv\" ]; then source \"$HOME/.zshenv\"; fi\n",
    )
    .ok()?;
    write_if_changed(
        &dir.join(".zprofile"),
        "if [ -r \"$HOME/.zprofile\" ]; then source \"$HOME/.zprofile\"; fi\n",
    )
    .ok()?;
    write_if_changed(
        &dir.join(".zlogin"),
        "if [ -r \"$HOME/.zlogin\" ]; then source \"$HOME/.zlogin\"; fi\n",
    )
    .ok()?;
    write_if_changed(
        &dir.join(".zshrc"),
        r#"if [ -r "$HOME/.zshrc" ]; then
  source "$HOME/.zshrc"
fi

if [[ -z "${_SPECTRA_TITLE_HOOK_INSTALLED:-}" ]]; then
  typeset -g _SPECTRA_TITLE_HOOK_INSTALLED=1
  _spectra_precmd() {
    print -Pn '\e]2;%~\a'
    print -Pn '\e]7;file://${HOST:-localhost}${PWD}\a'
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
        r#"if [ -r "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi

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

fn pump_reader<R: Read + ?Sized>(reader: &mut R, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

impl PaneBackend for PtyPaneBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
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
        let mut chunks = Vec::new();
        loop {
            match self.output_rx.try_recv() {
                Ok(chunk) => chunks.push(chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.output_channel_open = false;
                    break;
                }
            }
        }
        chunks
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
}

impl Drop for PtyPaneBackend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneSpawnConfig, build_command};

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

    #[test]
    fn command_mode_keeps_lc_execution() {
        let config = PaneSpawnConfig {
            shell: "/bin/zsh".to_string(),
            cwd: None,
            command: vec!["echo hi".to_string()],
            suppress_prompt_eol_marker: true,
            cols: 80,
            rows: 24,
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
        };

        let command = build_command(&config);
        assert_eq!(
            command.get_env("SPECTRA").and_then(|value| value.to_str()),
            Some("1")
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
        };

        let command = build_command(&config);
        assert!(command.get_env("ZDOTDIR").is_some());
    }

    #[test]
    fn interactive_bash_uses_rcfile_for_prompt_integration() {
        let config = PaneSpawnConfig {
            shell: "/bin/bash".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
        };

        let argv = argv(&config);
        assert_eq!(argv[0], "/bin/bash");
        assert_eq!(argv[1], "--login");
        assert_eq!(argv[2], "--rcfile");
        assert!(
            argv[3].contains("spectra-shell-integration"),
            "expected integration rcfile path, got {}",
            argv[3]
        );
        assert_eq!(argv[4], "-i");
    }
}
