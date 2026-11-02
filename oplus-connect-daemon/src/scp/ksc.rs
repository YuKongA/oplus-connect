//! KSC (key for the secure channel) storage.
//!
//! `connection::protocol::ksc::ksc_repo::gen_ksc` generates it purely locally:
//! 16 random bytes (key) + 6 random bytes (alias) via `ThreadRng`. The value is
//! then handed to peers inside `ScpSyncSecure` during pairing, so we are free to
//! mint our own instead of needing an OPPO account.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
    crate::net_utils::config_dir().join("oppo-connect").join("ksc.bin")
}

impl Ksc {
    /// Loads the persisted KSC or creates it with owner-only file permissions.
    /// Invalid existing data is an error; replacing it would silently rotate identity.
    pub fn load_or_create() -> io::Result<Self> {
        Self::load_or_create_at(&path())
    }

    fn load_or_create_at(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
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
        let bytes = fs::read(path)?;
        if bytes.len() != KSC_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid KSC file length: expected {KSC_LEN} bytes, got {}",
                    bytes.len()
                ),
            ));
        }

        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

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
}
