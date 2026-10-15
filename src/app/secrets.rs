//! Secrets of application instances kept on the server (`compost secret …`): a
//! deployment overlay per instance, which `app deploy` merges after compost.yaml.

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use super::state::initialized_server;
use crate::config::app::default_prefix;
use crate::config::check_name;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::server::lock::Lock;
use crate::shared::ServerCtx;

/// Relative to COMPOST_SERVER_HOME. Outside `apps/`, so `app remove` keeps them.
const DIR: &str = "secrets/apps";

/// The stored overlay in an app archive, next to the instance's `compost.yaml`.
pub const ARCHIVE_ITEM: &str = "secrets.yaml";

/// `secrets/apps/<app>/<prefix>.yaml`
pub fn path(name: &str, prefix: &str) -> String {
    format!("{DIR}/{name}/{prefix}.yaml")
}

/// An overlay ready to be stored.
#[derive(Debug)]
pub struct Overlay {
    pub name: String,
    pub prefix: String,
    pub text: String,
}

/// Read `name` and `prefix` of an overlay; `prefix` replaces the file's own.
/// The stored copy always names its prefix.
pub fn prepare(text: &str, prefix: Option<&str>) -> Result<Overlay> {
    let mut value: Value =
        serde_yaml_ng::from_str(text).map_err(|e| config_err(format!("secrets file: {e}")))?;
    let Value::Mapping(map) = &mut value else {
        return Err(config_err("secrets file: expected a mapping with `name`"));
    };
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| config_err("secrets file: `name` (the app name) is required"))?
        .to_string();
    let prefix = match prefix {
        Some(prefix) => prefix.to_string(),
        None => match map.get("prefix") {
            None => default_prefix(),
            Some(value) => value
                .as_str()
                .ok_or_else(|| config_err("secrets file: `prefix` must be a string"))?
                .to_string(),
        },
    };
    check_name("app name", &name)?;
    check_name("prefix", &prefix)?;
    map.insert("prefix".into(), Value::String(prefix.clone()));
    Ok(Overlay {
        name,
        prefix,
        text: serde_yaml_ng::to_string(&value)?,
    })
}

/// `compost secret deploy`
pub fn deploy(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    overlay: &Overlay,
    force_unlock: bool,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let _lock = Lock::acquire(connector, &ctx.home, force_unlock)?;
    let path = store(&ctx, overlay)?;
    out!(
        "Stored secrets of {}-{} in {path}",
        overlay.name,
        overlay.prefix
    );
    Ok(())
}

/// Write the overlay of its instance; returns the absolute path. The caller
/// holds the lock.
pub fn store(ctx: &ServerCtx, overlay: &Overlay) -> Result<String> {
    let path = ctx.path(&path(&overlay.name, &overlay.prefix));
    ctx.connector
        .write_file(&path, overlay.text.as_bytes(), 0o600)?;
    Ok(path)
}

/// The stored overlay of an instance, if any. Reads without checking the
/// server; `app deploy` does that next.
pub fn read(
    connector: &dyn Connector,
    server_home: &str,
    name: &str,
    prefix: &str,
) -> Result<Option<(String, Value)>> {
    let ctx = ServerCtx {
        connector,
        home: connector.resolve_path(server_home)?,
    };
    read_in(&ctx, name, prefix)
}

/// The stored overlay of an instance with its absolute path, if any.
pub fn read_in(ctx: &ServerCtx, name: &str, prefix: &str) -> Result<Option<(String, Value)>> {
    let path = ctx.path(&path(name, prefix));
    let Some(text) = ctx.connector.read_file(&path)? else {
        return Ok(None);
    };
    let value: Value =
        serde_yaml_ng::from_str(&text).with_context(|| format!("invalid {path} on the server"))?;
    if value.get("name").and_then(Value::as_str) != Some(name) {
        return Err(config_err(format!(
            "{path} on the server is not for app `{name}`"
        )));
    }
    Ok(Some((path, value)))
}

/// `compost secret list`
pub fn list(connector: &dyn Connector, server_home: &str, manager_key: &str) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let mut found = Vec::new();
    for app in connector.list_dir(&ctx.path(DIR))? {
        for file in connector.list_dir(&ctx.path(&format!("{DIR}/{app}")))? {
            if let Some(prefix) = file.strip_suffix(".yaml") {
                found.push((app.clone(), prefix.to_string()));
            }
        }
    }
    if found.is_empty() {
        out!("No stored secrets. Add some with `compost secret deploy`.");
        return Ok(());
    }
    found.sort();
    let width = found.iter().map(|(app, _)| app.len()).max().unwrap_or(0);
    out!("{:<width$}  PREFIX", "APP");
    for (app, prefix) in found {
        out!("{app:<width$}  {prefix}");
    }
    Ok(())
}

/// `compost secret remove`
pub fn remove(
    connector: &dyn Connector,
    server_home: &str,
    manager_key: &str,
    name: &str,
    prefix: &str,
    force_unlock: bool,
) -> Result<()> {
    let ctx = initialized_server(connector, server_home, manager_key)?;
    let relative = path(name, prefix);
    if ctx.read(&relative)?.is_none() {
        return Err(config_err(format!(
            "no stored secrets for {name}-{prefix} (see `compost secret list`)"
        )));
    }
    let _lock = Lock::acquire(connector, &ctx.home, force_unlock)?;
    let dir = shell_quote(&ctx.path(&format!("{DIR}/{name}")));
    connector.exec(&format!(
        "rm -f -- {} && rmdir -- {dir} 2>/dev/null || true",
        shell_quote(&ctx.path(&relative))
    ))?;
    out!("Removed the stored secrets of {name}-{prefix}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_from_flag_file_or_default() {
        let overlay = prepare("{name: app, prefix: staging}", None).unwrap();
        assert_eq!(
            (overlay.name.as_str(), overlay.prefix.as_str()),
            ("app", "staging")
        );

        let overlay = prepare("{name: app, prefix: staging}", Some("review")).unwrap();
        assert_eq!(overlay.prefix, "review");
        assert!(overlay.text.contains("prefix: review"), "{}", overlay.text);

        let overlay = prepare("name: app", None).unwrap();
        assert_eq!(overlay.prefix, "main");
        assert!(overlay.text.contains("prefix: main"), "{}", overlay.text);
    }

    #[test]
    fn rejects_files_without_valid_names() {
        for (text, expected) in [
            ("prefix: x", "`name`"),
            ("[a]", "mapping"),
            ("name: Bad", "invalid app name"),
            ("{name: a, prefix: 1}", "must be a string"),
        ] {
            let err = format!("{:#}", prepare(text, None).unwrap_err());
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn path_is_outside_the_instance_folder() {
        assert_eq!(path("app", "staging"), "secrets/apps/app/staging.yaml");
    }
}
