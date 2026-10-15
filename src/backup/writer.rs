//! Writing a backup: items are collected in a staging folder (on this machine
//! or on the server, depending on the target), then packed into one file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use tempfile::TempDir;

use super::archive::{Item, MANIFEST_FILE, Manifest};
use super::{Format, Role};
use crate::connector::{Connector, shell_quote};

/// Where the finished backup goes.
pub enum Output {
    /// A file on this machine: the `-o` path (a file, or a folder for the
    /// default name), else the default name in the current folder.
    Local(Option<PathBuf>),
    /// A file in the server's archive folder, with the `-o` name or the default one.
    Server { dir: String, name: Option<String> },
}

enum Staging {
    Local {
        dir: TempDir,
        output: Option<PathBuf>,
    },
    Server {
        dir: String,
        output_dir: String,
        name: Option<String>,
    },
}

pub struct Backup<'c> {
    connector: &'c dyn Connector,
    format: Format,
    manifest: Manifest,
    staging: Staging,
}

impl<'c> Backup<'c> {
    pub fn new(
        connector: &'c dyn Connector,
        format: Format,
        manifest: Manifest,
        output: Output,
    ) -> Result<Self> {
        let staging = match output {
            Output::Local(output) => Staging::Local {
                dir: tempfile::Builder::new().prefix("depap-backup-").tempdir()?,
                output,
            },
            Output::Server {
                dir: output_dir,
                name,
            } => {
                if let Some(name) = &name {
                    if name.contains('/') || name.starts_with('.') {
                        bail!(
                            "`{name}`: with the server target, -o is a file name in {output_dir}"
                        );
                    }
                }
                let dir = format!("{output_dir}/.partial-{}", uuid::Uuid::new_v4().simple());
                connector.exec(&format!(
                    "umask 077 && mkdir -p -- {} && mkdir -- {}",
                    shell_quote(&output_dir),
                    shell_quote(&dir)
                ))?;
                Staging::Server {
                    dir,
                    output_dir,
                    name,
                }
            }
        };
        Ok(Self {
            connector,
            format,
            manifest,
            staging,
        })
    }

    pub fn manifest_mut(&mut self) -> &mut Manifest {
        &mut self.manifest
    }

    /// Run `script` on the server; its stdout becomes the item at `path`.
    pub fn stream(&mut self, path: &str, role: Role, script: &str) -> Result<()> {
        if self.skips(role) {
            return Ok(());
        }
        let size = match &self.staging {
            Staging::Local { dir, .. } => {
                let file = create_parent(&dir.path().join(path))?;
                let mut writer = BufWriter::new(file);
                self.connector
                    .stream_out(script, &mut writer)
                    .with_context(|| format!("backing up {path}"))?;
                writer
                    .into_inner()
                    .map_err(|e| e.into_error())?
                    .metadata()?
                    .len()
            }
            Staging::Server { dir, .. } => {
                let file = shell_quote(&format!("{dir}/{path}"));
                let size = self
                    .connector
                    .exec(&format!(
                        "umask 077 && mkdir -p -- \"$(dirname -- {file})\" && ( {script} ) > {file} && wc -c < {file}"
                    ))
                    .with_context(|| format!("backing up {path}"))?;
                size.trim().parse().unwrap_or(0)
            }
        };
        out!("  {path}  {}", human_size(size));
        self.manifest.items.push(Item {
            path: path.to_string(),
            size,
        });
        Ok(())
    }

    /// Add a file with the given contents.
    pub fn file(&mut self, path: &str, role: Role, contents: &[u8]) -> Result<()> {
        if self.skips(role) {
            return Ok(());
        }
        match &self.staging {
            Staging::Local { dir, .. } => {
                create_parent(&dir.path().join(path))?.write_all(contents)?;
            }
            Staging::Server { dir, .. } => {
                self.connector
                    .write_file(&format!("{dir}/{path}"), contents, 0o600)?;
            }
        }
        self.manifest.items.push(Item {
            path: path.to_string(),
            size: contents.len() as u64,
        });
        Ok(())
    }

    fn skips(&self, role: Role) -> bool {
        self.format == Format::Raw && role == Role::Meta
    }

    /// Pack the items into the output file and return where it is.
    /// `stem` is the default name without extension, e.g. `myapp-staging-20261015-120000`.
    pub fn finish(self, stem: &str) -> Result<String> {
        let items: Vec<String> = self.manifest.items.iter().map(|i| i.path.clone()).collect();
        let (default_name, packed) = match (self.format, items.as_slice()) {
            (_, []) => bail!("nothing to back up"),
            (Format::Raw, [single]) => (format!("{stem}-{}", base_name(single)), None),
            (Format::Raw, _) => (format!("{stem}.tar.gz"), Some(items)),
            (Format::Depap, _) => {
                let mut entries = vec![MANIFEST_FILE.to_string()];
                entries.extend(items);
                (format!("{stem}.depap.tar.gz"), Some(entries))
            }
        };
        let manifest = serde_yaml_ng::to_string(&self.manifest)?;

        match &self.staging {
            Staging::Local { dir, output } => {
                let target = match output {
                    Some(path) if path.is_dir() => path.join(&default_name),
                    Some(path) => path.clone(),
                    None => PathBuf::from(&default_name),
                };
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)
                    .with_context(|| format!("cannot create {}", target.display()))?;
                let written = match &packed {
                    None => {
                        let mut source = File::open(dir.path().join(&self.manifest.items[0].path))?;
                        io::copy(&mut source, &mut BufWriter::new(file)).map(|_| ())
                    }
                    Some(entries) => {
                        fs::write(dir.path().join(MANIFEST_FILE), &manifest)?;
                        pack_local(dir.path(), entries, file)
                    }
                };
                if let Err(err) = written {
                    let _ = fs::remove_file(&target);
                    return Err(anyhow::Error::from(err)
                        .context(format!("cannot write {}", target.display())));
                }
                Ok(target.display().to_string())
            }
            Staging::Server {
                dir,
                output_dir,
                name,
            } => {
                let target = format!("{output_dir}/{}", name.as_deref().unwrap_or(&default_name));
                let quoted = shell_quote(&target);
                let script = match &packed {
                    None => format!(
                        "mv -- {} {quoted}",
                        shell_quote(&format!("{dir}/{}", self.manifest.items[0].path))
                    ),
                    Some(entries) => {
                        self.connector.write_file(
                            &format!("{dir}/{MANIFEST_FILE}"),
                            manifest.as_bytes(),
                            0o600,
                        )?;
                        let entries: Vec<String> = entries.iter().map(|e| shell_quote(e)).collect();
                        format!(
                            "umask 077 && cd {} && tar -czf {quoted}.tmp {} && mv -- {quoted}.tmp {quoted}",
                            shell_quote(dir),
                            entries.join(" ")
                        )
                    }
                };
                self.connector.exec(&format!(
                    "if [ -e {quoted} ]; then echo \"{target} already exists\" >&2; exit 1; fi; {script}"
                ))?;
                Ok(format!("{target} (on the server)"))
            }
        }
    }
}

impl Drop for Backup<'_> {
    fn drop(&mut self) {
        // The local staging folder removes itself; the server one is ours to remove.
        if let Staging::Server { dir, .. } = &self.staging {
            let _cleanup = crate::interrupt::cleanup();
            if let Err(err) = self
                .connector
                .exec(&format!("rm -rf -- {}", shell_quote(dir)))
            {
                eprintln!("warning: could not remove {dir}: {err:#}");
            }
        }
    }
}

fn create_parent(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(File::create(path)?)
}

/// e.g. `12.3 MB`
pub fn human_size(bytes: u64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "kB", "MB", "GB"] {
        if size < 1000.0 {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= 1000.0;
    }
    format!("{size:.1} TB")
}

/// `links/database/main-db.sql` → `main-db.sql`
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A tar.gz of the given files of `dir`, in the given order.
fn pack_local(dir: &Path, entries: &[String], file: File) -> io::Result<()> {
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for entry in entries {
        builder.append_path_with_name(dir.join(entry), entry)?;
    }
    builder.into_inner()?.finish()?.flush()
}
