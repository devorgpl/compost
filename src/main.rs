mod app;
mod cli;
mod config;
mod connector;
mod error;
mod interrupt;
mod server;
mod shared;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Cli::parse();
    if let Err(err) = interrupt::install() {
        eprintln!("warning: cannot handle Ctrl-C: {err:#}");
    }
    match cli::run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            let code = err
                .downcast_ref::<error::DepapError>()
                .map_or(1, error::DepapError::exit_code);
            ExitCode::from(code)
        }
    }
}
