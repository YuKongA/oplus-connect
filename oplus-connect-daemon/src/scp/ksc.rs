//! KSC (key for the secure channel) storage.
//!
//! `connection::protocol::ksc::ksc_repo::gen_ksc` generates it purely locally:
//! 16 random bytes (key) + 6 random bytes (alias) via `ThreadRng`. The value is
//! then handed to peers inside `ScpSyncSecure` during pairing, so we are free to
//! mint our own instead of needing an OPPO account.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const KSC_LEN: usize = 22;

#[cfg(test)]
static NEXT_TEST_PATH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ksc {
    pub alias: Vec<u8>,
    pub key: Vec<u8>,
}

fn path() -> PathBuf {
    crate::net_utils::config_dir()
        .join("oppo-connect")
        .join("ksc.bin")
}

impl Ksc {
    /// Loads the persisted KSC or creates it with owner-only file permissions.
    /// Invalid existing data is an error; replacing it would silently rotate identity.
    pub fn load_or_create() -> io::Result<Self> {
        Self::load_or_create_at(&path())
    }

    fn load_or_create_at(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(dir)?;
        }

        match Self::load_existing(path) {
            Ok(ksc) => return Ok(ksc),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let generated = Self::generate()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        match options.open(path) {
            Ok(mut file) => {
                let mut bytes = Vec::with_capacity(KSC_LEN);
                bytes.extend_from_slice(&generated.alias);
                bytes.extend_from_slice(&generated.key);
                if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    drop(file);
                    let _ = fs::remove_file(path);
                    return Err(error);
                }
                Ok(generated)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Self::load_existing(path),
            Err(error) => Err(error),
        }
    }

    fn load_existing(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(path)?;

        #[cfg(unix)]
        {
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != crate::ipc::runtime::effective_uid()
                || metadata.mode() & 0o022 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "KSC file must be a regular file owned by the current user and not group/other writable",
                ));
            }
        }

        let mut bytes = Vec::with_capacity(KSC_LEN + 1);
        (&file).take(KSC_LEN as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() != KSC_LEN {
            let observed = if bytes.len() > KSC_LEN {
                format!("at least {}", KSC_LEN + 1)
            } else {
                bytes.len().to_string()
            };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid KSC file length: expected {KSC_LEN} bytes, got {observed}"),
            ));
        }

        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            alias: bytes[..6].to_vec(),
            key: bytes[6..].to_vec(),
        })
    }

    pub fn generate() -> io::Result<Self> {
        // Use the platform's operating system CSPRNG; /dev/urandom is not
        // available on Windows.
        let mut buf = [0u8; KSC_LEN];
        getrandom::fill(&mut buf).map_err(io::Error::other)?;
        Ok(Self {
            alias: buf[..6].to_vec(),
            key: buf[6..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path() -> PathBuf {
        let id = NEXT_TEST_PATH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "oppo-connect-ksc-{}-{id}/ksc.bin",
            std::process::id()
        ))
    }

    #[test]
    fn persists_identity_with_private_permissions() {
        let path = test_path();
        let first = Ksc::load_or_create_at(&path).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let second = Ksc::load_or_create_at(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.alias.len(), 6);
        assert_eq!(first.key.len(), 16);

        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn refuses_to_replace_corrupt_identity() {
        let path = test_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0x5a; KSC_LEN - 1]).unwrap();

        let error = Ksc::load_or_create_at(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), [0x5a; KSC_LEN - 1]);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_or_foreign_key_store() {
        use std::os::unix::fs::symlink;

        let path = test_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let identity = Ksc::generate().unwrap();
        let mut encoded = identity.alias.clone();
        encoded.extend_from_slice(&identity.key);

        let target = path.with_extension("target");
        fs::write(&target, &encoded).unwrap();
        symlink(&target, &path).unwrap();
        assert!(
            Ksc::load_or_create_at(&path).is_err(),
            "followed a symlinked key store"
        );

        // A group/other writable store can be swapped by another local user.
        fs::remove_file(&path).unwrap();
        fs::write(&path, &encoded).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Ksc::load_or_create_at(&path).is_err());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
