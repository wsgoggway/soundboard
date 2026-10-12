//! Entry point: parses the CLI and dispatches to the graph and player
//! operations.
//!
//! `soundboard` with no subcommand is the GUI placeholder (the GTK4 UI lands
//! in a later milestone).
//!
//! `play`/`stop` auto-reconcile the audio graph first (auto-setup
//! everywhere): if the graph cannot come up, they fail and print the
//! `doctor` report as a hint. `list` only reads the clips directory and
//! never touches the audio session.

mod cli;
mod clips;
mod config;
mod graph;
mod player;
mod scheduler;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::Parser;

use cli::Commands;
use clips::Clips;
use config::Config;
use graph::{Graph, PulseSession};
use player::{Player as _, PwCatPlayer};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Reconciles the audio graph so playback has somewhere to go; on failure
/// prints the doctor report to stderr as a hint, then returns the error.
fn ensure_graph(cfg: &Config) -> Result<()> {
    let mut graph = Graph::new(PulseSession::connect("soundboard")?);
    match graph.reconcile(cfg) {
        Ok(report) => {
            if !report.is_noop() {
                println!("{}", report.summary());
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{}", graph.doctor(cfg).render());
            Err(e)
        }
    }
}

fn print_clip_warnings(clips: &Clips) {
    for warning in clips.warnings() {
        eprintln!("warning: {warning}");
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
        Some(Commands::List) => {
            let cfg = Config::load().context("failed to load config")?;
            let clips = Clips::scan(&cfg.clips_dir);
            print_clip_warnings(&clips);
            if clips.is_empty() {
                println!("no clips found in {}", cfg.clips_dir.display());
            } else {
                for name in clips.list() {
                    println!("{name}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Play { name }) => {
            let cfg = Config::load().context("failed to load config")?;
            let clips = Clips::scan(&cfg.clips_dir);
            print_clip_warnings(&clips);
            let clip = clips.resolve(&name)?;
            ensure_graph(&cfg)?;
            // Per-clip interrupt across CLI invocations: a second play of the
            // same clip kills the playback started by a previous process.
            // (The resident GUI in M3 does this via the scheduler instead.)
            if cfg.interrupt_mode {
                PwCatPlayer::stop_external(Some(clip.path))?;
            }
            let mut player = PwCatPlayer::new();
            player.play(clip.path, clip.name, cfg.volume)?;
            println!("playing {}", clip.name);
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Stop { name }) => {
            let cfg = Config::load().context("failed to load config")?;
            let clip_path: Option<PathBuf> = match &name {
                Some(query) => {
                    let clips = Clips::scan(&cfg.clips_dir);
                    print_clip_warnings(&clips);
                    Some(clips.resolve(query)?.path.to_path_buf())
                }
                None => None,
            };
            ensure_graph(&cfg)?;
            let stopped = PwCatPlayer::stop_external(clip_path.as_deref())?;
            match (&name, stopped) {
                (Some(clip_name), 0) => println!("{clip_name} is not playing"),
                (Some(clip_name), _) => println!("stopped {clip_name}"),
                (None, 0) => println!("nothing playing"),
                (None, n) => println!("stopped {n}"),
            }
            Ok(ExitCode::SUCCESS)
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
        for args in [
            vec!["soundboard", "up"],
            vec!["soundboard", "down"],
            vec!["soundboard", "doctor"],
            vec!["soundboard", "list"],
        ] {
            let cli = Cli::try_parse_from(args).expect("parse");
            assert!(cli.command.is_some());
        }
        let play = Cli::try_parse_from(["soundboard", "play", "airhorn"]).expect("parse play");
        assert!(matches!(play.command, Some(Commands::Play { name }) if name == "airhorn"));
        let stop_all = Cli::try_parse_from(["soundboard", "stop"]).expect("parse stop");
        assert!(matches!(
            stop_all.command,
            Some(Commands::Stop { name: None })
        ));
        let stop_one =
            Cli::try_parse_from(["soundboard", "stop", "airhorn"]).expect("parse stop name");
        assert!(
            matches!(stop_one.command, Some(Commands::Stop { name }) if name.as_deref() == Some("airhorn"))
        );
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["soundboard", "frobnicate"]).is_err());
    }

    #[test]
    fn play_without_name_is_rejected() {
        assert!(Cli::try_parse_from(["soundboard", "play"]).is_err());
    }
}
