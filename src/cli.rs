use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    AttachOrCreate,
    RunServer,
    RunCommand,
    Update,
}

#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    /// Attach to an existing session target.
    AttachSession {
        #[arg(value_name = "TARGET")]
        target: Option<String>,
    },
    /// Create a new detached session.
    NewSession,
    /// List running sessions.
    Ls,
    /// Kill a target session or the current active session.
    KillSession {
        #[arg(long, value_name = "SESSION")]
        target: Option<String>,
    },
    /// Create a new window/pane in the target context.
    NewWindow {
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// Split the focused pane in the target context.
    SplitWindow {
        #[arg(long, conflicts_with = "vertical")]
        horizontal: bool,
        #[arg(long, conflicts_with = "horizontal")]
        vertical: bool,
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// Select a session by token.
    SelectSession {
        #[arg(long, value_name = "SESSION")]
        target: Option<String>,
    },
    /// Select a window number in an optional session context.
    SelectWindow {
        #[arg(value_name = "WINDOW")]
        window: usize,
        #[arg(long, value_name = "SESSION")]
        target: Option<String>,
    },
    /// Select a pane id in an optional session context.
    SelectPane {
        #[arg(value_name = "PANE")]
        pane: usize,
        #[arg(long, value_name = "SESSION")]
        target: Option<String>,
    },
    /// Send raw text bytes to panes in the selected scope.
    SendKeys {
        #[arg(long, value_name = "TARGET", conflicts_with = "all")]
        target: Option<String>,
        #[arg(long, conflicts_with = "target")]
        all: bool,
        #[arg(
            value_name = "TEXT",
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        text: Vec<String>,
    },
    /// Reload config from PATH or the default config path.
    SourceFile {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Parser)]
#[command(name = "spectra", about = "tmux-like terminal session manager")]
pub struct Cli {
    /// Internal flag: run only the socket server runtime.
    #[arg(long, hide = true)]
    pub server: bool,

    /// Attach to a specific target: session[:window[.pane]].
    #[arg(long, value_name = "TARGET")]
    pub attach: Option<String>,

    /// Start panes in this working directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Shell executable used when COMMAND is not provided.
    #[arg(long, value_name = "PATH")]
    pub shell: Option<String>,

    /// Check for and install the latest spectra release from GitHub.
    #[arg(long)]
    pub update: bool,

    /// Optional subcommand command surface.
    #[command(subcommand)]
    pub subcommand: Option<CliCommand>,

    /// Optional command to run via <shell> -lc <command>.
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

impl Cli {
    pub fn mode(&self) -> CliMode {
        if self.server {
            CliMode::RunServer
        } else if self.update {
            CliMode::Update
        } else if matches!(self.subcommand, Some(CliCommand::AttachSession { .. }))
            || self.subcommand.is_none()
        {
            CliMode::AttachOrCreate
        } else {
            CliMode::RunCommand
        }
    }

    pub fn attach_target_raw(&self) -> Option<&str> {
        if let Some(CliCommand::AttachSession { target }) = &self.subcommand {
            return target.as_deref();
        }
        self.attach.as_deref()
    }

    pub fn has_startup_options(&self) -> bool {
        self.cwd.is_some() || self.shell.is_some() || !self.command.is_empty()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.server && self.subcommand.is_some() {
            return Err("--server cannot be used with subcommands".to_string());
        }
        if self.attach.is_some() && self.subcommand.is_some() {
            return Err("--attach cannot be used with subcommands".to_string());
        }
        if self.update && self.server {
            return Err("--update cannot be used with --server".to_string());
        }
        if self.update && self.attach.is_some() {
            return Err("--update cannot be used with --attach".to_string());
        }
        if self.update && self.subcommand.is_some() {
            return Err("--update cannot be used with subcommands".to_string());
        }
        if self.update
            && (self.cwd.is_some() || self.shell.is_some() || !self.command.is_empty())
        {
            return Err("--update cannot be used with startup options".to_string());
        }
        Ok(())
    }

    pub fn without_server_flag(&self) -> Self {
        let mut next = self.clone();
        next.server = false;
        next
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, CliCommand, CliMode};
    use clap::Parser;

    #[test]
    fn parse_minimal() {
        let cli = Cli::try_parse_from(["spectra"]).expect("parse minimal");
        assert!(!cli.server);
        assert!(cli.attach.is_none());
        assert!(cli.cwd.is_none());
        assert!(cli.shell.is_none());
        assert!(!cli.update);
        assert!(cli.subcommand.is_none());
        assert!(cli.command.is_empty());
        assert_eq!(cli.mode(), CliMode::AttachOrCreate);
    }

    #[test]
    fn parse_all_fields() {
        let cli = Cli::try_parse_from([
            "spectra", "--cwd", "/tmp", "--shell", "/bin/zsh", "--", "echo", "hello",
        ])
        .expect("parse all fields");

        assert!(!cli.server);
        assert!(cli.attach.is_none());
        assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(cli.shell.as_deref(), Some("/bin/zsh"));
        assert!(cli.subcommand.is_none());
        assert_eq!(cli.command, vec!["echo", "hello"]);
    }

    #[test]
    fn parses_server_flag() {
        let cli = Cli::try_parse_from(["spectra", "--server"]).expect("parse server");
        assert!(cli.server);
        assert_eq!(cli.mode(), CliMode::RunServer);
    }

    #[test]
    fn parses_update_flag() {
        let cli = Cli::try_parse_from(["spectra", "--update"]).expect("parse update");
        assert!(cli.update);
        assert_eq!(cli.mode(), CliMode::Update);
    }

    #[test]
    fn parses_attach_target_flag() {
        let cli = Cli::try_parse_from(["spectra", "--attach", "s2:1.3"]).expect("parse attach");
        assert_eq!(cli.attach.as_deref(), Some("s2:1.3"));
        assert_eq!(cli.attach_target_raw(), Some("s2:1.3"));
        assert_eq!(cli.mode(), CliMode::AttachOrCreate);
    }

    #[test]
    fn attach_target_coexists_with_startup_options() {
        let cli = Cli::try_parse_from([
            "spectra",
            "--attach",
            "dev:w2.p4",
            "--cwd",
            "/tmp",
            "--shell",
            "/bin/bash",
            "--",
            "echo",
            "ok",
        ])
        .expect("parse attach with startup options");

        assert_eq!(cli.attach.as_deref(), Some("dev:w2.p4"));
        assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(cli.shell.as_deref(), Some("/bin/bash"));
        assert!(cli.subcommand.is_none());
        assert_eq!(cli.command, vec!["echo", "ok"]);
    }

    #[test]
    fn attach_target_requires_value() {
        assert!(Cli::try_parse_from(["spectra", "--attach"]).is_err());
    }

    #[test]
    fn parses_attach_session_subcommand_with_target() {
        let cli = Cli::try_parse_from(["spectra", "attach-session", "dev:w2.p4"])
            .expect("parse attach-session target");
        match &cli.subcommand {
            Some(CliCommand::AttachSession { target }) => {
                assert_eq!(target.as_deref(), Some("dev:w2.p4"));
            }
            _ => panic!("expected attach-session subcommand"),
        }
        assert_eq!(cli.attach_target_raw(), Some("dev:w2.p4"));
        assert_eq!(cli.mode(), CliMode::AttachOrCreate);
    }

    #[test]
    fn parses_command_subcommand() {
        let cli = Cli::try_parse_from(["spectra", "new-session"]).expect("parse new-session");
        assert!(matches!(cli.subcommand, Some(CliCommand::NewSession)));
        assert_eq!(cli.mode(), CliMode::RunCommand);
    }

    #[test]
    fn split_window_axis_flags_conflict() {
        let err = Cli::try_parse_from(["spectra", "split-window", "--horizontal", "--vertical"])
            .expect_err("split axis conflict");
        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn parses_select_window_with_target() {
        let cli = Cli::try_parse_from(["spectra", "select-window", "2", "--target", "s3"])
            .expect("parse select-window");
        match &cli.subcommand {
            Some(CliCommand::SelectWindow { window, target }) => {
                assert_eq!(*window, 2);
                assert_eq!(target.as_deref(), Some("s3"));
            }
            _ => panic!("expected select-window"),
        }
    }

    #[test]
    fn legacy_attach_conflicts_with_subcommand() {
        let cli = Cli::try_parse_from(["spectra", "--attach", "s1", "new-session"])
            .expect("parse conflicting form");
        assert!(cli.validate().is_err());
    }

    #[test]
    fn parses_send_keys_basic() {
        let cli = Cli::try_parse_from(["spectra", "send-keys", "hello"]).expect("parse send-keys");
        match &cli.subcommand {
            Some(CliCommand::SendKeys { target, all, text }) => {
                assert!(target.is_none());
                assert!(!all);
                assert_eq!(text, &vec!["hello".to_string()]);
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn parses_send_keys_with_target() {
        let cli = Cli::try_parse_from(["spectra", "send-keys", "--target", "s2:1.3", "echo", "hi"])
            .expect("parse send-keys target");
        match &cli.subcommand {
            Some(CliCommand::SendKeys { target, all, text }) => {
                assert_eq!(target.as_deref(), Some("s2:1.3"));
                assert!(!all);
                assert_eq!(text, &vec!["echo".to_string(), "hi".to_string()]);
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn parses_send_keys_with_all() {
        let cli = Cli::try_parse_from(["spectra", "send-keys", "--all", "uptime"])
            .expect("parse send-keys all");
        match &cli.subcommand {
            Some(CliCommand::SendKeys { target, all, text }) => {
                assert!(target.is_none());
                assert!(*all);
                assert_eq!(text, &vec!["uptime".to_string()]);
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn send_keys_target_conflicts_with_all() {
        let err = Cli::try_parse_from(["spectra", "send-keys", "--target", "s1", "--all", "echo"])
            .expect_err("send-keys conflict");
        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn parses_source_file_with_path() {
        let cli =
            Cli::try_parse_from(["spectra", "source-file", "/tmp/spectra.toml"]).expect("parse");
        match &cli.subcommand {
            Some(CliCommand::SourceFile { path }) => {
                assert_eq!(
                    path.as_deref(),
                    Some(std::path::Path::new("/tmp/spectra.toml"))
                );
            }
            _ => panic!("expected source-file"),
        }
    }

    #[test]
    fn parses_source_file_without_path() {
        let cli = Cli::try_parse_from(["spectra", "source-file"]).expect("parse");
        match &cli.subcommand {
            Some(CliCommand::SourceFile { path }) => {
                assert!(path.is_none());
            }
            _ => panic!("expected source-file"),
        }
    }

    #[test]
    fn rejects_update_with_startup_options() {
        let cli = Cli::try_parse_from(["spectra", "--update", "--cwd", "/tmp"])
            .expect("parse update startup option");
        assert!(cli.validate().is_err());
    }

    #[test]
    fn rejects_update_with_command_subcommand() {
        let cli = Cli::try_parse_from(["spectra", "--update", "new-session"])
            .expect("parse update with command");
        assert!(cli.validate().is_err());
    }
}
