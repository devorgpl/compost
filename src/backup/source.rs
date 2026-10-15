//! Reading a depap archive for restore. It is unpacked where it is: in a
//! local temporary folder, or next to the archive on the server.

use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use tempfile::TempDir;

use super::archive::{MANIFEST_FILE, Manifest};
use crate::connector::{Connector, Output, shell_quote};
use crate::error::config_err;

enum Unpacked {
    Local(TempDir),
    Server(String),
}

pub struct Archive<'c> {
    connector: &'c dyn Connector,
    pub manifest: Manifest,
    unpacked: Unpacked,
}

impl<'c> Archive<'c> {
    /// Unpack an archive on this machine.
    pub fn open_local(connector: &'c dyn Connector, path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| config_err(format!("{}: {e}", path.display())))?;
        let dir = tempfile::Builder::new()
            .prefix("depap-restore-")
            .tempdir()?;
        // `unpack` refuses entries that would land outside `dir`.
        tar::Archive::new(GzDecoder::new(BufReader::new(file)))
            .unpack(dir.path())
            .map_err(|e| not_an_archive(&path.display().to_string(), &e.to_string()))?;
        let text = fs::read_to_string(dir.path().join(MANIFEST_FILE))
            .map_err(|_| not_an_archive(&path.display().to_string(), "no manifest.yaml"))?;
        Self::new(connector, &text, Unpacked::Local(dir))
    }

    /// Unpack an archive that is on the server.
    pub fn open_server(connector: &'c dyn Connector, path: &str) -> Result<Self> {
        let parent = path.rsplit_once('/').map_or(".", |(dir, _)| dir);
        let dir = format!("{parent}/.restore-{}", uuid::Uuid::new_v4().simple());
        let quoted = shell_quote(path);
        let output = connector.run(
            &format!(
                "[ -f {quoted} ] || {{ echo 'no such file' >&2; exit 1; }}; \
                 umask 077 && mkdir -p -- {dir} && tar -xzf {quoted} -C {dir}",
                dir = shell_quote(&dir)
            ),
            None,
        )?;
        let unpacked = Unpacked::Server(dir.clone());
        if !output.success() {
            drop(Self::cleanup_server(connector, &dir));
            return Err(not_an_archive(
                &format!("{path} (on the server)"),
                output.stderr.trim(),
            ));
        }
        let Some(text) = connector.read_file(&format!("{dir}/{MANIFEST_FILE}"))? else {
            drop(Self::cleanup_server(connector, &dir));
            return Err(not_an_archive(
                &format!("{path} (on the server)"),
                "no manifest.yaml",
            ));
        };
        Self::new(connector, &text, unpacked)
    }

    fn new(connector: &'c dyn Connector, manifest: &str, unpacked: Unpacked) -> Result<Self> {
        // Built first, so its drop removes the unpacked files on errors below.
        let mut archive = Self {
            connector,
            manifest: Manifest::new(super::Scope::App, ""),
            unpacked,
        };
        archive.manifest = serde_yaml_ng::from_str(manifest).context("invalid manifest.yaml")?;
        archive.manifest.check_format()?;
        Ok(archive)
    }

    fn cleanup_server(connector: &dyn Connector, dir: &str) -> Result<String> {
        let _cleanup = crate::interrupt::cleanup();
        connector.exec(&format!("rm -rf -- {}", shell_quote(dir)))
    }

    pub fn has(&self, path: &str) -> bool {
        self.manifest.has(path)
    }

    fn check(&self, path: &str) -> Result<()> {
        if !self.has(path) {
            bail!("the archive has no {path}");
        }
        Ok(())
    }

    /// A small text item, e.g. a config file.
    pub fn read(&self, path: &str) -> Result<String> {
        self.check(path)?;
        match &self.unpacked {
            Unpacked::Local(dir) => Ok(fs::read_to_string(dir.path().join(path))?),
            Unpacked::Server(dir) => self
                .connector
                .read_file(&format!("{dir}/{path}"))?
                .with_context(|| format!("{path} is missing in the unpacked archive")),
        }
    }

    /// Run `script` on the server with `prelude` followed by the item as stdin.
    pub fn feed(&self, path: &str, prelude: &[u8], script: &str) -> Result<()> {
        self.check(path)?;
        match &self.unpacked {
            Unpacked::Local(dir) => {
                let file = File::open(dir.path().join(path))?;
                let mut stdin = Cursor::new(prelude.to_vec()).chain(BufReader::new(file));
                self.connector.stream_in(script, &mut stdin)
            }
            Unpacked::Server(dir) => {
                let file = shell_quote(&format!("{dir}/{path}"));
                let script = if prelude.is_empty() {
                    format!("( {script} ) < {file}")
                } else {
                    let prelude = shell_quote(&String::from_utf8_lossy(prelude));
                    format!("{{ printf '%s' {prelude}; cat -- {file}; }} | ( {script} )")
                };
                self.connector.exec(&script).map(drop)
            }
        }
    }

    /// Run `script` with the item as stdin and return its output, whatever the exit status.
    pub fn feed_output(&self, path: &str, script: &str) -> Result<Output> {
        self.check(path)?;
        match &self.unpacked {
            Unpacked::Local(dir) => {
                let mut file = BufReader::new(File::open(dir.path().join(path))?);
                self.connector
                    .run_stream(script, Some(&mut file), &mut std::io::sink())
            }
            Unpacked::Server(dir) => {
                let file = shell_quote(&format!("{dir}/{path}"));
                self.connector
                    .run(&format!("( {script} ) < {file} > /dev/null"), None)
            }
        }
    }

    /// Copy an item to a local file.
    pub fn fetch(&self, path: &str, local: &Path) -> Result<()> {
        self.check(path)?;
        match &self.unpacked {
            Unpacked::Local(dir) => {
                fs::copy(dir.path().join(path), local)?;
            }
            Unpacked::Server(dir) => {
                let mut file = File::create(local)?;
                self.connector.stream_out(
                    &format!("cat -- {}", shell_quote(&format!("{dir}/{path}"))),
                    &mut file,
                )?;
            }
        }
        Ok(())
    }
}

impl Drop for Archive<'_> {
    fn drop(&mut self) {
        if let Unpacked::Server(dir) = &self.unpacked {
            if let Err(err) = Self::cleanup_server(self.connector, dir) {
                eprintln!("warning: could not remove {dir}: {err:#}");
            }
        }
    }
}

fn not_an_archive(path: &str, reason: &str) -> anyhow::Error {
    config_err(format!(
        "{path} is not a depap archive ({reason}); only backups in the depap format can be restored"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Backup, Format, Output, Role, Scope};
    use super::*;
    use crate::connector::local::Local;

    const LOCAL: Local = Local { verbose: false };

    fn backup(format: Format, output: Output) -> Backup<'static> {
        let mut backup =
            Backup::new(&LOCAL, format, Manifest::new(Scope::App, "test"), output).unwrap();
        backup
            .stream(
                "apps/a/p/links/database/main-db.sql",
                Role::Data,
                "printf 'SELECT 1;'",
            )
            .unwrap();
        backup
            .file("apps/a/p/depap.yaml", Role::Meta, b"name: a\n")
            .unwrap();
        backup
    }

    #[test]
    fn local_round_trip() {
        let out = TempDir::new().unwrap();
        let path = backup(Format::Depap, Output::Local(Some(out.path().to_path_buf())))
            .finish("a-p-1")
            .unwrap();
        assert!(path.ends_with("a-p-1.depap.tar.gz"), "{path}");
        let mode = fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );

        let archive = Archive::open_local(&LOCAL, Path::new(&path)).unwrap();
        assert_eq!(archive.manifest.items.len(), 2);
        assert_eq!(archive.read("apps/a/p/depap.yaml").unwrap(), "name: a\n");
        let sink = out.path().join("fed");
        let script = format!("cat > {}", shell_quote(sink.to_str().unwrap()));
        archive
            .feed("apps/a/p/links/database/main-db.sql", b"SET x;\n", &script)
            .unwrap();
        assert_eq!(fs::read_to_string(&sink).unwrap(), "SET x;\nSELECT 1;");
        assert!(archive.read("apps/a/p/other").is_err());
    }

    #[test]
    fn server_round_trip() {
        let home = TempDir::new().unwrap();
        let dir = home.path().join("backups").to_str().unwrap().to_string();
        let output = Output::Server {
            dir: dir.clone(),
            name: None,
        };
        let path = backup(Format::Depap, output).finish("a-p-1").unwrap();
        assert_eq!(path, format!("{dir}/a-p-1.depap.tar.gz (on the server)"));
        // Only the archive is left, no staging folder.
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["a-p-1.depap.tar.gz"]);

        let archive = Archive::open_server(&LOCAL, &format!("{dir}/a-p-1.depap.tar.gz")).unwrap();
        assert_eq!(archive.read("apps/a/p/depap.yaml").unwrap(), "name: a\n");
        let sink = home.path().join("fed");
        let script = format!("cat > {}", shell_quote(sink.to_str().unwrap()));
        archive
            .feed("apps/a/p/links/database/main-db.sql", b"SET x;\n", &script)
            .unwrap();
        assert_eq!(fs::read_to_string(&sink).unwrap(), "SET x;\nSELECT 1;");
        drop(archive);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "unpacked files are removed"
        );
    }

    #[test]
    fn raw_single_item_is_the_file_itself() {
        let out = TempDir::new().unwrap();
        let path = backup(Format::Raw, Output::Local(Some(out.path().to_path_buf())))
            .finish("a-p-1")
            .unwrap();
        assert!(path.ends_with("a-p-1-main-db.sql"), "{path}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "SELECT 1;");

        let err = Archive::open_local(&LOCAL, Path::new(&path)).err().unwrap();
        assert!(err.to_string().contains("not a depap archive"), "{err}");
    }

    #[test]
    fn does_not_overwrite() {
        let out = TempDir::new().unwrap();
        let target = out.path().join("x.tar.gz");
        fs::write(&target, "keep").unwrap();
        let err = backup(Format::Depap, Output::Local(Some(target.clone())))
            .finish("a")
            .unwrap_err();
        assert!(format!("{err:#}").contains("cannot create"), "{err:#}");
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    }
}
