//! Targets outside the server (S3), reached with rclone in a container on the
//! server. Archives are packed on the server as for the `server` target, then
//! copied; credentials go to rclone in an env file, never on a command line.

use std::fmt;

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use crate::config::server::TargetConfig;
use crate::connector::{Connector, shell_quote};
use crate::error::config_err;
use crate::server::secrets as server_secrets;
use crate::shared::ServerCtx;

/// The rclone image, unless the target sets `image`.
pub const RCLONE_IMAGE: &str = "rclone/rclone:1.68.2";
/// Name of the remote in rclone's configuration (`RCLONE_CONFIG_<NAME>_…`).
const REMOTE: &str = "remote";

/// A remote target, ready to use: configuration with its credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Remote {
    /// The name in `backup.targets`.
    pub name: String,
    /// `<bucket>/<path>`, without a trailing slash.
    location: String,
    image: String,
    /// rclone's settings, e.g. `RCLONE_CONFIG_REMOTE_TYPE=s3`.
    env: Vec<(String, String)>,
    /// Absolute folder for short-lived files on the server.
    tmp: String,
}

// By hand: the credentials must not show up in debug output.
impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote")
            .field("name", &self.name)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (s3://{})", self.name, self.location)
    }
}

impl Remote {
    /// The target `name`. Credentials missing from the config (the stored one
    /// has none) come from the secrets stored with `server deploy --secret`.
    pub fn new(ctx: &ServerCtx, name: &str, target: &TargetConfig) -> Result<Self> {
        let TargetConfig::S3 {
            bucket,
            path,
            endpoint,
            region,
            provider,
            access_key_id,
            secret_access_key,
            image,
        } = target
        else {
            anyhow::bail!("backup target `{name}` is not a remote target");
        };
        let stored = match (access_key_id, secret_access_key) {
            (Some(_), Some(_)) => None,
            _ => stored_target(ctx, name)?,
        };
        let from_stored = |key: &str| {
            stored
                .as_ref()
                .and_then(|t| t.get(key))
                .and_then(Value::as_str)
                .map(String::from)
        };
        let key_id = access_key_id
            .clone()
            .or_else(|| from_stored("access_key_id"));
        let secret = secret_access_key
            .clone()
            .or_else(|| from_stored("secret_access_key"));
        let (Some(key_id), Some(secret)) = (key_id, secret) else {
            return Err(config_err(format!(
                "backup target `{name}`: access_key_id and secret_access_key are not stored on the server; \
                 put them in the secrets file and run `compost server deploy --secret <file>`"
            )));
        };

        let var = |key: &str| format!("RCLONE_CONFIG_{}_{key}", REMOTE.to_uppercase());
        let mut env = vec![
            (var("TYPE"), "s3".to_string()),
            (
                var("PROVIDER"),
                provider.clone().unwrap_or_else(|| "Other".into()),
            ),
            (var("ACCESS_KEY_ID"), key_id),
            (var("SECRET_ACCESS_KEY"), secret),
            // Uploads must not need the right to create buckets.
            (var("NO_CHECK_BUCKET"), "true".to_string()),
        ];
        if let Some(endpoint) = endpoint {
            env.push((var("ENDPOINT"), endpoint.clone()));
        }
        if let Some(region) = region {
            env.push((var("REGION"), region.clone()));
        }
        // No config file (empty: rclone doesn't look for one or warn): all is in the environment.
        env.push(("RCLONE_CONFIG".into(), String::new()));
        env.push(("RCLONE_CACHE_DIR".into(), "/tmp/rclone".into()));

        let path = path.as_deref().unwrap_or_default().trim_matches('/');
        let location = if path.is_empty() {
            bucket.clone()
        } else {
            format!("{bucket}/{path}")
        };
        Ok(Self {
            name: name.to_string(),
            location,
            image: image.clone().unwrap_or_else(|| RCLONE_IMAGE.into()),
            env,
            tmp: ctx.path("tmp"),
        })
    }

    /// rclone's name of the file `name` at the target.
    fn url(&self, name: &str) -> String {
        format!("{REMOTE}:{}/{name}", self.location)
    }

    /// Run rclone with `args` (already quoted), with `mount` as `/data`.
    fn rclone(&self, connector: &dyn Connector, mount: Option<&str>, args: &str) -> Result<String> {
        let env_file = format!("{}/rclone-{}.env", self.tmp, uuid::Uuid::new_v4().simple());
        let env: String = self
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        connector.exec(&format!(
            "umask 077 && mkdir -p -- {}",
            shell_quote(&self.tmp)
        ))?;
        connector.write_file(&env_file, env.as_bytes(), 0o600)?;
        let mount = mount
            .map(|dir| format!(" -v {}:/data", shell_quote(dir)))
            .unwrap_or_default();
        let env_file = shell_quote(&env_file);
        let output = connector.exec(&format!(
            "docker run --rm --user \"$(id -u):$(id -g)\" --env-file {env_file}{mount} {} {args}; \
             status=$?; rm -f -- {env_file}; exit $status",
            shell_quote(&self.image)
        ));
        output.with_context(|| format!("rclone failed for backup target {self}"))
    }

    /// Copy the file at `path` on the server to the target, under its own name.
    pub fn upload(&self, connector: &dyn Connector, path: &str) -> Result<()> {
        let (dir, name) = path.rsplit_once('/').context("absolute path expected")?;
        self.rclone(
            connector,
            Some(dir),
            &format!(
                "copyto {} {}",
                shell_quote(&format!("/data/{name}")),
                shell_quote(&self.url(name))
            ),
        )?;
        Ok(())
    }

    /// Copy the archive `name` from the target into a new folder on the
    /// server; returns the folder and the file's path. The caller removes the folder.
    pub fn download(&self, connector: &dyn Connector, name: &str) -> Result<(String, String)> {
        if name.contains('/') {
            return Err(config_err(format!(
                "`{name}`: with the target {}, the archive is a file name there",
                self.name
            )));
        }
        let dir = format!("{}/download-{}", self.tmp, uuid::Uuid::new_v4().simple());
        connector.exec(&format!("umask 077 && mkdir -p -- {}", shell_quote(&dir)))?;
        let copied = self.rclone(
            connector,
            Some(&dir),
            &format!(
                "copyto {} {}",
                shell_quote(&self.url(name)),
                shell_quote(&format!("/data/{name}"))
            ),
        );
        let path = format!("{dir}/{name}");
        let exists = copied.and_then(|_| {
            connector.exec(&format!(
                "[ -f {} ] || {{ echo 'no such archive' >&2; exit 1; }}",
                shell_quote(&path)
            ))
        });
        if let Err(err) = exists {
            let _ = connector.exec(&format!("rm -rf -- {}", shell_quote(&dir)));
            return Err(err.context(format!("cannot get {name} from backup target {self}")));
        }
        Ok((dir, path))
    }

    /// Files at the target with their sizes.
    pub fn list(&self, connector: &dyn Connector) -> Result<Vec<(String, u64)>> {
        let listing = self.rclone(
            connector,
            None,
            &format!(
                "lsf --files-only --format sp --separator '\t' {}",
                shell_quote(&self.url(""))
            ),
        )?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let (size, name) = line.split_once('\t')?;
                Some((name.to_string(), size.trim().parse().ok()?))
            })
            .collect())
    }

    pub fn delete(&self, connector: &dyn Connector, name: &str) -> Result<()> {
        self.rclone(
            connector,
            None,
            &format!("deletefile {}", shell_quote(&self.url(name))),
        )?;
        Ok(())
    }
}

/// `backup.targets.<name>` of the secrets stored with `server deploy --secret`.
fn stored_target(ctx: &ServerCtx, name: &str) -> Result<Option<Value>> {
    let Some(text) = ctx.read(server_secrets::PATH)? else {
        return Ok(None);
    };
    let value: Value = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("invalid {} on the server", server_secrets::PATH))?;
    Ok(value
        .get("backup")
        .and_then(|b| b.get("targets"))
        .and_then(|t| t.get(name))
        .cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::local::Local;

    fn target(text: &str) -> TargetConfig {
        serde_yaml_ng::from_str(text).unwrap()
    }

    #[test]
    fn configuration_and_stored_credentials() {
        let home = tempfile::TempDir::new().unwrap();
        let connector = Local { verbose: false };
        let ctx = ServerCtx {
            connector: &connector,
            home: home.path().display().to_string(),
        };
        let config = target(
            "{type: s3, bucket: b, path: /prod/, endpoint: 'https://s3.example.com', region: eu}",
        );
        let err = Remote::new(&ctx, "offsite", &config).unwrap_err();
        assert!(err.to_string().contains("server deploy --secret"), "{err}");

        std::fs::create_dir_all(home.path().join("secrets")).unwrap();
        std::fs::write(
            home.path().join(server_secrets::PATH),
            "backup: {targets: {offsite: {access_key_id: AK, secret_access_key: 's3cr3t'}}}",
        )
        .unwrap();
        let remote = Remote::new(&ctx, "offsite", &config).unwrap();
        assert_eq!(remote.url("a.tar.gz"), "remote:b/prod/a.tar.gz");
        assert_eq!(remote.to_string(), "offsite (s3://b/prod)");
        let env = |key: &str| {
            remote
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            env("RCLONE_CONFIG_REMOTE_SECRET_ACCESS_KEY"),
            Some("s3cr3t")
        );
        assert_eq!(env("RCLONE_CONFIG_REMOTE_PROVIDER"), Some("Other"));
        assert_eq!(
            env("RCLONE_CONFIG_REMOTE_ENDPOINT"),
            Some("https://s3.example.com")
        );
        assert!(!format!("{remote:?}").contains("s3cr3t"));
    }
}
