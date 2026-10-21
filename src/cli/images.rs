//! `compost images`: the images and modules this compost uses by default.

use anyhow::{Context, Result};
use clap::Args;

use crate::backup::remote::RCLONE_IMAGE;
use crate::shared::{self, Bundled};

#[derive(Debug, Args)]
pub struct ImagesArgs {
    /// Print a Markdown table, as on the wiki page Bundled-Images
    #[arg(long, conflicts_with = "tsv")]
    pub markdown: bool,
    /// One line per image or module: what, reference, tag (for scripts/outdated.sh)
    #[arg(long, hide = true)]
    pub tsv: bool,
}

/// Images compost runs itself, outside any shared app.
fn tools() -> Vec<Bundled> {
    vec![
        Bundled::new(
            "rclone",
            RCLONE_IMAGE,
            "backups to S3 and other targets; a target's `image`",
        ),
        Bundled::new(
            "volume helper",
            crate::app::volumes::HELPER_IMAGE,
            "copies app volumes in backups and restores",
        ),
        Bundled::new(
            "cleanup",
            crate::server::teardown::CLEANUP_IMAGE,
            "`dev reset-all` deletes files of other users with it",
        ),
    ]
}

/// `kind/type` and its bundled images, every type in order, then compost's
/// tools. Types of a kind with the same images are one group, `kind/a, b`.
fn groups() -> Result<Vec<(String, Vec<Bundled>)>> {
    let mut groups = Vec::new();
    for kind_name in shared::kind_names() {
        let kind = shared::find_kind(kind_name).context("kind names are registered")?;
        let mut types: Vec<(Vec<&str>, Vec<Bundled>)> = Vec::new();
        for type_name in kind.type_names() {
            let bundled = kind.bundled(type_name)?;
            match types.iter_mut().find(|(_, b)| *b == bundled) {
                Some((names, _)) => names.push(type_name),
                None => types.push((vec![type_name], bundled)),
            }
        }
        for (names, bundled) in types {
            groups.push((format!("{kind_name}/{}", names.join(", ")), bundled));
        }
    }
    groups.push(("compost".to_string(), tools()));
    Ok(groups)
}

fn tag(bundled: &Bundled) -> &'static str {
    if bundled.what == "Go module" {
        "pinned"
    } else if bundled.reference.starts_with("compost-") {
        "built"
    } else if bundled.moves() {
        "moving tag"
    } else {
        "exact tag"
    }
}

pub fn run(args: ImagesArgs) -> Result<()> {
    let groups = groups()?;
    if args.tsv {
        for (_, bundled) in &groups {
            for b in bundled {
                out!("{}\t{}\t{}", b.what, b.reference, tag(b));
            }
        }
        return Ok(());
    }
    if args.markdown {
        out!("| Type | What | Image or module | Tag | Note |");
        out!("|---|---|---|---|---|");
        for (group, bundled) in &groups {
            for b in bundled {
                out!(
                    "| `{group}` | {} | `{}` | {} | {} |",
                    b.what,
                    b.reference,
                    tag(b),
                    b.note
                );
            }
        }
        return Ok(());
    }
    let width = groups
        .iter()
        .flat_map(|(_, bundled)| bundled)
        .map(|b| b.reference.len())
        .max()
        .unwrap_or(0);
    out!(
        "compost {} uses these unless compost-server.yaml names others:",
        env!("CARGO_PKG_VERSION")
    );
    for (group, bundled) in &groups {
        out!();
        out!("{group}");
        for b in bundled {
            let line = format!(
                "  {:<13} {:<width$}  {:<10}  {}",
                b.what,
                b.reference,
                tag(b),
                b.note
            );
            out!("{}", line.trim_end());
        }
    }
    out!();
    out!("A moving tag gets newer images: `server deploy --apply --pull` fetches them.");
    out!("The others change with a compost release.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_has_images() {
        for (group, bundled) in groups().unwrap() {
            assert!(
                group == "storage/s3"
                    || group == "compost"
                    || bundled
                        .iter()
                        .any(|b| b.what == "image" || b.what == "build base"),
                "{group}: {bundled:?}"
            );
        }
    }

    #[test]
    fn tells_moving_tags() {
        let moves = |reference: &str| Bundled::new("image", reference, "").moves();
        assert!(moves("caddy:2-alpine"));
        assert!(moves("postgres:<version>"));
        assert!(moves("quay.io/keycloak/keycloak:26.6"));
        assert!(moves("busybox:stable"));
        assert!(moves("localhost:5000/app"));
        assert!(!moves("crowdsecurity/crowdsec:v1.8.1"));
        assert!(!moves("temporalio/ui:2.56.0"));
        assert!(!moves("postgres:18@sha256:0123"));
    }
}
