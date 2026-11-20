// SPDX-License-Identifier: GPL-3.0-only

//! `aptpod`: command line client for aptpod-server.

use clap::Parser;

#[derive(Parser)]
#[command(name = "aptpod", version = aptpod_core::version(), about = "Command line client for aptpod-server")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    eprintln!(
        "aptpod {}: nothing to do yet (see --help)",
        aptpod_core::version()
    );
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }
}
