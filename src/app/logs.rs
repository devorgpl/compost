//! `compost app logs`: the output of an instance's containers, streamed from
//! `docker compose logs` on the server.

use std::fmt::Write as _;
use std::io::IsTerminal;

use anyhow::{Result, bail};

use super::instance::{self, Selector};
use super::state::{initialized_server, not_deployed, stored_compose_file};
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;

pub struct Options {
    /// Compose services; all when empty.
    pub services: Vec<String>,
    pub follow: bool,
    /// Lines per service: a number or `all`.
    pub tail: String,
    /// e.g. `10m` or `2026-10-17T12:00:00`.
    pub since: Option<String>,
    pub timestamps: bool,
}

/// `compost app logs`
pub fn logs(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
    options: &Options,
) -> Result<()> {
    check(options)?;
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let (name, prefix) = instance::resolve(&ctx, selector)?;
    let instance =
        instance::load(&ctx, &name, &prefix)?.ok_or_else(|| not_deployed(&name, &prefix))?;
    // Unreadable compose files give no services; compose then reports the name.
    let compose = stored_compose_file(&ctx, &instance)?;
    if !compose.services.is_empty() {
        for service in &options.services {
            if !compose.services.contains_key(service) {
                let known: Vec<&str> = compose.services.keys().map(String::as_str).collect();
                return Err(config_err(format!(
                    "{} has no service `{service}` (services: {})",
                    instance.app.instance(),
                    known.join(", ")
                )));
            }
        }
    }
    stream(
        connector,
        &instance::compose_command(&ctx, &instance.app),
        options,
    )
}

/// Reject values that compose would misread before connecting.
pub fn check(options: &Options) -> Result<()> {
    if options.tail != "all" && options.tail.parse::<u32>().is_err() {
        return Err(config_err(format!(
            "--tail `{}`: use a number of lines or `all`",
            options.tail
        )));
    }
    if let Some(since) = &options.since {
        let valid = !since.is_empty()
            && since
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ":.+-".contains(c));
        if !valid {
            return Err(config_err(format!(
                "--since `{since}`: use a duration like `10m` or a time like `2026-10-17T12:00:00`"
            )));
        }
    }
    Ok(())
}

/// `<compose> logs …` with the options; `tty` keeps compose's colors.
pub fn script(compose: &str, options: &Options, tty: bool) -> String {
    let mut script = format!("{compose} logs --tail {}", shell_quote(&options.tail));
    if let Some(since) = &options.since {
        let _ = write!(script, " --since {}", shell_quote(since));
    }
    if options.timestamps {
        script.push_str(" --timestamps");
    }
    if options.follow {
        script.push_str(" --follow");
    }
    if !tty {
        script.push_str(" --no-color");
    }
    for service in &options.services {
        script.push(' ');
        script.push_str(&shell_quote(service));
    }
    script
}

/// Run `docker compose logs` attached to this terminal. Ctrl-C, which ends
/// `--follow`, is a normal end.
pub fn stream(connector: &dyn Connector, compose: &str, options: &Options) -> Result<()> {
    let tty = std::io::stdout().is_terminal();
    let _expected = crate::interrupt::expect_ctrl_c();
    let status = connector.attach(&script(compose, options, tty), tty)?;
    if status != 0 && !crate::interrupt::interrupted() {
        bail!("docker compose logs failed with exit status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;

    fn options() -> Options {
        Options {
            services: Vec::new(),
            follow: false,
            tail: "100".into(),
            since: None,
            timestamps: false,
        }
    }

    #[test]
    fn builds_the_logs_command() {
        assert_eq!(script("dc", &options(), true), "dc logs --tail '100'");
        let all = Options {
            services: vec!["web".into(), "api".into()],
            follow: true,
            tail: "all".into(),
            since: Some("10m".into()),
            timestamps: true,
        };
        assert_eq!(
            script("dc", &all, false),
            "dc logs --tail 'all' --since '10m' --timestamps --follow --no-color 'web' 'api'"
        );
    }

    #[test]
    fn rejects_bad_tail_and_since() {
        check(&options()).unwrap();
        for (tail, since) in [("ten", None), ("10", Some("x;rm")), ("10", Some(""))] {
            let bad = Options {
                tail: tail.into(),
                since: since.map(str::to_string),
                ..options()
            };
            assert!(check(&bad).is_err(), "{tail} {since:?}");
        }
    }

    #[test]
    fn attach_returns_the_exit_status() {
        let local = Local { verbose: false };
        assert_eq!(local.attach("exit 3", false).unwrap(), 3);
        assert_eq!(local.attach("true", false).unwrap(), 0);
    }
}
