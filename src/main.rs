//! Entry point: parses the CLI and dispatches to the graph operations.
//!
//! `soundboard` with no subcommand is the GUI placeholder (the GTK4 UI lands
//! in a later milestone).

mod cli;
mod config;
mod graph;

use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::Parser;

use cli::Commands;
use config::Config;
use graph::{Graph, PulseSession};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = cli::Cli::parse();
    match cli.command {
        None => {
            println!("GUI not implemented yet");
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Up) => {
            let cfg = Config::load().context("failed to load config")?;
            let mut graph = Graph::new(PulseSession::connect("soundboard")?);
            let report = graph.reconcile(&cfg)?;
            println!("{}", report.summary());
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Down) => {
            let mut graph = Graph::new(PulseSession::connect("soundboard")?);
            let report = graph.teardown()?;
            println!("{}", report.summary());
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Doctor) => {
            let cfg = Config::load().context("failed to load config")?;
            let mut graph = Graph::new(PulseSession::connect("soundboard")?);
            let report = graph.doctor(&cfg);
            println!("{}", report.render());
            Ok(if report.all_passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cli::Cli;
    use super::cli::Commands;
    use clap::CommandFactory;
    use clap::Parser;

    #[test]
    fn cli_command_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_smoke() {
        let version = Cli::command().render_version();
        assert_eq!(
            version.trim(),
            format!("soundboard {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn no_args_means_gui_placeholder() {
        let cli = Cli::try_parse_from(["soundboard"]).expect("parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn subcommands_parse() {
        let up = Cli::try_parse_from(["soundboard", "up"]).expect("parse up");
        assert!(matches!(up.command, Some(Commands::Up)));
        let down = Cli::try_parse_from(["soundboard", "down"]).expect("parse down");
        assert!(matches!(down.command, Some(Commands::Down)));
        let doctor = Cli::try_parse_from(["soundboard", "doctor"]).expect("parse doctor");
        assert!(matches!(doctor.command, Some(Commands::Doctor)));
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["soundboard", "frobnicate"]).is_err());
    }
}
