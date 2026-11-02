use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;

/// Owns the private IPC directory and its exclusive daemon-instance lock.
pub struct RuntimeDir {
    path: PathBuf,
    _lock: File,
    _temporary: Option<tempfile::TempDir>,
}

impl RuntimeDir {
    pub fn new() -> io::Result<Arc<Self>> {
        if let Some(path) =
            std::env::var_os("OPLUS_CONNECT_RUNTIME_DIR").filter(|value| !value.is_empty())
        {
            let path = PathBuf::from(path);
            validate_directory(&path)?;
            return Self::lock(path, None);
        }
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
            Some(root) => Self::in_runtime_root(Path::new(&root)),
            None => Self::temporary(),
        }
    }

    fn in_runtime_root(root: &Path) -> io::Result<Arc<Self>> {
        validate_directory(root)?;
        let path = root.join("oppo-connect");
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        validate_directory(&path)?;
        Self::lock(path, None)
    }

    pub(super) fn temporary() -> io::Result<Arc<Self>> {
        let directory = tempfile::Builder::new()
            .prefix("oppo-connect-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        validate_directory(directory.path())?;
        Self::lock(directory.path().to_owned(), Some(directory))
    }

    fn lock(path: PathBuf, temporary: Option<tempfile::TempDir>) -> io::Result<Arc<Self>> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.join("instance.lock"))?;
        let metadata = lock.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != effective_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unsafe IPC instance lock",
            ));
        }
        // SAFETY: flock only acts on this live, owned file descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            path,
            _lock: lock,
            _temporary: temporary,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn bind(self: &Arc<Self>, name: &str) -> io::Result<BoundSocket> {
        if name.is_empty()
            || Path::new(name).components().count() != 1
            || !matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC socket name must be a basename",
            ));
        }
        validate_directory(&self.path)?;
        let path = self.path.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() || metadata.uid() != effective_uid() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Refusing to replace a non-owned socket node",
                    ));
                }
                match std::os::unix::net::UnixStream::connect(&path) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "IPC socket already has an active listener",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                    Err(error) => return Err(error),
                }
                let current = fs::symlink_metadata(&path)?;
                if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "IPC socket changed during stale-node check",
                    ));
                }
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        Ok(BoundSocket {
            listener,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            _directory: self.clone(),
        })
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no pointers and has no preconditions.
    unsafe { libc::geteuid() }
}

fn validate_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC runtime directory must be an absolute path without '..'",
        ));
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        if !fs::symlink_metadata(&prefix)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC runtime path must not contain symlinks",
            ));
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != effective_uid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC runtime directory must be owner-only and owned by the current user",
        ));
    }
    Ok(())
}

pub(crate) struct BoundSocket {
    pub(crate) listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
    _directory: Arc<RuntimeDir>,
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[tokio::test]
    async fn refuses_existing_files_symlinks_and_live_sockets() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = RuntimeDir::in_runtime_root(root.path()).unwrap();
        let file = runtime.path().join("file");
        fs::write(&file, b"keep me").unwrap();
        assert!(runtime.bind("file").is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep me");
        symlink(&file, runtime.path().join("link")).unwrap();
        assert!(runtime.bind("link").is_err());
        assert!(
            fs::symlink_metadata(runtime.path().join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let socket = runtime.bind("live").unwrap();
        let inode = fs::symlink_metadata(runtime.path().join("live"))
            .unwrap()
            .ino();
        assert!(runtime.bind("live").is_err());
        assert_eq!(
            fs::symlink_metadata(runtime.path().join("live"))
                .unwrap()
                .ino(),
            inode
        );
        assert!(RuntimeDir::in_runtime_root(root.path()).is_err());
        drop(socket);
        assert!(!runtime.path().join("live").exists());
    }

    #[tokio::test]
    async fn reclaims_only_stale_owned_socket_and_preserves_replacements() {
        let runtime = RuntimeDir::temporary().unwrap();
        let path = runtime.path().join("stale");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        // Other tests spawn processes concurrently; inherited CLOEXEC handles
        // are only released when those children reach exec.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match tokio::net::UnixStream::connect(&path).await {
                    Ok(stream) => drop(stream),
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => break,
                    Err(error) => panic!("unexpected stale socket error: {error}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("stale listener still has a live owner");
        let socket = runtime.bind("stale").unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        drop(socket);
        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }

    #[test]
    fn rejects_insecure_and_symlinked_directories() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(RuntimeDir::in_runtime_root(&link).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RuntimeDir::in_runtime_root(&target).is_err());
    }

    #[test]
    fn temporary_directory_lives_until_last_owner_drops() {
        let runtime = RuntimeDir::temporary().unwrap();
        let path = runtime.path().to_owned();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        let owner = runtime.clone();
        drop(runtime);
        assert!(path.exists());
        drop(owner);
        assert!(!path.exists());
    }
}
