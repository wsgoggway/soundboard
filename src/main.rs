//! Entry point: parses the CLI and dispatches to the graph and player
//! operations.
//!
//! `soundboard` with no subcommand launches the GTK4/libadwaita GUI: it
//! spawns the audio worker (auto reconcile + doctor at startup), shows the
//! doctor status and the virtual mic VU meter, and leaves the graph running
//! on close (`soundboard down` tears it down).
//!
//! `play` auto-reconciles the audio graph first (auto-setup): if the graph
//! cannot come up, play fails and prints the `doctor` report as a hint.
//! `stop` reconciles best-effort — it works from `/proc` alone, so an
//! unreachable pulse server only produces a warning. `list` only reads the
//! clips directory and never touches the audio session.

mod cli;
mod clips;
mod config;
mod graph;
mod hotkeys;
mod meter;
mod player;
mod scheduler;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::Parser;

use cli::Commands;
use clips::Clips;
use config::Config;
use graph::{Graph, PulseSession, PwTooling};
use player::{Player as _, PwCatPlayer};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ошибка: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Connects to the pulse server and builds the graph facade (pulse + native
/// pw-cli/pw-link tooling) with the default state path.
fn open_graph() -> Result<Graph<PulseSession, PwTooling>> {
    let session = PulseSession::connect("soundboard")?;
    let state_path = graph::default_state_path()?;
    Ok(Graph::new(session, PwTooling::system(), state_path))
}

/// Reconciles the audio graph so playback has somewhere to go; on failure
/// prints the doctor report to stderr as a hint, then returns the error.
fn ensure_graph(cfg: &Config) -> Result<()> {
    let mut graph = open_graph()?;
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
        eprintln!("предупреждение: {warning}");
    }
}

fn run() -> Result<ExitCode> {
    let cli = cli::Cli::parse();
    match cli.command {
        None => {
            ui::run()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Up) => {
            let cfg = Config::load().context("не удалось загрузить конфигурацию")?;
            let report = open_graph()?.reconcile(&cfg)?;
            println!("{}", report.summary());
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Down) => {
            let report = open_graph()?.teardown()?;
            println!("{}", report.summary());
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Doctor) => {
            let cfg = Config::load().context("не удалось загрузить конфигурацию")?;
            let report = open_graph()?.doctor(&cfg);
            println!("{}", report.render());
            Ok(if report.all_passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Some(Commands::List) => {
            let cfg = Config::load().context("не удалось загрузить конфигурацию")?;
            let clips = Clips::scan(&cfg.clips_dir);
            print_clip_warnings(&clips);
            if clips.is_empty() {
                println!("клипы не найдены в {}", cfg.clips_dir.display());
            } else {
                // Parse-friendly one-per-line output: `category/name` for
                // clips from subdirectories, bare `name` for top-level ones.
                for entry in clips.entries() {
                    if entry.category == clips::UNCATEGORIZED {
                        println!("{}", entry.name);
                    } else {
                        println!("{}/{}", entry.category, entry.name);
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Play { name }) => {
            let cfg = Config::load().context("не удалось загрузить конфигурацию")?;
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
            println!("воспроизведение: {}", clip.name);
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Hotkeys { write }) => {
            let cfg = Config::load().context("не удалось загрузить конфигурацию")?;
            let clips = Clips::scan(&cfg.clips_dir);
            print_clip_warnings(&clips);
            let names = clips.list();
            let entries = hotkeys::assign_keys(&names);
            print!(
                "{}",
                hotkeys::render_table(&entries, names.len().saturating_sub(entries.len()))
            );
            if !write {
                print!("{}", hotkeys::render_manual_steps());
                return Ok(ExitCode::SUCCESS);
            }
            if entries.is_empty() {
                eprintln!("записывать нечего: клипы не найдены");
                return Ok(ExitCode::FAILURE);
            }
            let dir = dirs::data_dir()
                .context("не удалось определить каталог данных (XDG_DATA_HOME или HOME)")?
                .join("applications");
            let files = hotkeys::build_files(&entries, &hotkeys::exec_binary());
            let outcome = hotkeys::write_desktop_files(&dir, &files)?;
            for name in &outcome.created {
                println!("создано: {name}");
            }
            for name in &outcome.updated {
                println!("обновлено: {name}");
            }
            for name in &outcome.unchanged {
                println!("без изменений: {name}");
            }
            for name in &outcome.backups {
                println!("резервная копия: {name}");
            }
            for reason in &outcome.skipped {
                eprintln!("пропущено: {reason}");
            }
            // Make a RUNNING Plasma re-read the files: the shortcut daemon
            // never reloads an existing component on its own (stale Exec —
            // see `src/ui/kglobalaccel.rs` and docs/kde-hotkeys.md), so a
            // plain kbuildsycoca6 is not enough after a rewrite.
            let names: Vec<String> = files.iter().map(|file| file.file_name.clone()).collect();
            match ui::kglobalaccel::refresh_services(&dir, &names) {
                Ok(ui::kglobalaccel::RefreshOutcome::Refreshed) => {
                    println!("Горячие клавиши активированы в Plasma (kglobalaccel).");
                }
                Ok(ui::kglobalaccel::RefreshOutcome::NoDaemon) => {
                    hotkeys::refresh_ksycoca()?;
                    println!(
                        "Активация: выполните `kbuildsycoca6`; если клавиши не появились — перелогиньтесь."
                    );
                }
                Err(e) => {
                    eprintln!(
                        "предупреждение: Plasma не перечитала хоткеи: {e:#} — \
                         перелогиньтесь, чтобы они заработали"
                    );
                }
            }
            println!(
                "Проверка: Параметры системы → Клавиатура → Комбинации клавиш → раздел «Команды»."
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Stop { name }) => {
            let cfg = Config::load().context("не удалось загрузить конфигурацию")?;
            let clip_path: Option<PathBuf> = match &name {
                Some(query) => {
                    let clips = Clips::scan(&cfg.clips_dir);
                    print_clip_warnings(&clips);
                    Some(clips.resolve(query)?.path.to_path_buf())
                }
                None => None,
            };
            // Stopping works from /proc alone, so the graph reconcile here is
            // best-effort: `soundboard stop` must also work while the pulse
            // server is down (no doctor dump — that is play's job).
            match open_graph().and_then(|mut graph| graph.reconcile(&cfg)) {
                Ok(report) => {
                    if !report.is_noop() {
                        println!("{}", report.summary());
                    }
                }
                Err(e) => {
                    eprintln!(
                        "предупреждение: граф не приведён в порядок, останавливаю всё равно: {e:#}"
                    );
                }
            }
            let stopped = PwCatPlayer::stop_external(clip_path.as_deref())?;
            match (&name, stopped) {
                (Some(clip_name), 0) => println!("{clip_name} не воспроизводится"),
                (Some(clip_name), _) => println!("остановлено: {clip_name}"),
                (None, 0) => println!("ничего не воспроизводится"),
                (None, n) => println!("остановлено: {n}"),
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
    fn no_args_means_gui() {
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

    #[test]
    fn hotkeys_parse_with_and_without_write() {
        let plain = Cli::try_parse_from(["soundboard", "hotkeys"]).expect("parse hotkeys");
        assert!(matches!(
            plain.command,
            Some(Commands::Hotkeys { write: false })
        ));
        let write =
            Cli::try_parse_from(["soundboard", "hotkeys", "--write"]).expect("parse --write");
        assert!(matches!(
            write.command,
            Some(Commands::Hotkeys { write: true })
        ));
    }
}
