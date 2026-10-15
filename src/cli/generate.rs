//! `compost server generate` and `compost app generate`: starter configuration files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::config::app::ComposeFile;
use crate::config::check_name;
use crate::error::config_err;
use crate::shared::{self, Generated, Kind};

const SERVER_FILE: &str = "compost-server.yaml";
const SERVER_PROD_FILE: &str = "compost-server.prod.yaml";
const SERVER_SECRETS_EXAMPLE: &str = ".compost-server.secrets.example.yaml";
const APP_FILE: &str = "compost.yaml";
const APP_SECRETS_EXAMPLE: &str = ".compost.secrets.example.yaml";
const DEFAULT_COMPOSE_FILE: &str = "docker-compose.yaml";

#[derive(Debug, Args)]
pub struct ServerGenerateArgs {
    /// List the shared app types that can be generated
    #[arg(long, conflicts_with_all = ["with", "force"])]
    pub list: bool,
    /// Shared app types to include, e.g. `caddy,postgres` (asked when not given)
    #[arg(long, value_delimiter = ',')]
    pub with: Vec<String>,
    /// Overwrite existing files
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct AppGenerateArgs {
    /// Compose file to read services from
    #[arg(long, default_value = DEFAULT_COMPOSE_FILE)]
    pub compose_file: PathBuf,
    /// Application name (default: the current directory's name)
    #[arg(long)]
    pub name: Option<String>,
    /// Overwrite existing files
    #[arg(long)]
    pub force: bool,
}

/// A shared app type with its kind.
struct Choice {
    kind: &'static dyn Kind,
    type_name: &'static str,
}

impl Choice {
    fn label(&self) -> String {
        format!("{}/{}", self.kind.name(), self.type_name)
    }
}

fn choices() -> Vec<Choice> {
    shared::kind_names()
        .into_iter()
        .filter_map(shared::find_kind)
        .flat_map(|kind| {
            kind.type_names()
                .into_iter()
                .map(move |type_name| Choice { kind, type_name })
        })
        .collect()
}

pub fn server(args: ServerGenerateArgs) -> Result<()> {
    let all = choices();
    if args.list {
        for choice in &all {
            out!("{}", choice.label());
        }
        return Ok(());
    }

    let selected = if args.with.is_empty() {
        ask_types(&all)?
    } else {
        args.with
            .iter()
            .map(|name| find_choice(&all, name))
            .collect::<Result<Vec<_>>>()?
    };
    let files = server_files(&selected)?;

    write_all(
        &[
            (SERVER_FILE, &files.base),
            (SERVER_PROD_FILE, &files.prod),
            (SERVER_SECRETS_EXAMPLE, &files.secrets),
        ],
        args.force,
    )?;
    out!();
    out!("On this machine:   compost server deploy --apply");
    out!(
        "On a real server:  compost server deploy --apply -f {SERVER_FILE},{SERVER_PROD_FILE},.compost-server.secrets.yaml"
    );
    print_notes(&files.notes);
    Ok(())
}

/// A type by `type` or `kind/type`.
fn find_choice<'a>(all: &'a [Choice], name: &str) -> Result<&'a Choice> {
    let name = name.trim();
    all.iter()
        .find(|c| c.type_name == name || c.label() == name)
        .ok_or_else(|| {
            let known: Vec<String> = all.iter().map(Choice::label).collect();
            config_err(format!(
                "unknown shared app type `{name}` (known: {})",
                known.join(", ")
            ))
        })
}

/// Show the types as a numbered list and read a comma-separated choice.
fn ask_types(all: &[Choice]) -> Result<Vec<&Choice>> {
    if !std::io::stdin().is_terminal() {
        return Err(config_err(
            "no terminal to ask which shared apps to include; pass --with (see --list)",
        ));
    }
    out!("Shared apps:");
    for (i, choice) in all.iter().enumerate() {
        out!("  {}) {}", i + 1, choice.label());
    }
    out_part!("Include (numbers or names, comma-separated): ");
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;

    let mut selected = Vec::new();
    for item in answer.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let choice = match item.parse::<usize>() {
            Ok(n) if (1..=all.len()).contains(&n) => &all[n - 1],
            Ok(n) => return Err(config_err(format!("no shared app number {n}"))),
            Err(_) => find_choice(all, item)?,
        };
        selected.push(choice);
    }
    if selected.is_empty() {
        return Err(config_err("no shared apps selected"));
    }
    Ok(selected)
}

struct ServerFiles {
    base: String,
    prod: String,
    secrets: String,
    notes: Vec<&'static str>,
}

fn server_files(selected: &[&Choice]) -> Result<ServerFiles> {
    // Kind name → snippets, in the order the types were chosen; once per type.
    let mut by_kind: BTreeMap<&str, Vec<Generated>> = BTreeMap::new();
    let mut seen = Vec::new();
    for choice in selected {
        let label = choice.label();
        if seen.contains(&label) {
            continue;
        }
        seen.push(label);
        let generated = choice.kind.generate(choice.type_name)?;
        by_kind
            .entry(choice.kind.name())
            .or_default()
            .push(generated);
    }

    let notes = by_kind
        .values()
        .flatten()
        .flat_map(|g| g.notes.iter().copied())
        .collect();
    Ok(ServerFiles {
        base: shared_file(
            "# Shared applications of the server (spec §5). This file runs them on this\n\
             # machine; compost-server.prod.yaml holds what a real server changes.\n\
             # Safe to commit: secrets go in .compost-server.secrets.yaml.\n\
             #\n\
             #   compost server deploy --apply\n",
            &by_kind,
            |g| Some(g.local),
        ),
        prod: shared_file(
            "# Overrides for a real server, merged by `name` over compost-server.yaml:\n\
             #\n\
             #   compost server deploy --apply -f compost-server.yaml,compost-server.prod.yaml,.compost-server.secrets.yaml\n",
            &by_kind,
            |g| g.prod,
        ),
        secrets: shared_file(
            "# Secrets of the shared applications. Copy to .compost-server.secrets.yaml,\n\
             # which `compost server deploy` loads next to compost-server.yaml. Never commit\n\
             # the copy. Merged by `name`, so only the secret keys are repeated here.\n",
            &by_kind,
            |g| g.secrets,
        ),
        notes,
    })
}

/// `shared:` with the snippets `part` picks. Without any, `shared: {}`, so the
/// file still merges as an overlay.
fn shared_file(
    header: &str,
    by_kind: &BTreeMap<&str, Vec<Generated>>,
    part: impl Fn(&Generated) -> Option<&'static str>,
) -> String {
    let mut body = String::new();
    for (kind, generated) in by_kind {
        let snippets: Vec<&str> = generated.iter().filter_map(&part).collect();
        if snippets.is_empty() {
            continue;
        }
        let _ = writeln!(body, "  {kind}:");
        for snippet in snippets {
            body.push_str(snippet);
        }
    }
    if body.is_empty() {
        format!("{header}shared: {{}}\n")
    } else {
        format!("{header}shared:\n{body}")
    }
}

pub fn app(args: AppGenerateArgs) -> Result<()> {
    let compose = ComposeFile::read(&args.compose_file)?;
    let name = match args.name {
        Some(name) => name,
        None => default_app_name()?,
    };
    check_name("app name", &name)?;

    let compose_file = args.compose_file.to_string_lossy();
    let (config, secrets) = app_files(&name, &compose_file, &compose);
    write_all(
        &[(APP_FILE, &config), (APP_SECRETS_EXAMPLE, &secrets)],
        args.force,
    )?;
    out!();
    out!("Next:");
    out!("  1. Uncomment the links the app uses in both files and fill in the ports.");
    out!("  2. cp {APP_SECRETS_EXAMPLE} .compost.secrets.yaml   # set domains and passwords");
    out!("  3. compost secret deploy .compost.secrets.yaml --prefix staging");
    out!("  4. compost app deploy --apply --prefix staging");
    Ok(())
}

/// The current directory's name as an app name, e.g. `My_App` → `my-app`.
fn default_app_name() -> Result<String> {
    let dir = std::env::current_dir().context("cannot read the current directory")?;
    let raw = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = sanitize_name(&raw);
    if name.is_empty() {
        return Err(config_err(
            "cannot derive an app name from the current directory; pass --name",
        ));
    }
    Ok(name)
}

fn sanitize_name(raw: &str) -> String {
    let mapped: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_start_matches(|c: char| !c.is_ascii_lowercase());
    let mut name: String = trimmed.chars().take(31).collect();
    while name.ends_with('-') {
        name.pop();
    }
    name
}

/// `compost.yaml` and `.compost.secrets.example.yaml` for the compose services.
fn app_files(name: &str, compose_file: &str, compose: &ComposeFile) -> (String, String) {
    let (services, skipped): (Vec<&str>, Vec<&str>) = compose
        .services
        .keys()
        .map(String::as_str)
        .partition(|s| check_name("", s).is_ok());
    let first = services.first().copied().unwrap_or("<service>");
    let builds = compose.services.values().any(|s| s.builds);

    let mut config = format!(
        "# How {name} is deployed (spec §6.1). Safe to commit: domains and secrets\n\
         # go in .compost.secrets.yaml (see {APP_SECRETS_EXAMPLE}).\n\
         name: {name}\n\
         deployment:\n"
    );
    if compose_file != DEFAULT_COMPOSE_FILE {
        let _ = writeln!(config, "  compose_file: {compose_file}");
    }
    let _ = writeln!(
        config,
        "  # true: images are built on the server, so the project folder is uploaded.\n  build: {builds}"
    );
    let db_name = name.replace('-', "_");
    let _ = write!(
        config,
        "
# Links to shared apps of the server. Uncomment the ones the app uses, together
# with the matching entries in .compost.secrets.yaml.
# services:
#   ingress:
#     - name: site
#       routes:
#         - name: web
#           target: {first}
#           port: 80          # port the service listens on in its container
#   database:
#     - name: main-db
#       type: postgres
#       version: \">=16\"
#       targets: [{first}]
#       environment:
#         DATABASE_URL: db_url

# Names that links refer to, mapped to services in {compose_file}.
"
    );
    if services.is_empty() {
        config.push_str("app_services: []\n");
    } else {
        config.push_str("app_services:\n");
        for service in &services {
            let _ = writeln!(config, "  - name: {service}\n    service: {service}");
        }
    }
    if !skipped.is_empty() {
        let _ = writeln!(
            config,
            "# Not listed (names must be lowercase letters, digits and `-`): {}",
            skipped.join(", ")
        );
    }

    let secrets = format!(
        "# Values and secrets of one instance of {name}. Copy to .compost.secrets.yaml
# (never commit the copy) and store it on the server:
#
#   compost secret deploy .compost.secrets.yaml --prefix staging
#   compost app deploy --apply --prefix staging
#
# Files named .compost.*.yaml are never uploaded with the sources.
name: {name}
prefix: staging

# Uncomment together with the matching entries in compost.yaml.
# services:
#   ingress:
#     - name: site
#       domains: [\"staging.example.com\"]
#   database:
#     - name: main-db
#       # Optional. Defaults: {db_name}_<prefix>_main_db for both, and a
#       # generated password that compost keeps on the server.
#       db_name: {db_name}_staging
#       db_user: {db_name}
#       db_password: change-me-to-a-long-random-string
"
    );
    (config, secrets)
}

/// Write the files; existing ones only with `force`.
fn write_all(files: &[(&str, &String)], force: bool) -> Result<()> {
    if !force {
        let existing: Vec<&str> = files
            .iter()
            .map(|(path, _)| *path)
            .filter(|path| Path::new(path).exists())
            .collect();
        if !existing.is_empty() {
            return Err(config_err(format!(
                "{} already exist(s); add --force to overwrite",
                existing.join(", ")
            )));
        }
    }
    for (path, contents) in files {
        fs::write(path, contents).with_context(|| format!("cannot write {path}"))?;
        out!("Wrote {path}");
    }
    std::io::stdout().flush().ok();
    Ok(())
}

fn print_notes(notes: &[&str]) {
    if notes.is_empty() {
        return;
    }
    out!();
    out!("Notes:");
    for note in notes {
        out!("  - {note}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::app::AppConfig;
    use crate::config::server::ServerConfig;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    /// The secrets example with its values, as a user would fill it in.
    fn copy_of_example(text: &str) -> String {
        text.replace("change-me-to-a-long-random-string", "secret")
    }

    #[test]
    fn generated_server_files_are_valid() {
        let all = choices();
        assert!(all.len() >= 2, "caddy and postgres");
        // Every non-empty subset (in kind order).
        for mask in 1..(1u32 << all.len()) {
            let selected: Vec<&Choice> = all
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, c)| c)
                .collect();
            let files = server_files(&selected).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let base = write(dir.path(), "base.yaml", &files.base);
            let prod = write(dir.path(), "prod.yaml", &files.prod);
            let secrets = write(dir.path(), "secrets.yaml", &copy_of_example(&files.secrets));

            ServerConfig::load(&[&base])
                .unwrap_or_else(|e| panic!("{mask}: {e:#}\n{}", files.base));
            let config = ServerConfig::load(&[&base, &prod, &secrets])
                .unwrap_or_else(|e| panic!("{mask}: {e:#}\n{}\n{}", files.prod, files.secrets));
            assert_eq!(config.shared_apps().count(), selected.len());
        }
    }

    #[test]
    fn prod_overlay_changes_caddy_ports() {
        let all = choices();
        let selected: Vec<&Choice> = all.iter().collect();
        let files = server_files(&selected).unwrap();
        assert!(files.base.contains("http_port: 18080"));
        assert!(files.prod.contains("http_port: 80"));
        assert!(files.secrets.contains("root_password"));
        assert!(files.notes.iter().any(|n| n.contains("root_password")));
    }

    #[test]
    fn finds_types_by_name_or_label() {
        let all = choices();
        assert_eq!(find_choice(&all, "caddy").unwrap().type_name, "caddy");
        assert_eq!(
            find_choice(&all, "database/postgres").unwrap().type_name,
            "postgres"
        );
        let err = find_choice(&all, "mysql").err().unwrap();
        assert!(format!("{err:#}").contains("known: "), "{err:#}");
    }

    #[test]
    fn sanitizes_directory_names() {
        assert_eq!(sanitize_name("My_App"), "my-app");
        assert_eq!(sanitize_name("2fast"), "fast");
        assert_eq!(sanitize_name("app-"), "app");
        assert_eq!(sanitize_name("___"), "");
    }

    #[test]
    fn generated_app_files_are_valid() {
        let compose_text = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/sampleapp/docker-compose.yaml"),
        )
        .unwrap();
        let compose = ComposeFile::parse(&compose_text).unwrap();
        let (config, secrets) = app_files("my-app", "docker-compose.yaml", &compose);
        assert!(config.contains("build: true"), "{config}");

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "docker-compose.yaml", &compose_text);
        let compost = write(dir.path(), "compost.yaml", &config);
        let overlay = write(dir.path(), "secrets.yaml", &secrets);
        let loaded = AppConfig::load(&[compost, overlay], None).unwrap();
        assert_eq!(loaded.config.instance(), "my-app-staging");
        assert_eq!(loaded.config.app_services.len(), 3);

        // With the examples uncommented in both files.
        // From `# services:` to the next blank line.
        let uncomment = |text: &str| {
            let mut inside = false;
            let lines: Vec<&str> = text
                .lines()
                .map(|line| {
                    inside = (inside && !line.is_empty()) || line == "# services:";
                    if inside { &line[2..] } else { line }
                })
                .collect();
            lines.join("\n")
        };
        let compost = write(dir.path(), "compost.yaml", &uncomment(&config));
        let overlay = write(dir.path(), "secrets.yaml", &uncomment(&secrets));
        let loaded = AppConfig::load(&[compost, overlay], None)
            .unwrap_or_else(|e| panic!("{e:#}\n{}", uncomment(&config)));
        assert_eq!(loaded.config.links().count(), 2);
    }

    #[test]
    fn app_files_skip_invalid_service_names() {
        let compose =
            ComposeFile::parse("{services: {web: {image: x}, Bad_Name: {image: y}}}").unwrap();
        let (config, _) = app_files("a", "compose.yaml", &compose);
        assert!(config.contains("compose_file: compose.yaml"));
        assert!(config.contains("build: false"));
        assert!(
            config
                .contains("Not listed (names must be lowercase letters, digits and `-`): Bad_Name")
        );
    }
}
