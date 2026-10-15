//! Collect and pack the files uploaded to `apps/<app>/<prefix>/sources/` (spec §10.8).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::config::app::Deployment;

/// Patterns in this file (gitignore syntax) are not uploaded.
const IGNORE_FILE: &str = ".depapignore";

/// Paths relative to `base_dir`, sorted. Without `attach_sources`/`build`
/// only the compose file is uploaded.
pub fn collect(
    base_dir: &Path,
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
    let walker = WalkBuilder::new(base_dir)
        .standard_filters(false) // no .gitignore: built files may be needed
        .add_custom_ignore_filename(IGNORE_FILE)
        .filter_entry(|entry| entry.file_name() != ".git")
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
        let is_overlay = name.starts_with(".depap-") && name.ends_with(".yaml");
        if is_overlay || path.canonicalize().is_ok_and(|p| excluded.contains(&p)) {
            continue;
        }
        files.push(path.strip_prefix(base_dir)?.to_path_buf());
    }
    files.sort();
    Ok(files)
}

/// An uncompressed tar archive of the given files.
pub fn pack(base_dir: &Path, files: &[PathBuf]) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    for file in files {
        builder
            .append_path_with_name(base_dir.join(file), file)
            .with_context(|| format!("cannot pack {}", file.display()))?;
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
            "depap.yaml",
            "docker-compose.yaml",
            "src/main.rs",
            "dist/app.js",
            ".git/config",
            ".depap-staging.yaml",
            "secrets/prod.yaml",
            "node_modules/x.js",
        ] {
            write(base, file);
        }
        fs::write(base.join(".depapignore"), "node_modules/\n").unwrap();
        fs::write(base.join(".gitignore"), "dist/\n").unwrap();

        let deployment = Deployment {
            build: true,
            ..Deployment::default()
        };
        let config_files = [base.join("depap.yaml"), base.join("secrets/prod.yaml")];
        let files = collect(base, &deployment, &config_files).unwrap();
        let files: Vec<_> = files.iter().map(|f| f.to_str().unwrap()).collect();
        assert_eq!(
            files,
            [
                ".depapignore",
                ".gitignore",
                "dist/app.js",
                "docker-compose.yaml",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn only_compose_file_without_sources() {
        let files = collect(Path::new("."), &Deployment::default(), &[]).unwrap();
        assert_eq!(files, [PathBuf::from("docker-compose.yaml")]);
    }
}
