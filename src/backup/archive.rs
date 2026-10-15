//! `manifest.yaml`: the first entry of every depap archive.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

pub const MANIFEST_FILE: &str = "manifest.yaml";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Version of the archive layout.
    pub format: u32,
    pub scope: Scope,
    pub depap_version: String,
    pub created_at: String,
    /// Name of the server the backup was taken from.
    pub server: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<InstanceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<SharedRef>,
    /// Every file in the archive except the manifest.
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Shared apps (`server backup`).
    Server,
    /// One application instance.
    App,
    /// Every application instance (`app backup --all`).
    Apps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRef {
    pub name: String,
    pub prefix: String,
}

impl InstanceRef {
    /// `<app>-<prefix>`
    pub fn instance(&self) -> String {
        format!("{}-{}", self.name, self.prefix)
    }

    /// Folder of the instance inside the archive.
    pub fn dir(&self) -> String {
        format!("apps/{}/{}", self.name, self.prefix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedRef {
    pub kind: String,
    pub name: String,
}

impl SharedRef {
    /// Folder of the shared app inside the archive.
    pub fn dir(&self) -> String {
        format!("server/shared/{}/{}", self.kind, self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub path: String,
    pub size: u64,
}

impl Manifest {
    pub fn new(scope: Scope, server: &str) -> Self {
        Self {
            format: FORMAT_VERSION,
            scope,
            depap_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            server: server.to_string(),
            instances: Vec::new(),
            shared: Vec::new(),
            items: Vec::new(),
        }
    }

    pub fn has(&self, path: &str) -> bool {
        self.items.iter().any(|item| item.path == path)
    }

    /// Items directly or indirectly below `dir`.
    pub fn items_under<'a>(&'a self, dir: &'a str) -> impl Iterator<Item = &'a Item> {
        self.items.iter().filter(move |item| {
            item.path
                .strip_prefix(dir)
                .is_some_and(|r| r.starts_with('/'))
        })
    }

    pub fn check_format(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format == FORMAT_VERSION,
            "archive format {} is not supported by this depap (supports {FORMAT_VERSION}); upgrade depap",
            self.format
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut manifest = Manifest::new(Scope::App, "prod1");
        manifest.instances.push(InstanceRef {
            name: "myapp".into(),
            prefix: "staging".into(),
        });
        manifest.items.push(Item {
            path: "apps/myapp/staging/sources.tar".into(),
            size: 10,
        });
        let text = serde_yaml_ng::to_string(&manifest).unwrap();
        assert!(text.contains("scope: app"), "{text}");
        let back: Manifest = serde_yaml_ng::from_str(&text).unwrap();
        back.check_format().unwrap();
        assert!(back.has("apps/myapp/staging/sources.tar"));
        assert_eq!(back.items_under("apps/myapp/staging").count(), 1);
        assert_eq!(back.items_under("apps/myapp/stag").count(), 0);
    }
}
