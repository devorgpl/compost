//! Collect and pack the files uploaded to `apps/<app>/<prefix>/sources/` (spec §10.8).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::config::app::{Deployment, LoadedApp, OVERRIDES_DIR};

/// Patterns in this file (gitignore syntax) are not uploaded.
const IGNORE_FILE: &str = ".compostignore";

/// A local file and its path in the uploaded sources.
pub struct Upload {
    pub from: PathBuf,
    pub name: PathBuf,
}

/// The files of the sources folder, then the compose overrides.
pub fn collect(loaded: &LoadedApp, config_files: &[PathBuf]) -> Result<Vec<Upload>> {
    let source_dir = &loaded.source_dir;
    let mut uploads: Vec<Upload> =
        source_files(source_dir, &loaded.config.deployment, config_files)?
            .into_iter()
            .map(|name| Upload {
                from: source_dir.join(&name),
                name,
            })
            .collect();
    for (from, name) in &loaded.overrides {
        uploads.push(Upload {
            from: from.clone(),
            name: PathBuf::from(name),
        });
    }
    Ok(uploads)
}

/// Paths relative to `source_dir`, sorted. Without `attach_sources`/`build`
/// only the compose file is uploaded.
fn source_files(
    source_dir: &Path,
    deployment: &Deployment,
    config_files: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    if !deployment.uploads_sources() {
        return Ok(vec![PathBuf::from(&deployment.compose_file)]);
    }

    // Config files (overlays hold secrets) are never uploaded.
    let excluded: Vec<PathBuf> = config_files
        .iter()
        .filter_map(|f| f.canonicalize().ok())
        .collect();

    let mut files = Vec::new();
    let walker = WalkBuilder::new(source_dir)
        .standard_filters(false) // no .gitignore: built files may be needed
        .add_custom_ignore_filename(IGNORE_FILE)
        // `.compost/` holds the overrides, which are added from where they are.
        .filter_entry(|entry| {
            entry.file_name() != ".git"
                && !(entry.depth() == 1 && entry.file_name() == OVERRIDES_DIR)
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let is_file = entry
            .file_type()
            .is_some_and(|t| t.is_file() || t.is_symlink());
        if !is_file {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        // `.compost-staging.yaml`, `.compost.secrets.yaml`, …
        let is_overlay = (name.starts_with(".compost-") || name.starts_with(".compost."))
            && name.ends_with(".yaml");
        if is_overlay || path.canonicalize().is_ok_and(|p| excluded.contains(&p)) {
            continue;
        }
        files.push(path.strip_prefix(source_dir)?.to_path_buf());
    }
    files.sort();
    Ok(files)
}

/// An uncompressed tar archive of the given files.
pub fn pack(uploads: &[Upload]) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    for upload in uploads {
        builder
            .append_path_with_name(&upload.from, &upload.name)
            .with_context(|| format!("cannot pack {}", upload.from.display()))?;
    }
    Ok(builder.into_inner()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, path: &str) {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn excludes_git_overlays_config_files_and_ignored() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        for file in [
            "compost.yaml",
            "docker-compose.yaml",
            "src/main.rs",
            "dist/app.js",
            ".git/config",
            ".compost-staging.yaml",
            ".compost.secrets.yaml",
            ".compost/compose-override-1.yaml",
            "secrets/prod.yaml",
            "node_modules/x.js",
        ] {
            write(base, file);
        }
        fs::write(base.join(".compostignore"), "node_modules/\n").unwrap();
        fs::write(base.join(".gitignore"), "dist/\n").unwrap();

        let deployment = Deployment {
            build: true,
            ..Deployment::default()
        };
        let config_files = [base.join("compost.yaml"), base.join("secrets/prod.yaml")];
        let files = source_files(base, &deployment, &config_files).unwrap();
        let files: Vec<_> = files.iter().map(|f| f.to_str().unwrap()).collect();
        assert_eq!(
            files,
            [
                ".compostignore",
                ".gitignore",
                "dist/app.js",
                "docker-compose.yaml",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn only_compose_file_without_sources() {
        let files = source_files(Path::new("."), &Deployment::default(), &[]).unwrap();
        assert_eq!(files, [PathBuf::from("docker-compose.yaml")]);
    }
}
