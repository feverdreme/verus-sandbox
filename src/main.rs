mod agent;
mod checker;
mod config;
mod docs;
mod llm;
mod proxy;
mod run;
mod sandbox;
mod server;
mod source;
mod verus;

use anyhow::{Result, bail};
use std::{path::Path, process::ExitCode};

fn main() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ok = match args.first().map(String::as_str) {
        Some("run") => run::main(&args[1..])?,
        // Started by `run` as the agent's MCP server.
        Some("proxy") if args.len() == 2 => {
            proxy::main(Path::new(&args[1]))?;
            true
        }
        // Run inside the container.
        Some("init") => {
            server::init()?;
            true
        }
        Some("serve") => {
            server::serve()?;
            true
        }
        _ => bail!("usage: vsb run <experiment_dir> [--agent claude|codex|api] [--model M]"),
    };
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
