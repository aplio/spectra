// Regression gate: production code must not panic via unwrap/expect.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

use clap::Parser;

#[cfg(unix)]
fn main() {
    let cli = spectra::cli::Cli::parse();
    if let Err(err) = cli.validate() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
    let mode = cli.mode();
    if matches!(
        mode,
        spectra::cli::CliMode::Update | spectra::cli::CliMode::Check
    ) {
        // Replacing the binary while a server runs is safe on Unix (the
        // update swaps the file, the running server keeps its old inode),
        // so --update is allowed with an active server; the live handoff
        // then moves the server onto the new binary without killing panes.
        let server_active =
            mode == spectra::cli::CliMode::Update && spectra::runtime::client::server_is_active();

        let command = if mode == spectra::cli::CliMode::Update {
            spectra::upgrade::UpdateCommand::Update
        } else {
            spectra::upgrade::UpdateCommand::Check
        };
        match spectra::upgrade::run(command) {
            Ok(outcome) => {
                println!("{}", outcome.message);
                if outcome.installed && server_active {
                    println!(
                        "A spectra server is still running the old binary. Run `spectra server-handoff` to switch it to the new one without killing panes (all clients must be detached)."
                    );
                }
                return;
            }
            Err(err) => {
                eprintln!("Error: {err}");
                std::process::exit(1);
            }
        }
    }
    if let Some(warning) = spectra::runtime::client::nested_session_warning(mode) {
        eprintln!("{warning}");
        std::process::exit(1);
    }
    let result = match mode {
        spectra::cli::CliMode::Update | spectra::cli::CliMode::Check => {
            unreachable!("update/check modes handled above")
        }
        spectra::cli::CliMode::RunServer => spectra::runtime::server::run(cli),
        spectra::cli::CliMode::AttachOrCreate => {
            spectra::runtime::client::run_attach_or_create(cli)
        }
        spectra::cli::CliMode::RunCommand => spectra::runtime::client::run_command(cli),
        spectra::cli::CliMode::ApiRequest => spectra::runtime::api_client::run(cli),
        spectra::cli::CliMode::RemoteAttach => spectra::runtime::remote::run(cli),
        spectra::cli::CliMode::RemoteClientBridge => spectra::runtime::remote::run_bridge(&cli),
        spectra::cli::CliMode::ServerHandoff => spectra::runtime::handoff::run(cli),
        spectra::cli::CliMode::Integration => spectra::integration::run(cli),
    };

    if let Err(err) = result {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    let cli = spectra::cli::Cli::parse();
    if let Err(err) = cli.validate() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
    eprintln!("Error: spectra socket client/server mode is currently supported on Unix only");
    std::process::exit(1);
}
