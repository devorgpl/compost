//! The settings of one backup: a profile merged from the server's and the
//! app's, and the command line's flags over it.

use super::{Component, Format, components};
use crate::config::app::{AppConfig, Step, StopServices};
use crate::config::backup::{AppProfile, Profile};
use crate::config::server::BackupSettings;
use crate::error::config_err;

/// Flags of `app backup` / `server backup`; unset ones come from the profile.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub profile: Option<String>,
    pub include: Vec<Component>,
    pub links: Vec<String>,
    pub format: Option<Format>,
    pub target: Option<String>,
}

/// What one backup of an instance saves and where it goes.
#[derive(Debug, Clone, PartialEq)]
pub struct AppRun {
    pub include: Vec<Component>,
    pub links: Vec<String>,
    pub format: Format,
    /// `None`: the server's `backup.target`.
    pub target: Option<String>,
    pub keep: Option<u32>,
    pub volumes: Vec<String>,
    pub stop: Option<StopServices>,
    pub before_backup: Vec<Step>,
    pub after_backup: Vec<Step>,
}

/// What one backup of the shared apps writes and where it goes.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerRun {
    pub format: Format,
    pub target: Option<String>,
    pub keep: Option<u32>,
}

/// The profile `name` as the server defines it, if it does.
fn server_profile<'a>(settings: Option<&'a BackupSettings>, name: &str) -> Option<&'a Profile> {
    settings.and_then(|s| s.profiles.get(name))
}

/// How a profile applies to an app: `None` when the app opted out or neither
/// file defines it.
fn app_profile(settings: Option<&BackupSettings>, app: &AppConfig, name: &str) -> Option<Profile> {
    let server = server_profile(settings, name);
    match (server, app.backup.profiles.get(name)) {
        (_, Some(AppProfile::Enabled(false))) | (None, None) => None,
        (Some(server), None | Some(AppProfile::Enabled(true))) => Some(server.clone()),
        (None, Some(AppProfile::Enabled(true))) => Some(Profile::default()),
        (server, Some(AppProfile::Settings(own))) => {
            Some(own.over(&server.cloned().unwrap_or_default()))
        }
    }
}

/// Whether the profile `name` exists in either file.
pub fn defined(settings: Option<&BackupSettings>, apps: &[&AppConfig], name: &str) -> bool {
    server_profile(settings, name).is_some()
        || apps
            .iter()
            .any(|app| app.backup.profiles.contains_key(name))
}

/// The settings of an app backup with `flags`. `None`: the app opted out of
/// the profile, or doesn't have it.
pub fn app_run(
    settings: Option<&BackupSettings>,
    app: &AppConfig,
    flags: &Flags,
) -> Option<AppRun> {
    let profile = match &flags.profile {
        Some(name) => app_profile(settings, app, name)?,
        None => Profile::default(),
    };
    let backup = &app.backup;
    let links = if flags.links.is_empty() {
        profile.links.clone().unwrap_or_default()
    } else {
        flags.links.clone()
    };
    let include = match (&flags.include[..], &flags.links[..], &profile.include) {
        ([], [], Some(include)) => include.clone(),
        (include, _, _) => components(include, &links),
    };
    Some(AppRun {
        include,
        links,
        format: flags.format.or(profile.format).unwrap_or_default(),
        target: flags
            .target
            .clone()
            .or(profile.target)
            .or_else(|| backup.target.clone()),
        keep: profile.keep,
        volumes: profile.volumes.unwrap_or_else(|| backup.volumes.clone()),
        stop: profile.stop.or_else(|| backup.stop.clone()),
        before_backup: profile
            .before_backup
            .unwrap_or_else(|| backup.before_backup.clone()),
        after_backup: profile
            .after_backup
            .unwrap_or_else(|| backup.after_backup.clone()),
    })
}

/// The settings of a server backup with `flags`. `None`: the server doesn't
/// define the profile, or the profile is for apps only (`server: false`).
pub fn server_run(settings: Option<&BackupSettings>, flags: &Flags) -> Option<ServerRun> {
    let profile = match &flags.profile {
        Some(name) => server_profile(settings, name)?.clone(),
        None => Profile::default(),
    };
    if profile.server == Some(false) {
        return None;
    }
    Some(ServerRun {
        format: flags.format.or(profile.format).unwrap_or_default(),
        target: flags.target.clone().or(profile.target),
        keep: profile.keep,
    })
}

/// Error for a profile that neither file defines.
pub fn unknown_profile(name: &str) -> anyhow::Error {
    config_err(format!(
        "no backup profile `{name}` (define it under `backup.profiles` of compost-server.yaml or compost.yaml)"
    ))
}

/// Error for `server backup --profile` with a profile that doesn't save the server.
pub fn not_for_server(name: &str) -> anyhow::Error {
    config_err(format!(
        "backup profile `{name}` does not back up the shared apps (it is not in compost-server.yaml, or has `server: false`)"
    ))
}

/// Error for a check that a profile applies to an instance.
pub fn not_for_app(name: &str, instance: &str) -> anyhow::Error {
    config_err(format!(
        "backup profile `{name}` is not defined for {instance}, or it opted out (`backup.profiles.{name}: false`)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> BackupSettings {
        serde_yaml_ng::from_str(
            "{target: server, profiles: {
               daily: {include: [links], keep: 7},
               weekly: {keep: 4, target: local, format: raw},
               apps: {server: false}}}",
        )
        .unwrap()
    }

    fn app(text: &str) -> AppConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn profile(name: &str) -> Flags {
        Flags {
            profile: Some(name.into()),
            ..Flags::default()
        }
    }

    #[test]
    fn without_a_profile_the_app_settings_apply() {
        let app = app("{name: a, backup: {volumes: [up], target: nas}}");
        let run = app_run(Some(&settings()), &app, &Flags::default()).unwrap();
        use Component::*;
        assert_eq!(run.include, [Links, Sources, Volumes]);
        assert_eq!(run.volumes, ["up"]);
        assert_eq!(run.target.as_deref(), Some("nas"));
        assert_eq!(run.keep, None);
        let flags = Flags {
            links: vec!["db".into()],
            ..Flags::default()
        };
        assert_eq!(app_run(None, &app, &flags).unwrap().include, [Links]);
    }

    #[test]
    fn app_profiles_override_server_profiles() {
        let settings = settings();
        let app = app("{name: a, backup: {volumes: [up], target: nas, profiles: {
               daily: {keep: 2},
               weekly: false,
               anon: {include: [volumes], volumes: [anon], format: raw,
                      before_backup: [{run: anonymizer}]}}}}");
        let daily = app_run(Some(&settings), &app, &profile("daily")).unwrap();
        assert_eq!(daily.include, [Component::Links]);
        assert_eq!(daily.keep, Some(2));
        assert_eq!(daily.target.as_deref(), Some("nas"));
        assert_eq!(app_run(Some(&settings), &app, &profile("weekly")), None);
        assert_eq!(app_run(Some(&settings), &app, &profile("nope")), None);

        let anon = app_run(Some(&settings), &app, &profile("anon")).unwrap();
        assert_eq!(anon.include, [Component::Volumes]);
        assert_eq!(anon.volumes, ["anon"]);
        assert_eq!(anon.format, Format::Raw);
        assert_eq!(anon.before_backup.len(), 1);

        // Without an override, the server profile's target beats the app's default.
        let plain = self::app("{name: b, backup: {target: nas}}");
        let weekly = app_run(Some(&settings), &plain, &profile("weekly")).unwrap();
        assert_eq!(weekly.target.as_deref(), Some("local"));
        // Flags beat everything.
        let flags = Flags {
            target: Some("server".into()),
            format: Some(Format::Compost),
            ..profile("weekly")
        };
        let run = app_run(Some(&settings), &plain, &flags).unwrap();
        assert_eq!(run.target.as_deref(), Some("server"));
        assert_eq!(run.format, Format::Compost);
    }

    #[test]
    fn server_runs() {
        let settings = settings();
        let daily = server_run(Some(&settings), &profile("daily")).unwrap();
        assert_eq!(daily.keep, Some(7));
        assert_eq!(daily.target, None);
        assert_eq!(server_run(Some(&settings), &profile("apps")), None);
        assert_eq!(server_run(Some(&settings), &profile("nope")), None);
        assert!(server_run(None, &Flags::default()).is_some());
        let only_app = app("{name: a, backup: {profiles: {anon: {keep: 1}}}}");
        assert!(defined(Some(&settings), &[&only_app], "anon"));
        assert!(!defined(Some(&settings), &[&only_app], "nope"));
    }
}
