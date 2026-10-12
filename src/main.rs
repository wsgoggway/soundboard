use clap::Parser;

/// Soundboard: plays short meme audio clips into a PipeWire virtual microphone
/// for use in Zoom and other conferencing apps on Linux.
#[derive(Parser, Debug)]
#[command(name = "soundboard", version, about, long_about = None)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_command_is_valid() {
        Cli::command().debug_assert();
    }
}
