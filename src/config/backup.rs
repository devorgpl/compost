//! Backup profiles: named settings for `--profile`, defined in
//! `compost-server.yaml` and overridden or added per app in `compost.yaml`.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::app::{Step, StopServices};
use super::check_name;
use crate::backup::{Component, Format};
use crate::error::config_err;

/// Target names compost defines itself.
pub const BUILTIN_TARGETS: [&str; 2] = ["local", "server"];

/// One profile. Unset keys come from the profile at the next level down
/// (app profile → server profile → the app's `backup` settings).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<Component>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    /// `local`, `server` or a name from `backup.targets`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Archives of this profile kept per instance (and for the server) at the
    /// target; older ones are deleted after each backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<u32>,
    /// Server profiles only: whether `backup run` also backs up the shared apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<bool>,
    /// App profiles only: replace the keys of the app's `backup` settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopServices>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_backup: Option<Vec<Step>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_backup: Option<Vec<Step>>,
}

/// A profile in `compost.yaml`: settings, or `false` to opt out of a server profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AppProfile {
    Enabled(bool),
    Settings(Box<Profile>),
}

// By hand, so that a mistyped key is reported as such rather than as
// "no variant matched".
impl<'de> Deserialize<'de> for AppProfile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_yaml_ng::Value::deserialize(deserializer)? {
            serde_yaml_ng::Value::Bool(enabled) => Ok(AppProfile::Enabled(enabled)),
            value => serde_yaml_ng::from_value(value)
                .map(|profile| AppProfile::Settings(Box::new(profile)))
                .map_err(D::Error::custom),
        }
    }
}

impl Profile {
    /// Checks common to both levels.
    fn validate_common(&self, path: &str) -> Result<()> {
        if self.keep == Some(0) {
            return Err(config_err(format!(
                "{path}.keep: must be at least 1 (leave it out to keep every archive)"
            )));
        }
        if let Some(target) = &self.target {
            if target.trim().is_empty() {
                return Err(config_err(format!("{path}.target: must not be empty")));
            }
        }
        Ok(())
    }

    /// A profile of `compost-server.yaml`; `targets` are the named targets.
    pub fn validate_server(&self, name: &str, targets: &[&str]) -> Result<()> {
        let path = format!("backup.profiles.{name}");
        check_name("backup profile name", name)?;
        self.validate_common(&path)?;
        for (key, set) in [
            ("volumes", self.volumes.is_some()),
            ("stop", self.stop.is_some()),
            ("before_backup", self.before_backup.is_some()),
            ("after_backup", self.after_backup.is_some()),
        ] {
            if set {
                return Err(config_err(format!(
                    "{path}.{key}: set it in the app's compost.yaml (`backup.profiles.{name}.{key}`)"
                )));
            }
        }
        if let Some(target) = &self.target {
            check_target(&format!("{path}.target"), target, targets)?;
        }
        Ok(())
    }

    /// A profile of `compost.yaml`; its parts are checked against the compose file by the caller.
    pub fn validate_app(&self, name: &str) -> Result<()> {
        let path = format!("backup.profiles.{name}");
        check_name("backup profile name", name)?;
        self.validate_common(&path)?;
        if self.server.is_some() {
            return Err(config_err(format!(
                "{path}.server: only a profile of compost-server.yaml backs up the shared apps"
            )));
        }
        Ok(())
    }

    /// `self`'s keys over `base`'s.
    pub fn over(&self, base: &Profile) -> Profile {
        Profile {
            include: self.include.clone().or_else(|| base.include.clone()),
            links: self.links.clone().or_else(|| base.links.clone()),
            format: self.format.or(base.format),
            target: self.target.clone().or_else(|| base.target.clone()),
            keep: self.keep.or(base.keep),
            server: self.server.or(base.server),
            volumes: self.volumes.clone().or_else(|| base.volumes.clone()),
            stop: self.stop.clone().or_else(|| base.stop.clone()),
            before_backup: self
                .before_backup
                .clone()
                .or_else(|| base.before_backup.clone()),
            after_backup: self
                .after_backup
                .clone()
                .or_else(|| base.after_backup.clone()),
        }
    }
}

impl AppProfile {
    pub fn validate(&self, name: &str) -> Result<()> {
        match self {
            AppProfile::Enabled(false) => check_name("backup profile name", name),
            AppProfile::Enabled(true) => Err(config_err(format!(
                "backup.profiles.{name}: `true` is not needed; use `false` to opt out, or settings"
            ))),
            AppProfile::Settings(profile) => profile.validate_app(name),
        }
    }
}

/// `target` must be a built-in target or one of `targets`.
pub fn check_target(path: &str, target: &str, targets: &[&str]) -> Result<()> {
    if BUILTIN_TARGETS.contains(&target) || targets.contains(&target) {
        Ok(())
    } else {
        Err(unknown_target(path, target, targets))
    }
}

/// `path` is the config key, if the target came from one.
pub fn unknown_target(path: &str, target: &str, targets: &[&str]) -> anyhow::Error {
    let mut known: Vec<&str> = BUILTIN_TARGETS.to_vec();
    known.extend(targets);
    let path = if path.is_empty() {
        String::new()
    } else {
        format!("{path}: ")
    };
    config_err(format!(
        "{path}unknown backup target `{target}` (known: {})",
        known.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn profile(text: &str) -> Profile {
        serde_yaml_ng::from_str(text).unwrap()
    }

    #[test]
    fn app_profiles_are_settings_or_false() {
        let profiles: BTreeMap<String, AppProfile> =
            serde_yaml_ng::from_str("{daily: false, weekly: {keep: 2}}").unwrap();
        assert_eq!(profiles["daily"], AppProfile::Enabled(false));
        assert_eq!(
            profiles["weekly"],
            AppProfile::Settings(Box::new(profile("{keep: 2}")))
        );
        assert!(AppProfile::Enabled(true).validate("daily").is_err());
        let err = serde_yaml_ng::from_str::<AppProfile>("{nope: 1}").unwrap_err();
        assert!(err.to_string().contains("unknown field `nope`"), "{err}");
    }

    #[test]
    fn upper_keys_win() {
        let base = profile("{include: [links], keep: 7, target: server}");
        let top = profile("{keep: 2, format: raw}");
        let merged = top.over(&base);
        assert_eq!(merged.include, Some(vec![Component::Links]));
        assert_eq!(merged.keep, Some(2));
        assert_eq!(merged.format, Some(Format::Raw));
        assert_eq!(merged.target.as_deref(), Some("server"));
    }

    #[test]
    fn validation() {
        let err = |p: &str| {
            format!(
                "{:#}",
                profile(p).validate_server("daily", &["nas"]).unwrap_err()
            )
        };
        assert!(err("{keep: 0}").contains("at least 1"));
        assert!(err("{volumes: [x]}").contains("compost.yaml"));
        assert!(
            err("{target: s3}").contains("unknown backup target `s3` (known: local, server, nas)")
        );
        assert!(
            profile("{target: nas}")
                .validate_server("daily", &["nas"])
                .is_ok()
        );
        assert!(profile("{server: false}").validate_app("daily").is_err());
        assert!(profile("{}").validate_app("Daily").is_err());
    }
}
