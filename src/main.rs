mod checker;
mod config;
mod docs;
mod sandbox;
mod server;
mod source;
mod verus;

use anyhow::{Result, bail};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // Run inside the container.
        Some("init") => server::init(),
        Some("serve") => server::serve(),
        _ => bail!("usage: vsb init | serve"),
    }
}
