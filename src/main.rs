mod agent;
mod checker;
mod config;
mod llm;
mod proxy;
mod run;
mod source;

use std::{path::Path, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => run::main(&args[1..]),
        Some("proxy") if args.len() == 2 => proxy::main(Path::new(&args[1])).map(|()| true),
        _ => Err(anyhow::anyhow!("usage: vsb run|proxy ...")),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
