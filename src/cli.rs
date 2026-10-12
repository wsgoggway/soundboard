//! Clap definitions for the soundboard command-line interface.

use clap::Parser;
use clap::Subcommand;

/// Soundboard: plays short meme audio clips into a PipeWire virtual microphone
/// for use in Zoom and other conferencing apps on Linux.
#[derive(Parser, Debug)]
#[command(name = "soundboard", version, about, long_about = None)]
pub struct Cli {
    /// Subcommand to run; none means the GUI (not implemented yet).
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create or repair the audio graph (idempotent desired-state reconcile).
    Up,
    /// Tear down soundboard's audio modules (foreign modules survive).
    Down,
    /// Verify the audio graph state; exit code 1 when problems are found.
    Doctor,
    /// List clips found in the configured clips directory.
    List,
    /// Start playing a clip (non-blocking; auto-reconciles the graph first).
    Play {
        /// Clip name: exact, unique case-insensitive or unique prefix.
        name: String,
    },
    /// Stop playing; without NAME stops everything (auto-reconciles first).
    Stop {
        /// Clip name to stop; omit to stop all playbacks.
        name: Option<String>,
    },
}
