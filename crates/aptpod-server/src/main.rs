// SPDX-License-Identifier: GPL-3.0-only

//! `aptpod-server`: apt repository and mirror server.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(name = "aptpod-server", version = aptpod_core::version(), about = "apt repository and mirror server")]
struct Cli {
    /// Server configuration file.
    #[arg(short, long, default_value = "/etc/aptpod/server.toml")]
    config: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    eprintln!(
        "aptpod-server {}: not implemented yet (config {})",
        aptpod_core::version(),
        cli.config.display()
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }
}
