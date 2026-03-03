use clap::Parser;

#[cfg(unix)]
fn main() {
    let cli = spectra::cli::Cli::parse();
    if let Err(err) = cli.validate() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
    let mode = cli.mode();
    if mode == spectra::cli::CliMode::Update {
        if spectra::runtime::client::server_is_active() {
            eprintln!("Error: --update cannot run while a spectra server is active");
            std::process::exit(1);
        }

        match spectra::upgrade::run(spectra::upgrade::UpdateCommand::Update) {
            Ok(message) => {
                println!("{message}");
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
        spectra::cli::CliMode::Update => unreachable!("update mode handled above"),
        spectra::cli::CliMode::RunServer => spectra::runtime::server::run(cli),
        spectra::cli::CliMode::AttachOrCreate => {
            spectra::runtime::client::run_attach_or_create(cli)
        }
        spectra::cli::CliMode::RunCommand => spectra::runtime::client::run_command(cli),
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
