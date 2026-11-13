use std::{
    env,
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

pub struct Listener {
    pub socket: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
    _lock: File,
}

pub fn home_dir() -> io::Result<PathBuf> {
    env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = env::var_os("HLINK_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if cfg!(target_os = "macos") {
        Ok(home_dir()?.join("Library/Application Support/hlink"))
    } else {
        let base = env::var_os("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".local/share"));
        Ok(base.join("hlink"))
    }
}

fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn require_uid(peer: u32) -> io::Result<()> {
    if peer == uid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer uid does not match",
        ))
    }
}

pub fn check_uid(stream: &UnixStream) -> io::Result<()> {
    require_uid(stream.peer_cred()?.uid())
}

pub(crate) async fn disconnected(socket: OwnedFd) -> io::Error {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tick.tick().await;
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // No reads or buffering: notice hangup even while the child's stdin is full.
        // SAFETY: descriptor points to one initialized pollfd and socket owns its fd.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return err;
            }
        } else if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return io::Error::new(io::ErrorKind::ConnectionAborted, "peer disconnected");
        }
    }
}

pub fn listen(dir: &Path) -> io::Result<Listener> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data path must be a real directory",
        ));
    }
    require_uid(metadata.uid())?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let lock = File::open(dir)?;
    // Lock the directory so two agents cannot race when removing a stale socket.
    // SAFETY: the file descriptor stays open for the lifetime of the listener.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "agent is already running",
        ));
    }
    let path = dir.join("agent.sock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            require_uid(metadata.uid())?;
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "agent.sock is not a socket",
                ));
            }
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "agent is already running",
                    ));
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&path)?;
                }
                Err(err) => return Err(err),
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let socket = UnixListener::bind(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    let listener = Listener {
        socket,
        path,
        identity: (metadata.dev(), metadata.ino()),
        _lock: lock,
    };
    fs::set_permissions(&listener.path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn rejects_a_different_uid() {
        assert_eq!(
            require_uid(uid().wrapping_add(1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn secures_directory_and_socket_and_checks_peer_uid() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("hlink");
        let listener = listen(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(dir.join("agent.sock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let client = UnixStream::connect(dir.join("agent.sock")).await.unwrap();
        let (server, _) = listener.socket.accept().await.unwrap();
        check_uid(&client).unwrap();
        check_uid(&server).unwrap();
        assert!(listen(&dir).is_err());
        drop(listener);
        assert!(!dir.join("agent.sock").exists());
    }

    #[tokio::test]
    async fn recovers_stale_socket_but_preserves_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agent.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        drop(listen(temp.path()).unwrap());
        std::fs::write(&path, b"keep").unwrap();
        assert!(listen(temp.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn rejects_symlink_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        symlink(temp.path(), &link).unwrap();
        assert!(listen(&link).is_err());
    }
}
