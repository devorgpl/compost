//! Built-in commands of an instance's links, run by `compost app cmd
//! <instance> <entry>.<command>`: `import` loads a dump into a database link's
//! database as the link's user, `export` writes a dump of it.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::exec::{COMMANDS_LOG, Entry};
use super::instance::{self, Instance, PinnedLink};
use crate::connector::shell_quote;
use crate::error::config_err;
use crate::server::lock::Lock;
use crate::shared::{self, AppCtx, ServerCtx};

/// Name, arguments and description of each command.
const COMMANDS: [(&str, &str, &str); 2] = [
    (
        "import",
        "[--file <dump>] [--clean] [--yes]",
        "Load a dump (stdin without --file) as the link's user; --clean recreates the database first",
    ),
    (
        "export",
        "[--file <dump>]",
        "Write a plain SQL dump of the link's database (stdout without --file)",
    ),
];

/// `<entry>.<command>`, e.g. `main.import`; never a name from `commands`.
pub fn is_link_command(name: &str) -> bool {
    name.contains('.')
}

/// The links' commands, for the list of an instance's commands.
pub fn list(instance: &Instance) -> String {
    let rows: Vec<(String, &str, &str)> = links(instance)
        .flat_map(|link| {
            COMMANDS.iter().map(move |(name, args, description)| {
                (
                    format!("{}.{name} {args}", link.entry),
                    *description,
                    link.instance.as_str(),
                )
            })
        })
        .collect();
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(usage, description, place)| format!("  {usage:<width$}  {description}  ({place})\n"))
        .collect()
}

/// Links whose kind has these commands.
fn links(instance: &Instance) -> impl Iterator<Item = &PinnedLink> {
    instance
        .links
        .iter()
        .filter(|link| shared::find_kind(&link.kind).is_some_and(|k| k.imports()))
}

struct Options {
    file: Option<PathBuf>,
    clean: bool,
    yes: bool,
    help: bool,
}

/// Run `<entry>.<command>` with its words.
pub fn run(ctx: &ServerCtx, instance: &Instance, name: &str, words: &[String]) -> Result<()> {
    let (entry, command) = name.split_once('.').context("link commands have a `.`")?;
    // `<kind>/<entry>` when two kinds have a link of that name.
    let (kind, entry) = match entry.split_once('/') {
        Some((kind, entry)) => (Some(kind), entry),
        None => (None, entry),
    };
    let found: Vec<&PinnedLink> = links(instance)
        .filter(|l| l.entry == entry && kind.is_none_or(|k| k == l.kind))
        .collect();
    let link = match found.as_slice() {
        [link] => *link,
        [] => {
            let names: Vec<&str> = links(instance).map(|l| l.entry.as_str()).collect();
            return Err(config_err(format!(
                "{} has no database link `{entry}` (links with commands: {})",
                instance.app.instance(),
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            )));
        }
        _ => {
            return Err(config_err(format!(
                "several links are named `{entry}`; write `<kind>/{entry}.{command}`"
            )));
        }
    };
    let Some((_, args, description)) = COMMANDS.iter().find(|(n, _, _)| *n == command) else {
        let names: Vec<&str> = COMMANDS.iter().map(|(n, _, _)| *n).collect();
        return Err(config_err(format!(
            "links have no command `{command}` (commands: {})",
            names.join(", ")
        )));
    };
    let options = parse(name, command, words)?;
    if options.help {
        out!("Usage: {name} {args}");
        out!("{description}.");
        out!("Runs in {} ({} link {entry}).", link.instance, link.kind);
        return Ok(());
    }

    let kind = shared::find_kind(&link.kind).context("listed links have known kinds")?;
    let server_config = instance::server_config(ctx)?;
    let shared = link.shared_app(&server_config).with_context(|| {
        format!(
            "{} {} is no longer configured on the server",
            link.kind, link.instance
        )
    })?;
    let link_ctx = AppCtx {
        server: ctx,
        app: &instance.app,
        shared: &shared,
        peers: &[],
    };
    let source = match &options.file {
        Some(file) => file.display().to_string(),
        None if command == "import" => "stdin".to_string(),
        None => "stdout".to_string(),
    };
    let mut entry_log = Entry {
        time: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
        by: crate::server::lock::operator(),
        instance: Some(instance.app.instance()),
        name: name.to_string(),
        place: link.instance.clone(),
        words: [command.to_string()]
            .into_iter()
            .chain(options.clean.then(|| "--clean".to_string()))
            .chain([source.clone()])
            .collect(),
        exit: 0,
    };

    let result = if command == "import" {
        if !options.yes {
            out!(
                "Loads {source} into {} {entry} of {} on {}{}.",
                link.kind,
                instance.app.instance(),
                link.instance,
                if options.clean {
                    ", dropping and recreating its database first"
                } else {
                    ""
                }
            );
            if options.file.is_none() && !std::io::stdin().is_terminal() {
                return Err(config_err(
                    "the dump comes on stdin, so compost can't ask; add --yes",
                ));
            }
            if !super::confirm()? {
                out!("Cancelled.");
                return Ok(());
            }
        }
        let mut input: Box<dyn Read + Send> = match &options.file {
            Some(file) => Box::new(
                File::open(file).with_context(|| format!("cannot open {}", file.display()))?,
            ),
            None => Box::new(std::io::stdin()),
        };
        let _lock = Lock::acquire(ctx.connector, &ctx.home, false)?;
        kind.import_link(&link_ctx, entry, input.as_mut(), options.clean)
    } else {
        let mut output: Box<dyn Write> = match &options.file {
            Some(file) => Box::new(
                File::create(file).with_context(|| format!("cannot create {}", file.display()))?,
            ),
            None => Box::new(std::io::stdout()),
        };
        kind.export_link(&link_ctx, entry, output.as_mut())
            .and_then(|()| output.flush().context("writing the dump failed"))
    };
    entry_log.exit = i32::from(result.is_err());
    log(ctx, &entry_log)?;
    result?;
    if command == "import" {
        out!("Imported {source} into {} {entry}.", link.kind);
    }
    Ok(())
}

/// `--file <path>`, `--file=<path>`, `--clean` (import only), `--yes`, `--help`.
fn parse(name: &str, command: &str, words: &[String]) -> Result<Options> {
    let mut options = Options {
        file: None,
        clean: false,
        yes: false,
        help: false,
    };
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--file" => {
                let file = words
                    .next()
                    .ok_or_else(|| config_err(format!("{name}: --file needs a path")))?;
                options.file = Some(PathBuf::from(file));
            }
            "--clean" if command == "import" => options.clean = true,
            "--yes" | "-y" if command == "import" => options.yes = true,
            "--help" | "-h" => options.help = true,
            _ => match word.strip_prefix("--file=") {
                Some(file) => options.file = Some(PathBuf::from(file)),
                None => {
                    return Err(config_err(format!(
                        "{name}: unexpected `{word}`; see `{name} --help`"
                    )));
                }
            },
        }
    }
    Ok(options)
}

/// A line in the commands log, as `app cmd` writes for its commands.
fn log(ctx: &ServerCtx, entry: &Entry) -> Result<()> {
    let json = serde_json::to_string(entry)?;
    ctx.connector.exec(&format!(
        "mkdir -p -- {} && printf '%s\\n' {} >> {}",
        shell_quote(&ctx.path("logs")),
        shell_quote(&json),
        shell_quote(&ctx.path(COMMANDS_LOG))
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_options() {
        let options = parse(
            "main.import",
            "import",
            &words("--file d.sql --clean --yes"),
        )
        .unwrap();
        assert_eq!(options.file, Some(PathBuf::from("d.sql")));
        assert!(options.clean && options.yes && !options.help);
        let options = parse("main.export", "export", &words("--file=out.sql")).unwrap();
        assert_eq!(options.file, Some(PathBuf::from("out.sql")));
        for (command, text, expected) in [
            ("import", "--file", "--file needs a path"),
            ("export", "--clean", "unexpected `--clean`"),
            ("import", "dump.sql", "unexpected `dump.sql`"),
        ] {
            let err = parse("main.x", command, &words(text)).err().unwrap();
            assert!(err.to_string().contains(expected), "{text}: {err}");
        }
    }
}
