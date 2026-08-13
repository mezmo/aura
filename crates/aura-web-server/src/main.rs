//! Deprecated compatibility entry point for `aura webserver`.

use clap::Parser;

use aura_web_server::server::{ServerArgs, serve};

const DEPRECATION_NOTICE: &str = "\
warning: `aura-web-server` is deprecated and will be removed in a future release.
         Run `aura webserver` instead; it accepts the same flags and \
environment variables.";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    eprintln!("{DEPRECATION_NOTICE}");
    match serve(ServerArgs::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Display, not the `io::Error` Debug that `Termination` would print, so
        // startup failures read the same here as under `aura webserver`.
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
