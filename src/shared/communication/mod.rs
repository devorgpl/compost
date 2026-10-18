//! Communication kind: channels that compost sends notifications to.
//!
//! A communication app runs nothing by itself. Its image implements a small
//! contract (API 1): `send --severity … --title … < body` and `describe`,
//! with the channel and its credentials in an env file. compost builds its
//! own image on the server from `images/communication/`; `image` replaces it
//! with any image that implements the contract.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_yaml_ng::Value;

use super::{
    AppCtx, Contribution, Generated, Kind, Rendered, RenderedFile, Resolution, ServerCtx,
    SharedAppConfig, shared_dir,
};
use crate::config::app::AppConfig;
use crate::error::config_err;

pub const KIND: &str = "communication";
/// Version of the contract between compost and the image.
pub const API: u32 = 1;
const DOCKERFILE: &str = include_str!("../../../images/communication/Dockerfile");
const SCRIPT: &str = include_str!("../../../images/communication/communicate");
/// Repository of the image compost builds.
const IMAGE_REPOSITORY: &str = "compost-communication";
const TYPES: [&str; 6] = [
    "webhook",
    "slack",
    "mattermost",
    "discord",
    "matrix",
    "email",
];

/// The env file the image runs with, relative to `COMPOST_SERVER_HOME`.
pub fn env_path(name: &str) -> String {
    format!("{}/secrets/communication.env", shared_dir(KIND, name))
}

/// The file with the image to run, relative to `COMPOST_SERVER_HOME`.
pub fn image_path(name: &str) -> String {
    format!("{}/image", shared_dir(KIND, name))
}

/// Folder of the image's build context, relative to `COMPOST_SERVER_HOME`.
pub fn build_dir(name: &str) -> String {
    format!("{}/build", shared_dir(KIND, name))
}

/// The tag of compost's own image: changes with its files.
pub fn builtin_image() -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{DOCKERFILE}\0{SCRIPT}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{IMAGE_REPOSITORY}:{}", &hex[..12])
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookConfig {
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatConfig {
    webhook_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixConfig {
    homeserver: String,
    room: String,
    access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailConfig {
    smtp_url: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    from: String,
    to: Vec<String>,
    #[serde(default = "yes")]
    require_tls: bool,
}

fn yes() -> bool {
    true
}

/// Secret keys by type. The stored config has none, so they are optional in
/// the config; the env file on the server keeps them.
fn secret_keys(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "webhook" => &["url", "headers"],
        "slack" | "mattermost" | "discord" => &["webhook_url"],
        "matrix" => &["access_token"],
        "email" => &["password"],
        _ => &[],
    }
}

fn parse<T: DeserializeOwned>(config: &SharedAppConfig) -> Result<T> {
    let value = if config.configuration.is_null() {
        Value::Mapping(serde_yaml_ng::Mapping::new())
    } else {
        config.configuration.clone()
    };
    serde_yaml_ng::from_value(value).map_err(|e| anyhow::anyhow!("configuration: {e}"))
}

fn check_url(key: &str, url: &str, schemes: &[&str]) -> Result<()> {
    ensure!(
        schemes.iter().any(|s| url.starts_with(&format!("{s}://"))),
        "{key} must start with {}",
        schemes
            .iter()
            .map(|s| format!("{s}://"))
            .collect::<Vec<_>>()
            .join(" or ")
    );
    Ok(())
}

/// The variables of the env file, `COMMUNICATION_*`. Secrets missing from the
/// config are `None`.
fn variables(config: &SharedAppConfig) -> Result<Vec<(String, Option<String>)>> {
    let var = |key: &str| format!("COMMUNICATION_{}", key.to_uppercase());
    let mut vars = vec![
        (var("api"), Some(API.to_string())),
        (var("type"), Some(config.type_name.clone())),
    ];
    match config.type_name.as_str() {
        "webhook" => {
            let c: WebhookConfig = parse(config)?;
            if let Some(url) = &c.url {
                check_url("url", url, &["http", "https"])?;
            }
            vars.push((var("url"), c.url));
            for (index, (name, value)) in c.headers.iter().enumerate() {
                ensure!(
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'),
                    "headers: `{name}` is not a header name"
                );
                vars.push((
                    var(&format!("header_{}", index + 1)),
                    Some(format!("{name}: {value}")),
                ));
            }
        }
        "slack" | "mattermost" | "discord" => {
            let c: ChatConfig = parse(config)?;
            if let Some(url) = &c.webhook_url {
                check_url("webhook_url", url, &["https", "http"])?;
            }
            vars.push((var("webhook_url"), c.webhook_url));
        }
        "matrix" => {
            let c: MatrixConfig = parse(config)?;
            check_url("homeserver", &c.homeserver, &["https", "http"])?;
            ensure!(
                c.room.starts_with('!') && c.room.contains(':'),
                "room must be a room ID like `!abc123:example.org` (Room settings → Advanced)"
            );
            vars.push((var("homeserver"), Some(c.homeserver)));
            vars.push((var("room"), Some(c.room)));
            vars.push((var("access_token"), c.access_token));
        }
        "email" => {
            let c: EmailConfig = parse(config)?;
            check_url("smtp_url", &c.smtp_url, &["smtp", "smtps"])?;
            ensure!(!c.to.is_empty(), "to: give at least one address");
            for address in std::iter::once(&c.from).chain(&c.to) {
                ensure!(
                    address.contains('@') && !address.contains([',', ' ']),
                    "`{address}` is not an email address"
                );
            }
            vars.push((var("smtp_url"), Some(c.smtp_url)));
            vars.push((var("from"), Some(c.from)));
            vars.push((var("to"), Some(c.to.join(","))));
            vars.push((var("require_tls"), Some(c.require_tls.to_string())));
            if let Some(user) = c.username {
                vars.push((var("username"), Some(user)));
                vars.push((var("password"), c.password));
            }
        }
        other => bail!(
            "unknown communication type `{other}` (known: {})",
            TYPES.join(", ")
        ),
    }
    for (name, value) in &vars {
        if let Some(value) = value {
            ensure!(
                !value.contains(['\n', '\r']),
                "{name}: values must be on one line"
            );
        }
    }
    Ok(vars)
}

/// `NAME=value` lines of an env file.
fn read_env(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn generated(type_name: &str) -> Generated {
    let (local, secrets): (&'static str, &'static str) = match type_name {
        "webhook" => (
            "    # Notifications as JSON to any URL (see Notifications in the docs).\n    - name: ops-webhook\n      type: webhook\n",
            "    - name: ops-webhook\n      configuration:\n        url: https://hooks.example.com/compost\n        headers: {Authorization: Bearer change-me}\n",
        ),
        "slack" => (
            "    # Notifications to a Slack channel, through an incoming webhook.\n    - name: ops-slack\n      type: slack\n",
            "    - name: ops-slack\n      configuration:\n        webhook_url: https://hooks.slack.com/services/T000/B000/change-me\n",
        ),
        "mattermost" => (
            "    # Notifications to a Mattermost channel, through an incoming webhook.\n    - name: ops-mattermost\n      type: mattermost\n",
            "    - name: ops-mattermost\n      configuration:\n        webhook_url: https://mattermost.example.com/hooks/change-me\n",
        ),
        "discord" => (
            "    # Notifications to a Discord channel, through a webhook.\n    - name: ops-discord\n      type: discord\n",
            "    - name: ops-discord\n      configuration:\n        webhook_url: https://discord.com/api/webhooks/000/change-me\n",
        ),
        "matrix" => (
            "    # Notifications to a Matrix room. The account must have joined the room.\n    - name: ops-matrix\n      type: matrix\n      configuration:\n        homeserver: https://matrix.example.org\n        room: \"!change-me:example.org\"\n",
            "    - name: ops-matrix\n      configuration:\n        access_token: change-me\n",
        ),
        _ => (
            "    # Notifications by email, through an SMTP server.\n    - name: ops-mail\n      type: email\n      configuration:\n        smtp_url: smtps://smtp.example.com:465\n        username: alerts@example.com\n        from: alerts@example.com\n        to: [ops@example.com]\n",
            "    - name: ops-mail\n      configuration:\n        password: change-me\n",
        ),
    };
    Generated {
        local,
        prod: None,
        secrets: Some(secrets),
        notes: &[
            "communication: add `notifications: {send_to: [<name>]}` to compost-server.yaml, deploy, then try it with `compost notify test`.",
        ],
        requires: &[],
    }
}

pub struct CommunicationKind;

impl Kind for CommunicationKind {
    fn name(&self) -> &'static str {
        KIND
    }

    fn type_names(&self) -> Vec<&'static str> {
        TYPES.to_vec()
    }

    fn generate(&self, type_name: &str) -> Result<Generated> {
        ensure!(
            TYPES.contains(&type_name),
            "unknown communication type `{type_name}`"
        );
        Ok(generated(type_name))
    }

    fn apply_defaults(&self, _config: &mut SharedAppConfig) {}

    fn validate_shared(&self, config: &SharedAppConfig) -> Result<()> {
        ensure!(
            config.monitoring.is_none(),
            "`monitoring` is not supported for communication apps: they run only to send"
        );
        if let Some(image) = &config.image {
            ensure!(!image.trim().is_empty(), "image must not be empty");
        }
        variables(config).map(|_| ())
    }

    fn secret_keys(&self, config: &SharedAppConfig) -> Result<&'static [&'static str]> {
        Ok(secret_keys(&config.type_name))
    }

    fn runs_service(&self, _config: &SharedAppConfig) -> bool {
        false
    }

    /// The env file, and compost's image's build context unless `image` replaces it.
    /// Secrets the config leaves out are kept from the env file on the server.
    fn render(&self, ctx: &ServerCtx, config: &SharedAppConfig) -> Result<Rendered> {
        let existing = read_env(&ctx.read(&env_path(&config.name))?.unwrap_or_default());
        let mut env = String::new();
        for (name, value) in variables(config)? {
            let value = value
                .or_else(|| existing.get(&name).cloned())
                .ok_or_else(|| {
                    let key = name
                        .strip_prefix("COMMUNICATION_")
                        .unwrap_or(&name)
                        .to_lowercase();
                    config_err(format!(
                        "`{key}` is needed; put it in the secrets file (.compost-server.secrets.yaml)"
                    ))
                })?;
            env.push_str(&format!("{name}={value}\n"));
        }
        let mut files = vec![RenderedFile::secret(env_path(&config.name), env)];
        let image = match &config.image {
            Some(image) => image.clone(),
            None => {
                files.push(RenderedFile::new(
                    format!("{}/Dockerfile", build_dir(&config.name)),
                    DOCKERFILE.to_string(),
                ));
                files.push(RenderedFile::new(
                    format!("{}/communicate", build_dir(&config.name)),
                    SCRIPT.to_string(),
                ));
                builtin_image()
            }
        };
        files.push(RenderedFile::new(
            image_path(&config.name),
            format!("{image}\n"),
        ));
        Ok(Rendered {
            files,
            dirs: vec![format!("{}/secrets", shared_dir(KIND, &config.name))],
            owned_dirs: vec![build_dir(&config.name)],
            ..Rendered::default()
        })
    }

    fn apply_order(&self) -> i32 {
        5 // first: nothing depends on it, and it needs nothing
    }

    // Apps don't link to communication apps.

    fn validate_link(&self, _link: &Value, _app: &AppConfig) -> Result<()> {
        bail!("apps can't link to communication apps; they are for compost's notifications")
    }

    fn resolve<'s>(
        &self,
        _link: &Value,
        _available: &'s [SharedAppConfig],
    ) -> Result<Resolution<'s>> {
        bail!("apps can't link to communication apps")
    }

    fn prepare_application(
        &self,
        _ctx: &AppCtx,
        _link: &Value,
        _out: &mut Contribution,
    ) -> Result<()> {
        bail!("apps can't link to communication apps")
    }

    fn before_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn after_apply_application(&self, _ctx: &AppCtx, _link: &Value) -> Result<()> {
        Ok(())
    }

    fn describe_link(&self, _ctx: &AppCtx, _entry: &str) -> Result<String> {
        bail!("apps can't link to communication apps")
    }

    fn remove_application(&self, _ctx: &AppCtx, _entry: &str, _purge: bool) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;

    fn config(text: &str) -> SharedAppConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn render(home: &tempfile::TempDir, text: &str) -> Result<Rendered> {
        let connector = Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: home.path().display().to_string(),
        };
        CommunicationKind.render(&ctx, &config(text))
    }

    fn file<'a>(rendered: &'a Rendered, suffix: &str) -> &'a RenderedFile {
        rendered
            .files
            .iter()
            .find(|f| f.path.ends_with(suffix))
            .unwrap()
    }

    #[test]
    fn validation() {
        let err = |text: &str| {
            format!(
                "{:#}",
                CommunicationKind
                    .validate_shared(&config(text))
                    .unwrap_err()
            )
        };
        assert!(CommunicationKind
            .validate_shared(&config("{name: m, type: matrix, configuration: {homeserver: 'https://m.org', room: '!a:m.org'}}"))
            .is_ok());
        assert!(err("{name: m, type: matrix, configuration: {homeserver: 'https://m.org', room: '#a:m.org'}}").contains("room ID"));
        assert!(
            err(
                "{name: m, type: email, configuration: {smtp_url: 'mail:25', from: a@b, to: [c@d]}}"
            )
            .contains("smtp://")
        );
        assert!(err("{name: m, type: email, configuration: {smtp_url: 'smtp://mail', from: a@b, to: []}}").contains("at least one"));
        assert!(
            err("{name: m, type: slack, configuration: {url: x}}").contains("unknown field `url`")
        );
        assert!(err("{name: m, type: sms}").contains("unknown communication type `sms`"));
        assert!(
            err("{name: m, type: webhook, configuration: {url: \"https://x\\ny\"}}")
                .contains("one line")
        );
    }

    #[test]
    fn renders_env_file_and_keeps_secrets_from_the_server() {
        let home = tempfile::TempDir::new().unwrap();
        let full = "{name: chat, type: matrix, configuration: {homeserver: 'https://m.org', room: '!a:m.org', access_token: TOKEN}}";
        let rendered = render(&home, full).unwrap();
        let env = file(&rendered, "secrets/communication.env");
        assert!(env.is_secret());
        assert_eq!(
            env.contents,
            "COMMUNICATION_API=1\nCOMMUNICATION_TYPE=matrix\nCOMMUNICATION_HOMESERVER=https://m.org\nCOMMUNICATION_ROOM=!a:m.org\nCOMMUNICATION_ACCESS_TOKEN=TOKEN\n"
        );
        assert_eq!(
            file(&rendered, "/image").contents,
            format!("{}\n", builtin_image())
        );
        assert!(
            file(&rendered, "build/communicate")
                .contents
                .contains("API=1")
        );

        // Without the secret: an error, until the env file is on the server.
        let stripped = "{name: chat, type: matrix, configuration: {homeserver: 'https://m.org', room: '!a:m.org'}}";
        let err = render(&home, stripped).unwrap_err();
        assert!(
            format!("{err:#}").contains("`access_token` is needed"),
            "{err:#}"
        );
        let path = home.path().join(env_path("chat"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &env.contents).unwrap();
        let again = render(&home, stripped).unwrap();
        assert_eq!(
            file(&again, "secrets/communication.env").contents,
            env.contents
        );
    }

    #[test]
    fn custom_image_has_no_build_context() {
        let home = tempfile::TempDir::new().unwrap();
        let rendered = render(
            &home,
            "{name: hook, type: webhook, image: 'me/notify:2', configuration: {url: 'https://h', headers: {X-Key: k}}}",
        )
        .unwrap();
        assert!(rendered.files.iter().all(|f| !f.path.contains("/build/")));
        assert_eq!(file(&rendered, "/image").contents, "me/notify:2\n");
        assert!(
            file(&rendered, "communication.env")
                .contents
                .contains("COMMUNICATION_HEADER_1=X-Key: k\n")
        );
    }
}
