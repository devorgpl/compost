//! Which shared apps `dev up` adds so that every link of an app resolves.

use anyhow::{Context, Result, bail};
use semver::{Op, Version, VersionReq};
use serde_yaml_ng::Value;

use crate::backup::schedule::OFF;
use crate::config::app::{AppConfig, link_name};
use crate::config::backup::AppProfile;
use crate::config::server::ServerConfig;
use crate::shared::{self, Kind, SharedAppConfig, communication};

/// A shared app the plan adds.
#[derive(Debug)]
pub struct Added {
    pub kind: &'static str,
    pub app: SharedAppConfig,
    /// The link that needs it, e.g. `services.storage[files]`.
    pub for_link: String,
}

/// A link that pins a shared app this server doesn't have, e.g. a production
/// server's name, and that another one serves.
#[derive(Debug, Clone)]
pub struct Repin {
    pub kind: String,
    pub entry: String,
    /// The pinned name.
    pub from: String,
    /// The shared app that matches the link without the pin.
    pub to: String,
}

/// The app's links whose `instance` names no shared app of `server`, but that
/// exactly one shared app serves when the pin is left out.
pub fn repins(server: &ServerConfig, app: &AppConfig) -> Result<Vec<Repin>> {
    let mut repins = Vec::new();
    for (kind_name, entry) in app.links() {
        let Some(pin) = entry.get("instance").and_then(Value::as_str) else {
            continue;
        };
        if server.shared_apps().any(|(_, a)| a.name == pin) {
            continue;
        }
        let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
        let mut unpinned = entry.clone();
        if let Value::Mapping(map) = &mut unpinned {
            map.remove("instance");
        }
        let available = server.shared.get(kind_name).map_or(&[][..], Vec::as_slice);
        if let Ok(shared::Resolution::Use(found)) = kind.resolve(&unpinned, available) {
            repins.push(Repin {
                kind: kind_name.to_string(),
                entry: link_name(entry).unwrap_or_default().to_string(),
                from: pin.to_string(),
                to: found.name.clone(),
            });
        }
    }
    Ok(repins)
}

/// The app's backup profiles with a schedule. Its schedules are meant for its
/// servers, so `dev` turns them off on this machine.
pub fn scheduled_profiles(app: &AppConfig) -> Vec<&str> {
    app.backup
        .profiles
        .iter()
        .filter(|(_, profile)| match profile {
            AppProfile::Settings(profile) => profile.schedule.as_deref().is_some_and(|s| s != OFF),
            AppProfile::Enabled(_) => false,
        })
        .map(|(name, _)| name.as_str())
        .collect()
}

/// What `dev` merges over the app's files: links pointed at their new shared
/// apps, and the schedules of the app's backup profiles turned off.
pub fn dev_overlay(repins: &[Repin], app: &AppConfig) -> Option<Value> {
    let scheduled = scheduled_profiles(app);
    if repins.is_empty() && scheduled.is_empty() {
        return None;
    }
    let mut overlay = serde_yaml_ng::Mapping::new();
    if !scheduled.is_empty() {
        let mut profiles = serde_yaml_ng::Mapping::new();
        for name in scheduled {
            let mut profile = serde_yaml_ng::Mapping::new();
            profile.insert("schedule".into(), OFF.into());
            profiles.insert(name.into(), Value::Mapping(profile));
        }
        let mut backup = serde_yaml_ng::Mapping::new();
        backup.insert("profiles".into(), Value::Mapping(profiles));
        overlay.insert("backup".into(), Value::Mapping(backup));
    }
    if repins.is_empty() {
        return Some(Value::Mapping(overlay));
    }
    let mut services = serde_yaml_ng::Mapping::new();
    for repin in repins {
        let mut link = serde_yaml_ng::Mapping::new();
        link.insert("name".into(), repin.entry.as_str().into());
        link.insert("instance".into(), repin.to.as_str().into());
        let entries = services
            .entry(repin.kind.as_str().into())
            .or_insert_with(|| Value::Sequence(Vec::new()));
        if let Value::Sequence(entries) = entries {
            entries.push(Value::Mapping(link));
        }
    }
    overlay.insert("services".into(), Value::Mapping(services));
    Some(Value::Mapping(overlay))
}

/// The app with its links pointed at the shared apps of `repins`.
fn repinned(app: &AppConfig, repins: &[Repin]) -> AppConfig {
    let mut app = app.clone();
    for repin in repins {
        let entries = app.services.get_mut(&repin.kind).into_iter().flatten();
        for entry in entries.filter(|e| link_name(e) == Some(repin.entry.as_str())) {
            if let Value::Mapping(map) = entry {
                map.insert("instance".into(), repin.to.as_str().into());
            }
        }
    }
    app
}

/// A shared app that gets a `host_port`.
#[derive(Debug)]
pub struct Published {
    pub kind: &'static str,
    pub name: String,
    pub port: u16,
    /// It was configured already, so its container is recreated.
    pub existing: bool,
}

/// Port a type publishes on first with `dev up --host`, unless taken.
const HOST_PORTS: &[(&str, u16)] = &[
    ("postgres", 15432),
    ("mysql", 13306),
    ("mariadb", 13306),
    ("valkey", 16379),
    ("redis", 16379),
    ("garage", 13900),
];

/// What `dev up` changes in the server's config.
#[derive(Debug)]
pub struct Plan {
    pub config: ServerConfig,
    /// Links pointed at another shared app than the one they pin.
    pub repins: Vec<Repin>,
    pub added: Vec<Added>,
    pub published: Vec<Published>,
}

impl Plan {
    pub fn changes(&self) -> bool {
        !self.added.is_empty() || !self.published.is_empty()
    }
}

/// `server` with the shared apps that the links of `app` need and it lacks.
/// A link that pins a shared app this server doesn't have uses the one that
/// matches it otherwise, if there is exactly one (see [`repins`]). Shared apps
/// that added ones link to (e.g. Keycloak's Postgres) are added too. With
/// `host` (which tells whether a port is free), the shared apps the app's
/// links use get a `host_port`. Nothing else of existing shared apps changes.
pub fn plan(
    server: &ServerConfig,
    app: &AppConfig,
    host: Option<&dyn Fn(u16) -> bool>,
) -> Result<Plan> {
    let repins = repins(server, app)?;
    let app = repinned(app, &repins);
    let (config, added) = add_missing(server, &app)?;
    let mut plan = Plan {
        config,
        repins,
        added,
        published: Vec::new(),
    };
    if let Some(free) = host {
        publish(&mut plan, &app, free)?;
    }
    if plan.changes() {
        // Validate the result as `server deploy` would.
        let text = serde_yaml_ng::to_string(&plan.config)?;
        plan.config = ServerConfig::parse(&text)?;
    }
    Ok(plan)
}

/// Give the shared apps that the app's links use a `host_port`, if their type
/// has one and they have none.
fn publish(plan: &mut Plan, app: &AppConfig, free: &dyn Fn(u16) -> bool) -> Result<()> {
    let mut used: Vec<u64> = plan
        .config
        .shared_apps()
        .filter_map(|(_, a)| a.configuration.get("host_port").and_then(Value::as_u64))
        .collect();
    for (kind_name, entry) in app.links() {
        let kind = shared::find_kind(kind_name).context("kinds are checked on load")?;
        let apps = plan
            .config
            .shared
            .get(kind_name)
            .map_or(&[][..], Vec::as_slice);
        let Ok(shared::Resolution::Use(found)) = kind.resolve(entry, apps) else {
            continue;
        };
        let name = found.name.clone();
        let Some((_, default)) = HOST_PORTS.iter().find(|(t, _)| *t == found.type_name) else {
            continue;
        };
        if found.is_external() || found.configuration.get("host_port").is_some() {
            continue;
        }
        let mut port = *default;
        while used.contains(&u64::from(port)) || !free(port) {
            port = port.checked_add(1).context("no free port for host_port")?;
        }
        used.push(u64::from(port));
        let app = plan
            .config
            .shared
            .get_mut(kind_name)
            .and_then(|apps| apps.iter_mut().find(|a| a.name == name))
            .context("resolved shared apps are configured")?;
        if !app.configuration.is_mapping() {
            app.configuration = Value::Mapping(serde_yaml_ng::Mapping::new());
        }
        if let Value::Mapping(map) = &mut app.configuration {
            map.insert("host_port".into(), port.into());
        }
        let existing = !plan.added.iter().any(|a| a.app.name == name);
        plan.published.push(Published {
            kind: kind.name(),
            name,
            port,
            existing,
        });
    }
    Ok(())
}

/// `server` with the shared apps the app's links need added, and those added.
fn add_missing(server: &ServerConfig, app: &AppConfig) -> Result<(ServerConfig, Vec<Added>)> {
    let mut config = server.clone();
    let mut added: Vec<Added> = Vec::new();
    let mut queue: Vec<(String, Value, String)> = app
        .links()
        .map(|(kind, entry)| {
            let name = link_name(entry).unwrap_or_default();
            (
                kind.to_string(),
                entry.clone(),
                format!("services.{kind}[{name}]"),
            )
        })
        .collect();
    queue.reverse();

    while let Some((kind_name, entry, path)) = queue.pop() {
        let kind = shared::find_kind(&kind_name).context("kinds are checked on load")?;
        let available = config.shared.get(&kind_name).map_or(&[][..], Vec::as_slice);
        let Err(err) = kind.resolve(&entry, available) else {
            continue;
        };
        let mut app = provide(kind, &entry, &config).with_context(|| format!("{path}: {err:#}"))?;
        kind.apply_defaults(&mut app);
        let list = config.shared.entry(kind_name.clone()).or_default();
        list.push(app.clone());
        if let Err(again) = kind.resolve(&entry, list) {
            bail!("{path}: {again:#}");
        }
        // The new shared app's own links, e.g. Keycloak's database.
        if let Some(links) = kind.shared_links(&app)? {
            let consumer = format!("shared.{kind_name}[{}]", app.name);
            for (link_kind, entries) in links.services.into_iter().rev() {
                for entry in entries.into_iter().rev() {
                    let name = link_name(&entry).unwrap_or_default().to_string();
                    let path = format!("{consumer}: services.{link_kind}[{name}]");
                    queue.push((link_kind.clone(), entry, path));
                }
            }
        }
        added.push(Added {
            kind: kind.name(),
            app,
            for_link: path,
        });
    }
    Ok((config, added))
}

/// The shared app of `server generate`'s local setup that serves `link`:
/// of the link's type (or the kind's first type compost runs), with a
/// version the link accepts, named as the link pins it.
fn provide(kind: &dyn Kind, link: &Value, config: &ServerConfig) -> Result<SharedAppConfig> {
    if kind.name() == communication::KIND {
        bail!("compost doesn't add communication apps");
    }
    let type_name = match link.get("type").and_then(Value::as_str) {
        Some(type_name) => type_name.to_string(),
        None => kind
            .type_names()
            .into_iter()
            .find(|t| snippet(kind, t).is_ok_and(|app| !app.is_external()))
            .with_context(|| format!("no {} type that compost runs", kind.name()))?
            .to_string(),
    };
    let mut app = snippet(kind, &type_name)?;
    if app.is_external() {
        bail!(
            "compost doesn't run {type_name} servers; add one under shared.{}",
            kind.name()
        );
    }
    let requirement = link.get("version").and_then(Value::as_str).unwrap_or("*");
    let requirement =
        VersionReq::parse(requirement).with_context(|| format!("version `{requirement}`"))?;
    let matches = |major: u32| requirement.matches(&Version::new(major.into(), 0, 0));
    if !app.version.is_some_and(matches) {
        let Some(major) = lowest_major(&requirement).filter(|m| matches(*m)) else {
            bail!("no {type_name} version matches `{requirement}`");
        };
        app.version = Some(major);
        app.image = None;
        app.name = format!("{type_name}{major}");
    }
    let pin = link.get("instance").and_then(Value::as_str);
    let taken = |name: &str| config.shared_apps().any(|(_, a)| a.name == name);
    match pin {
        Some(pin) if taken(pin) => bail!("`{pin}` exists, but doesn't match the link"),
        Some(pin) => app.name = pin.to_string(),
        None => {
            let base = app.name.clone();
            let mut n = 2;
            while taken(&app.name) {
                app.name = format!("{base}-{n}");
                n += 1;
            }
        }
    }
    Ok(app)
}

/// The first shared app of `server generate`'s local snippet of a type.
fn snippet(kind: &dyn Kind, type_name: &str) -> Result<SharedAppConfig> {
    let generated = kind.generate(type_name)?;
    let apps: Vec<SharedAppConfig> = serde_yaml_ng::from_str(generated.local)
        .with_context(|| format!("starter configuration of {type_name}"))?;
    apps.into_iter()
        .next()
        .with_context(|| format!("no starter configuration of {type_name}"))
}

/// The lowest major version a requirement's lower bounds allow, e.g. 17 for
/// `>=17`; `None` without lower bounds.
fn lowest_major(requirement: &VersionReq) -> Option<u32> {
    requirement
        .comparators
        .iter()
        .filter_map(|c| match c.op {
            Op::Exact | Op::GreaterEq | Op::Tilde | Op::Caret | Op::Wildcard => Some(c.major),
            Op::Greater if c.minor.is_none() => Some(c.major + 1),
            Op::Greater => Some(c.major),
            _ => None,
        })
        .max()
        .and_then(|major| u32::try_from(major).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(text: &str) -> ServerConfig {
        ServerConfig::parse(text).unwrap()
    }

    fn app(services: &str) -> AppConfig {
        serde_yaml_ng::from_str(&format!("name: shop\nprefix: local\nservices:\n{services}"))
            .unwrap()
    }

    fn plan_of(server: &ServerConfig, app: &AppConfig) -> Result<(ServerConfig, Vec<Added>)> {
        plan(server, app, None).map(|plan| (plan.config, plan.added))
    }

    fn names(added: &[Added]) -> Vec<String> {
        added
            .iter()
            .map(|a| format!("{}/{}/{}", a.kind, a.app.name, a.app.version.unwrap_or(0)))
            .collect()
    }

    #[test]
    fn nothing_missing_changes_nothing() {
        let config =
            server("shared:\n  database:\n    - {name: pg16, type: postgres, version: 16}\n");
        let (planned, added) = plan_of(
            &config,
            &app("  database:\n    - {name: db, type: postgres, version: '>=16'}\n"),
        )
        .unwrap();
        assert!(added.is_empty());
        assert_eq!(planned.shared["database"].len(), 1);
    }

    #[test]
    fn adds_missing_storage_and_keeps_the_database() {
        let config =
            server("shared:\n  database:\n    - {name: pg16, type: postgres, version: 16}\n");
        let links = "  database:\n    - {name: db, type: postgres, version: '>=16'}\n  storage:\n    - {name: files}\n";
        let (planned, added) = plan_of(&config, &app(links)).unwrap();
        assert_eq!(names(&added), ["storage/storage/2"]);
        assert_eq!(added[0].for_link, "services.storage[files]");
        assert_eq!(planned.shared["database"][0].name, "pg16");
        assert_eq!(planned.shared["storage"][0].type_name, "garage");
    }

    #[test]
    fn picks_a_version_the_link_accepts() {
        let config =
            server("shared:\n  database:\n    - {name: pg16, type: postgres, version: 16}\n");
        let (_, added) = plan_of(
            &config,
            &app("  database:\n    - {name: db, type: postgres, version: '>=17'}\n"),
        )
        .unwrap();
        // The starter's 18 matches, so its name and image stay.
        assert_eq!(names(&added), ["database/postgres18/18"]);

        let (_, added) = plan_of(
            &config,
            &app("  database:\n    - {name: db, type: postgres, version: '^15'}\n"),
        )
        .unwrap();
        assert_eq!(names(&added), ["database/postgres15/15"]);
        assert_eq!(added[0].app.image, None);

        let err = plan_of(
            &config,
            &app("  database:\n    - {name: db, type: postgres, version: '<16'}\n"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no postgres version matches"),
            "{err:#}"
        );
    }

    #[test]
    fn adds_what_added_shared_apps_link_to() {
        let (planned, added) = plan_of(
            &ServerConfig::default(),
            &app("  oidc:\n    - {name: login, type: keycloak, targets: []}\n"),
        )
        .unwrap();
        assert_eq!(
            names(&added),
            [
                "oidc/keycloak/26",
                "database/postgres18/18",
                "ingress/caddy-main/2"
            ]
        );
        assert!(
            added[1]
                .for_link
                .starts_with("shared.oidc[keycloak]: services.database[")
        );
        assert_eq!(planned.shared.len(), 3);
    }

    #[test]
    fn pinned_instance_names_the_added_app() {
        let (_, added) = plan_of(
            &ServerConfig::default(),
            &app("  keyvalue:\n    - {name: cache, type: valkey, instance: cache}\n"),
        )
        .unwrap();
        assert_eq!(names(&added), ["keyvalue/cache/9"]);
    }

    #[test]
    fn optional_links_add_nothing() {
        let (_, added) = plan_of(
            &ServerConfig::default(),
            &app("  monitoring:\n    - {name: telemetry, optional: true}\n  ingress:\n    - {name: site}\n"),
        )
        .unwrap();
        assert!(added.is_empty());
    }

    #[test]
    fn host_publishes_the_used_shared_apps_on_free_ports() {
        let config = server(
            "shared:\n  database:\n    - {name: pg16, type: postgres, version: 16}\n    - {name: other, type: postgres, version: 15, configuration: {host_port: 15432}}\n  ingress:\n    - {name: caddy, type: caddy}\n",
        );
        let links = "  database:\n    - {name: db, type: postgres, version: '>=16'}\n  storage:\n    - {name: files}\n  ingress:\n    - {name: site, domains: [shop.localhost]}\n";
        let busy = |port: u16| port != 15433;
        let planned = plan(&config, &app(links), Some(&busy)).unwrap();
        let published: Vec<String> = planned
            .published
            .iter()
            .map(|p| format!("{}/{}:{}/{}", p.kind, p.name, p.port, p.existing))
            .collect();
        assert_eq!(
            published,
            ["database/pg16:15434/true", "storage/storage:13900/false"]
        );
        let pg = &planned.config.shared["database"][0];
        assert_eq!(
            pg.configuration.get("host_port").and_then(Value::as_u64),
            Some(15434)
        );

        // Published already: nothing changes.
        let again = plan(&planned.config, &app(links), Some(&busy)).unwrap();
        assert!(!again.changes());
    }

    #[test]
    fn host_ports_must_differ() {
        let err = ServerConfig::parse(
            "shared:\n  database:\n    - {name: a, type: postgres, version: 16, configuration: {host_port: 15432}}\n  keyvalue:\n    - {name: b, type: valkey, configuration: {host_port: 15432}}\n",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("host_port 15432 is also the host_port of shared.database[a]"),
            "{err}"
        );
    }

    #[test]
    fn a_pin_of_another_server_uses_the_matching_shared_app() {
        let config = server(
            "shared:\n  database:\n    - {name: postgres18, type: postgres, version: 18}\n  keyvalue:\n    - {name: valkey, type: valkey}\n",
        );
        let links = "  database:\n    - {name: db, type: postgres, version: '18', instance: postgres18}\n  keyvalue:\n    - {name: cache, type: valkey, instance: valkey-main}\n    - {name: other, type: redis, instance: redis-main}\n";
        let planned = plan(&config, &app(links), None).unwrap();
        let repins: Vec<String> = planned
            .repins
            .iter()
            .map(|r| format!("{}/{}: {} -> {}", r.kind, r.entry, r.from, r.to))
            .collect();
        // postgres18 exists; nothing serves redis, so it is added with the pin's name.
        assert_eq!(repins, ["keyvalue/cache: valkey-main -> valkey"]);
        assert_eq!(names(&planned.added), ["keyvalue/redis-main/8"]);

        let overlay = dev_overlay(&planned.repins, &app(links)).unwrap();
        assert_eq!(
            serde_yaml_ng::to_string(&overlay).unwrap(),
            "services:\n  keyvalue:\n  - name: cache\n    instance: valkey\n"
        );
    }

    #[test]
    fn schedules_of_the_app_are_turned_off() {
        let app: AppConfig = serde_yaml_ng::from_str(
            "name: shop\nbackup:\n  profiles:\n    daily: {schedule: '30 3 * * *', keep: 14}\n    weekly: {schedule: 'off'}\n    manual: {keep: 3}\n    nightly: false\n",
        )
        .unwrap();
        assert_eq!(scheduled_profiles(&app), ["daily"]);
        let overlay = dev_overlay(&[], &app).unwrap();
        assert_eq!(
            serde_yaml_ng::to_string(&overlay).unwrap(),
            "backup:\n  profiles:\n    daily:\n      schedule: off\n"
        );
    }
}
