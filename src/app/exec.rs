//! `compost app exec` and `compost app run`: a command in a service's running
//! container, or in a new one-off container with the service's settings.

use std::io::IsTerminal;

use anyhow::Result;

use super::instance::{self, Selector};
use super::logs::deployed;
use crate::connector::{Connector, shell_quote};
use crate::error::CompostError;

pub enum Mode {
    /// In the running container (`docker compose exec`).
    Exec,
    /// In a new container that is removed afterwards (`docker compose run --rm`).
    Run {
        /// Don't start the services it depends on.
        no_deps: bool,
    },
}

pub struct Options {
    pub service: String,
    pub command: Vec<String>,
    pub mode: Mode,
}

/// Run the command attached to this terminal. A terminal on the server is
/// used when stdin and stdout are terminals, so a shell works; otherwise
/// input and output can be piped. The command's exit status becomes compost's.
pub fn exec(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
    options: &Options,
) -> Result<()> {
    let (ctx, instance) = deployed(
        connector,
        server_home,
        manager_key,
        selector,
        std::slice::from_ref(&options.service),
    )?;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let compose = instance::compose_command(&ctx, &instance.app);
    // Ctrl-C belongs to the command, e.g. a shell or a long migration.
    let _expected = crate::interrupt::expect_ctrl_c();
    match connector.attach(&script(&compose, options, tty), tty)? {
        0 => Ok(()),
        status => Err(CompostError::CommandExit(u8::try_from(status).unwrap_or(1)).into()),
    }
}

fn script(compose: &str, options: &Options, tty: bool) -> String {
    let mut script = match options.mode {
        Mode::Exec => format!("{compose} exec"),
        Mode::Run { no_deps } => {
            let deps = if no_deps { " --no-deps" } else { "" };
            format!("{compose} run --rm{deps}")
        }
    };
    if !tty {
        script.push_str(" -T");
    }
    script.push(' ');
    script.push_str(&shell_quote(&options.service));
    for arg in &options.command {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_exec_and_run_commands() {
        let exec = Options {
            service: "backend".into(),
            command: vec!["sh".into(), "-c".into(), "echo $HOME".into()],
            mode: Mode::Exec,
        };
        assert_eq!(
            script("dc", &exec, true),
            "dc exec 'backend' 'sh' '-c' 'echo $HOME'"
        );
        assert_eq!(
            script("dc", &exec, false),
            "dc exec -T 'backend' 'sh' '-c' 'echo $HOME'"
        );
        let run = Options {
            mode: Mode::Run { no_deps: true },
            command: vec!["python".into(), "migrate.py".into()],
            ..exec
        };
        assert_eq!(
            script("dc", &run, false),
            "dc run --rm --no-deps -T 'backend' 'python' 'migrate.py'"
        );
    }
}
