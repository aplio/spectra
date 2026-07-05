use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    AttachOrCreate,
    RunServer,
    RunCommand,
    ApiRequest,
    Update,
    Check,
    RemoteAttach,
    RemoteClientBridge,
    ServerHandoff,
    Integration,
}

#[derive(Debug, Clone, Subcommand)]
pub enum IntegrationAction {
    /// Install the integration for TOOL (supported: claude).
    Install {
        #[arg(value_name = "TOOL")]
        tool: String,
        /// Print the would-be changes without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a previously installed integration for TOOL (supported: claude).
    Uninstall {
        #[arg(value_name = "TOOL")]
        tool: String,
    },
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
    /// Send one JSON-RPC request to the API socket and print the result.
    Api {
        /// JSON-RPC method name, e.g. session.list, pane.read, pane.send_keys.
        #[arg(value_name = "METHOD")]
        method: String,
        /// Optional JSON params object, e.g. '{"pane_id":1,"lines":50}'.
        #[arg(value_name = "PARAMS_JSON")]
        params: Option<String>,
        /// Keep reading and printing server-pushed lines after the response
        /// (for events.subscribe) until EOF/ctrl-c.
        #[arg(long)]
        follow: bool,
    },
    /// Manage integrations with external tools (e.g. Claude Code hooks).
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Internal: relay stdio to the local client socket (remote end of --remote).
    #[command(hide = true)]
    RemoteClientBridge,
    /// Hand the running server's panes over to this binary without killing
    /// them (zero-downtime upgrade; refused while clients are attached).
    #[command(hide = true)]
    ServerHandoff {
        /// Internal: perform the takeover in this process (become the new
        /// server) instead of spawning a detached one.
        #[arg(long, hide = true)]
        foreground: bool,
    },
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "spectra",
    about = "tmux-like terminal session manager",
    version
)]
pub struct Cli {
    /// Internal flag: run only the socket server runtime.
    #[arg(long, hide = true)]
    pub server: bool,

    /// Attach to a specific target: session[:window[.pane]].
    #[arg(long, value_name = "TARGET")]
    pub attach: Option<String>,

    /// Attach to a spectra server on a remote host over ssh (experimental).
    /// HOST is `user@host`, `host`, or `ssh://user@host`.
    #[arg(long, value_name = "HOST", conflicts_with_all = ["server", "update", "check"])]
    pub remote: Option<String>,

    /// Start panes in this working directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Shell executable used when COMMAND is not provided.
    #[arg(long, value_name = "PATH")]
    pub shell: Option<String>,

    /// Check for and install the latest spectra release from GitHub.
    #[arg(long)]
    pub update: bool,

    /// Check whether a newer spectra release is available without installing it.
    #[arg(long, conflicts_with = "update")]
    pub check: bool,

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
        } else if self.check {
            CliMode::Check
        } else if matches!(self.subcommand, Some(CliCommand::RemoteClientBridge)) {
            CliMode::RemoteClientBridge
        } else if matches!(self.subcommand, Some(CliCommand::ServerHandoff { .. })) {
            CliMode::ServerHandoff
        } else if self.remote.is_some() {
            CliMode::RemoteAttach
        } else if matches!(self.subcommand, Some(CliCommand::Api { .. })) {
            CliMode::ApiRequest
        } else if matches!(self.subcommand, Some(CliCommand::Integration { .. })) {
            CliMode::Integration
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
        if self.remote.is_some() && self.subcommand.is_some() {
            return Err("--remote cannot be used with subcommands".to_string());
        }
        for (enabled, flag) in [(self.update, "--update"), (self.check, "--check")] {
            if !enabled {
                continue;
            }
            if self.server {
                return Err(format!("{flag} cannot be used with --server"));
            }
            if self.attach.is_some() {
                return Err(format!("{flag} cannot be used with --attach"));
            }
            if self.subcommand.is_some() {
                return Err(format!("{flag} cannot be used with subcommands"));
            }
            if self.cwd.is_some() || self.shell.is_some() || !self.command.is_empty() {
                return Err(format!("{flag} cannot be used with startup options"));
            }
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
    fn parses_check_flag() {
        let cli = Cli::try_parse_from(["spectra", "--check"]).expect("parse check");
        assert!(cli.check);
        assert_eq!(cli.mode(), CliMode::Check);
    }

    #[test]
    fn check_conflicts_with_update() {
        let err = Cli::try_parse_from(["spectra", "--check", "--update"])
            .expect_err("check/update conflict");
        assert!(err.to_string().contains("cannot be used with"));
    }

    #[test]
    fn rejects_check_with_startup_options() {
        let cli = Cli::try_parse_from(["spectra", "--check", "--cwd", "/tmp"])
            .expect("parse check startup option");
        assert!(cli.validate().is_err());
    }

    #[test]
    fn rejects_check_with_command_subcommand() {
        let cli = Cli::try_parse_from(["spectra", "--check", "new-session"])
            .expect("parse check with command");
        assert!(cli.validate().is_err());
    }

    #[test]
    fn version_flag_prints_version() {
        let err = Cli::try_parse_from(["spectra", "--version"]).expect_err("version exits parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
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
    fn parses_api_subcommand_with_method_only() {
        let cli = Cli::try_parse_from(["spectra", "api", "session.list"]).expect("parse api");
        match &cli.subcommand {
            Some(CliCommand::Api {
                method,
                params,
                follow,
            }) => {
                assert_eq!(method, "session.list");
                assert!(params.is_none());
                assert!(!follow);
            }
            _ => panic!("expected api subcommand"),
        }
        assert_eq!(cli.mode(), CliMode::ApiRequest);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn parses_api_subcommand_with_follow_flag() {
        let cli = Cli::try_parse_from(["spectra", "api", "--follow", "events.subscribe"])
            .expect("parse api follow");
        match &cli.subcommand {
            Some(CliCommand::Api { method, follow, .. }) => {
                assert_eq!(method, "events.subscribe");
                assert!(*follow);
            }
            _ => panic!("expected api subcommand"),
        }
        assert_eq!(cli.mode(), CliMode::ApiRequest);
    }

    #[test]
    fn parses_api_subcommand_with_params_json() {
        let cli =
            Cli::try_parse_from(["spectra", "api", "pane.read", r#"{"pane_id":1,"lines":50}"#])
                .expect("parse api with params");
        match &cli.subcommand {
            Some(CliCommand::Api { method, params, .. }) => {
                assert_eq!(method, "pane.read");
                assert_eq!(params.as_deref(), Some(r#"{"pane_id":1,"lines":50}"#));
            }
            _ => panic!("expected api subcommand"),
        }
        assert_eq!(cli.mode(), CliMode::ApiRequest);
    }

    #[test]
    fn api_subcommand_requires_method() {
        assert!(Cli::try_parse_from(["spectra", "api"]).is_err());
    }

    #[test]
    fn parses_remote_flag() {
        let cli = Cli::try_parse_from(["spectra", "--remote", "me@box"]).expect("parse remote");
        assert_eq!(cli.remote.as_deref(), Some("me@box"));
        assert_eq!(cli.mode(), CliMode::RemoteAttach);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn remote_requires_value() {
        assert!(Cli::try_parse_from(["spectra", "--remote"]).is_err());
    }

    #[test]
    fn remote_conflicts_with_server_update_and_check() {
        for flag in ["--server", "--update", "--check"] {
            let err = Cli::try_parse_from(["spectra", "--remote", "box", flag])
                .expect_err("remote conflict");
            assert!(
                err.to_string().contains("cannot be used with"),
                "unexpected error for {flag}: {err}"
            );
        }
    }

    #[test]
    fn remote_rejects_subcommands() {
        let cli = Cli::try_parse_from(["spectra", "--remote", "box", "new-session"])
            .expect("parse remote with subcommand");
        assert!(cli.validate().is_err());
    }

    #[test]
    fn remote_combines_with_attach_target() {
        let cli = Cli::try_parse_from(["spectra", "--remote", "me@box", "--attach", "dev:w2.p4"])
            .expect("parse remote with attach");
        assert_eq!(cli.remote.as_deref(), Some("me@box"));
        assert_eq!(cli.attach_target_raw(), Some("dev:w2.p4"));
        assert_eq!(cli.mode(), CliMode::RemoteAttach);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn parses_remote_client_bridge_subcommand() {
        let cli = Cli::try_parse_from(["spectra", "remote-client-bridge"]).expect("parse bridge");
        assert!(matches!(
            cli.subcommand,
            Some(CliCommand::RemoteClientBridge)
        ));
        assert_eq!(cli.mode(), CliMode::RemoteClientBridge);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn remote_client_bridge_is_hidden_from_help() {
        let err = Cli::try_parse_from(["spectra", "--help"]).expect_err("help exits parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(!err.to_string().contains("remote-client-bridge"));
    }

    #[test]
    fn parses_server_handoff_subcommand() {
        let cli = Cli::try_parse_from(["spectra", "server-handoff"]).expect("parse handoff");
        assert!(matches!(
            cli.subcommand,
            Some(CliCommand::ServerHandoff { foreground: false })
        ));
        assert_eq!(cli.mode(), CliMode::ServerHandoff);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn parses_server_handoff_foreground_flag() {
        let cli = Cli::try_parse_from(["spectra", "server-handoff", "--foreground"])
            .expect("parse handoff foreground");
        assert!(matches!(
            cli.subcommand,
            Some(CliCommand::ServerHandoff { foreground: true })
        ));
        assert_eq!(cli.mode(), CliMode::ServerHandoff);
    }

    #[test]
    fn server_handoff_is_hidden_from_help() {
        let err = Cli::try_parse_from(["spectra", "--help"]).expect_err("help exits parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(!err.to_string().contains("server-handoff"));
    }

    #[test]
    fn parses_integration_install_with_dry_run() {
        let cli = Cli::try_parse_from(["spectra", "integration", "install", "claude", "--dry-run"])
            .expect("parse integration install");
        match &cli.subcommand {
            Some(CliCommand::Integration {
                action: super::IntegrationAction::Install { tool, dry_run },
            }) => {
                assert_eq!(tool, "claude");
                assert!(*dry_run);
            }
            _ => panic!("expected integration install"),
        }
        assert_eq!(cli.mode(), CliMode::Integration);
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn parses_integration_uninstall() {
        let cli = Cli::try_parse_from(["spectra", "integration", "uninstall", "claude"])
            .expect("parse integration uninstall");
        match &cli.subcommand {
            Some(CliCommand::Integration {
                action: super::IntegrationAction::Uninstall { tool },
            }) => assert_eq!(tool, "claude"),
            _ => panic!("expected integration uninstall"),
        }
        assert_eq!(cli.mode(), CliMode::Integration);
    }

    #[test]
    fn integration_requires_action() {
        assert!(Cli::try_parse_from(["spectra", "integration"]).is_err());
        assert!(Cli::try_parse_from(["spectra", "integration", "install"]).is_err());
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
