//! Web-server mode of the `aura` binary.

use std::ffi::OsString;

use anyhow::Result;

/// Parse `args` as web-server options and serve until shutdown.
///
/// `args` is the tail clap collects after the `webserver` subcommand; the
/// leading program path the server's parser skips is prepended here.
pub fn run(args: &[OsString]) -> Result<()> {
    let argv = std::iter::once(OsString::from("aura")).chain(args.iter().cloned());
    let args = aura_web_server::server::parse_args(argv, "aura", "aura webserver");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(aura_web_server::server::serve(args))?;
    Ok(())
}
