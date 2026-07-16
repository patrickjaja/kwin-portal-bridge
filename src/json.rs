use std::io::Write;

use anyhow::Result;
use serde::Serialize;

pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let payload = serde_json::to_string_pretty(value)?;
    // Rust ignores SIGPIPE, so `println!` would panic when stdout is a closed
    // pipe (e.g. `kwin-portal-bridge windows | head`). Exit quietly instead.
    match writeln!(std::io::stdout(), "{payload}") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}
