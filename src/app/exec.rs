//! `compost app exec` and `compost app run`: a command in a service's running
//! container, or in a new one-off container with the service's settings.
//! `compost app cmd` runs a command of the app's `commands` the same way.

use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::instance::{self, Selector};
use super::link_cmd;
use super::logs::deployed;
use crate::config::command::{self, Rendered, Template};
use crate::connector::{Connector, shell_quote};
use crate::error::{CompostError, config_err};
use crate::shared::ServerCtx;

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
    let tty = tty();
    let compose = instance::compose_command(&ctx, &instance.app);
    attach(connector, &script(&compose, options, tty, &[]), tty)
}

/// stdin and stdout are terminals, so the command gets one on the server.
pub fn tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Run a script attached to this terminal; its exit status becomes compost's.
pub fn attach(connector: &dyn Connector, script: &str, tty: bool) -> Result<()> {
    // Ctrl-C belongs to the command, e.g. a shell or a long migration.
    let _expected = crate::interrupt::expect_ctrl_c();
    match connector.attach(script, tty)? {
        0 => Ok(()),
        status => Err(CompostError::CommandExit(u8::try_from(status).unwrap_or(1)).into()),
    }
}

fn script(compose: &str, options: &Options, tty: bool, env: &[String]) -> String {
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
    script.push_str(&env_flags(env));
    script.push(' ');
    script.push_str(&shell_quote(&options.service));
    for arg in &options.command {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script
}

/// ` -e NAME` for each variable: without a value, docker takes it from the
/// environment the prelude of [`env_file`] set.
pub fn env_flags(env: &[String]) -> String {
    env.iter().map(|name| format!(" -e {name}")).collect()
}

/// Put the values of `env` in a file only the server user can read, so they
/// show up neither in the script (`-v`) nor in the process list. Returns
/// the start of the script: it exports them and deletes the file.
pub fn env_file(ctx: &ServerCtx, env: &[(String, String)]) -> Result<String> {
    if env.is_empty() {
        return Ok(String::new());
    }
    let path = ctx.path(&format!("tmp/cmd-{}.env", uuid::Uuid::new_v4().simple()));
    let contents: String = env
        .iter()
        .map(|(name, value)| format!("{name}={}\n", shell_quote(value)))
        .collect();
    ctx.connector
        .write_file(&path, contents.as_bytes(), 0o600)?;
    let path = shell_quote(&path);
    Ok(format!("set -a; . {path}; set +a; rm -f {path}; "))
}

/// What a command of `commands` runs and where, after its values are checked.
pub struct Prepared<'a> {
    pub name: &'a str,
    pub template: &'a Template,
    /// e.g. `caddy-main/crowdsec`, `exec backend`.
    pub place: String,
    pub rendered: Rendered,
    pub yes: bool,
    pub dry_run: bool,
}

impl Prepared<'_> {
    /// Show it for `--dry-run`, or ask when the command wants it. `false`:
    /// don't run it.
    pub fn go_ahead(&self) -> Result<bool> {
        let words = command::display(&self.rendered.argv);
        if self.dry_run {
            out!("Would run {} in {}:", self.name, self.place);
            out!("  {words}");
            if !self.rendered.env.is_empty() {
                let secret = self.template.secret_env();
                let names: Vec<String> = self
                    .rendered
                    .env
                    .iter()
                    .map(|(name, _)| match secret.contains(&name.as_str()) {
                        true => format!("{name} (secret)"),
                        false => name.clone(),
                    })
                    .collect();
                out!("  environment: {}", names.join(", "));
            }
            return Ok(false);
        }
        if self.template.confirm && !self.yes {
            out!("Runs in {}: {words}", self.place);
            if !super::confirm()? {
                out!("Cancelled.");
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn env_names(&self) -> Vec<String> {
        self.rendered.env.iter().map(|(n, _)| n.clone()).collect()
    }
}

/// `compost app cmd`: list the instance's commands, or run one. The
/// commands are those of the deployed config, like the containers they run in.
pub fn template(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    selector: &Selector,
    name: Option<&str>,
    words: &[String],
) -> Result<()> {
    let (ctx, instance) = deployed(connector, server_home, manager_key, selector, &[])?;
    let app = &instance.app;
    let place = |template: &Template| match (&template.exec, &template.run) {
        (Some(service), _) => service.clone(),
        (_, Some(service)) => format!("a new container of {service}"),
        _ => String::new(),
    };
    if name == Some("--history") {
        if let Some(word) = words.first() {
            return Err(config_err(format!(
                "--history takes no arguments, found `{word}`"
            )));
        }
        return history(&ctx, Some(&app.instance()), &app.instance());
    }
    let Some(name) = name else {
        if app.commands.is_empty() {
            out!(
                "{} has no commands; add them under `commands` in compost.yaml and deploy",
                app.instance()
            );
        } else {
            out!("Commands of {}:", app.instance());
            out_part!(
                "{}",
                command::list(app.commands.iter().map(|(n, t)| (n, t, place(t))))
            );
        }
        let links = link_cmd::list(&instance);
        if !links.is_empty() {
            out!("Commands of its links:");
            out_part!("{links}");
        }
        return Ok(());
    };
    if link_cmd::is_link_command(name) {
        return link_cmd::run(&ctx, &instance, name, words);
    }
    let template = app.commands.get(name).ok_or_else(|| {
        config_err(format!(
            "{} has no command `{name}` ({})",
            app.instance(),
            known(&app.commands, "compost.yaml")
        ))
    })?;
    let call = template.parse_call(name, words)?;
    if call.help {
        out!("{}", template.help(name, &place(template)));
        return Ok(());
    }
    let rendered = template.render(&call.values, &mut |key| {
        Err(config_err(format!("no secret `{key}` for app commands")))
    })?;
    let (service, mode) = match (&template.exec, &template.run) {
        (Some(service), _) => (service, Mode::Exec),
        (_, Some(service)) => (service, Mode::Run { no_deps: false }),
        _ => {
            return Err(config_err(format!(
                "command `{name}` sets neither exec nor run"
            )));
        }
    };
    let prepared = Prepared {
        name,
        template,
        place: place(template),
        rendered,
        yes: call.yes,
        dry_run: call.dry_run,
    };
    if !prepared.go_ahead()? {
        return Ok(());
    }
    let options = Options {
        service: service.clone(),
        command: prepared.rendered.argv.clone(),
        mode,
    };
    let tty = tty();
    let compose = instance::compose_command(&ctx, app);
    let prelude = env_file(&ctx, &prepared.rendered.env)?;
    let script = format!(
        "{prelude}{}",
        script(&compose, &options, tty, &prepared.env_names())
    );
    let entry = Entry::new(&prepared, Some(&app.instance()));
    attach(connector, &logged(&ctx, &script, &entry)?, tty)
}

/// Where `server cmd` and `app cmd` record each command they run.
pub const COMMANDS_LOG: &str = "logs/commands.log";

/// How many entries `--history` shows.
const HISTORY: usize = 50;

/// One line of [`COMMANDS_LOG`]. Never holds the values of `env`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub time: String,
    /// Who ran it, e.g. `cyprian@laptop`.
    pub by: String,
    /// The instance whose command it is; none for the server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub name: String,
    /// e.g. `caddy-main/crowdsec`, `backend`.
    pub place: String,
    pub words: Vec<String>,
    /// The command's exit status; the script fills it in.
    #[serde(default)]
    pub exit: i32,
}

impl Entry {
    pub fn new(prepared: &Prepared, instance: Option<&str>) -> Self {
        Self {
            time: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
            by: crate::server::lock::operator(),
            instance: instance.map(str::to_string),
            name: prepared.name.to_string(),
            place: prepared.place.clone(),
            words: prepared.rendered.argv.clone(),
            exit: 0,
        }
    }
}

/// The script, followed by a line in [`COMMANDS_LOG`] with its exit status.
/// The script's status stays the status of the whole.
pub fn logged(ctx: &ServerCtx, script: &str, entry: &Entry) -> Result<String> {
    let json = serde_json::to_string(entry)?;
    // `…,"exit":0}` without the status, which the script prints.
    let prefix = json
        .strip_suffix("0}")
        .ok_or_else(|| anyhow::anyhow!("unexpected log entry {json}"))?;
    let log = shell_quote(&ctx.path(COMMANDS_LOG));
    let dir = shell_quote(&ctx.path("logs"));
    Ok(format!(
        "{script}; status=$?; mkdir -p -- {dir} && printf '%s%s}}\\n' {} \"$status\" >> {log}; exit $status",
        shell_quote(prefix)
    ))
}

/// `--history`: the last commands run, the server's (`instance` none) or an
/// instance's, oldest first.
pub fn history(ctx: &ServerCtx, instance: Option<&str>, whose: &str) -> Result<()> {
    let text = ctx.read(COMMANDS_LOG)?.unwrap_or_default();
    let entries: Vec<Entry> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .filter(|entry| entry.instance.as_deref() == instance)
        .collect();
    if entries.is_empty() {
        out!("No commands of {whose} were run yet");
        return Ok(());
    }
    let skipped = entries.len().saturating_sub(HISTORY);
    if skipped > 0 {
        out!(
            "The last {HISTORY} commands of {whose} ({skipped} earlier ones are in {}):",
            ctx.path(COMMANDS_LOG)
        );
    } else {
        out!("Commands of {whose}:");
    }
    let shown = &entries[skipped..];
    let width = |f: fn(&Entry) -> usize| shown.iter().map(f).max().unwrap_or(0);
    let (by, name) = (width(|e| e.by.len()), width(|e| e.name.len()));
    for entry in shown {
        out!(
            "  {}  {:<by$}  {:<name$}  exit {:<3}  {}: {}",
            entry.time,
            entry.by,
            entry.name,
            entry.exit,
            entry.place,
            command::display(&entry.words)
        );
    }
    Ok(())
}

/// The commands there are, for an error about one that isn't.
pub fn known(commands: &BTreeMap<String, Template>, file: &str) -> String {
    if commands.is_empty() {
        format!("there are none; add them under `commands` in {file} and deploy")
    } else {
        let names: Vec<&str> = commands.keys().map(String::as_str).collect();
        format!("commands: {}", names.join(", "))
    }
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
            script("dc", &exec, true, &[]),
            "dc exec 'backend' 'sh' '-c' 'echo $HOME'"
        );
        assert_eq!(
            script("dc", &exec, false, &["TOKEN".into()]),
            "dc exec -T -e TOKEN 'backend' 'sh' '-c' 'echo $HOME'"
        );
        let run = Options {
            mode: Mode::Run { no_deps: true },
            command: vec!["python".into(), "migrate.py".into()],
            ..exec
        };
        assert_eq!(
            script("dc", &run, false, &[]),
            "dc run --rm --no-deps -T 'backend' 'python' 'migrate.py'"
        );
    }
}
