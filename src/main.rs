/// Like `println!`, but a closed stdout (e.g. `compost connector list | head`)
/// is ignored instead of panicking, so the command still finishes its work.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// Like `print!` followed by a flush, with the same handling as `out!`.
macro_rules! out_part {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, $($arg)*);
        let _ = stdout.flush();
    }};
}

mod app;
mod backup;
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
                .downcast_ref::<error::CompostError>()
                .map_or(1, error::CompostError::exit_code);
            ExitCode::from(code)
        }
    }
}
