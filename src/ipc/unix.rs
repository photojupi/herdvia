use std::{
    env,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::{
    net::{UnixListener, UnixStream},
    signal::unix::{Signal, SignalKind, signal},
};

pub type ServerStream = UnixStream;
pub type ClientStream = UnixStream;
pub(crate) type Watcher = OwnedFd;

pub struct Listener {
    pub socket: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
    _lock: File,
}

impl Listener {
    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        self.socket.accept().await.map(|(stream, _)| stream)
    }
}

pub async fn connect(dir: &Path) -> io::Result<ClientStream> {
    let stream = UnixStream::connect(dir.join("agent.sock")).await?;
    check_uid(&stream)?;
    Ok(stream)
}

pub(crate) fn check_peer(stream: &ServerStream) -> io::Result<()> {
    check_uid(stream)
}

pub(crate) fn watch_disconnect(stream: &ServerStream) -> io::Result<Watcher> {
    stream.as_fd().try_clone_to_owned()
}

pub(crate) struct Shutdown {
    interrupt: Signal,
    terminate: Signal,
}

impl Shutdown {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
        }
    }
}

pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != uid() || meta.mode() & 0o177 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be owned by this user with permissions no wider than 0600",
                path.display()
            ),
        ));
    }
    check_acl(&file, path)?;
    Ok(file)
}

/// The read-only checks that `prepare_dir` makes after its chmod: on macOS, no ACL
/// entry that allows access. Mode bits do not limit what an ACL allows.
#[cfg(target_os = "macos")]
pub(crate) fn check_dir(dir: &Path) -> io::Result<()> {
    check_acl(&open_dir(dir)?, dir)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn check_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn check_acl(_file: &File, _path: &Path) -> io::Result<()> {
    Ok(())
}

/// Refuses an extended ACL with any allow entry. An ACL of deny entries only is accepted.
#[cfg(target_os = "macos")]
fn check_acl(file: &File, path: &Path) -> io::Result<()> {
    use acl::*;
    // SAFETY: file owns a live descriptor; the returned ACL is freed by Acl below.
    let list = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if list.is_null() {
        let err = io::Error::last_os_error();
        return if no_acl(err.raw_os_error()) {
            Ok(())
        } else {
            Err(err)
        };
    }
    let _list = Acl(list);
    let mut id = ACL_FIRST_ENTRY;
    let mut entry = std::ptr::null_mut();
    // SAFETY: list is a live ACL and entry is a writable output. The entry stays
    // valid while list lives. The call fails with EINVAL after the last entry.
    while unsafe { acl_get_entry(list, id, &mut entry) } == 0 {
        id = ACL_NEXT_ENTRY;
        let mut tag = 0;
        // SAFETY: entry came from acl_get_entry on the live list; tag is writable.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if tag == ACL_EXTENDED_ALLOW {
            let path = path.display().to_string();
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{path} has an access control list (ACL) entry that allows access; remove the ACL with chmod -N '{}'",
                    path.replace('\'', r"'\''")
                ),
            ));
        }
    }
    Ok(())
}

/// Whether acl_get_fd_np failing with `errno` means that the file has no extended ACL: it has
/// none, or its file system does not support ACLs and so cannot hold an allow entry either.
#[cfg(target_os = "macos")]
fn no_acl(errno: Option<i32>) -> bool {
    matches!(errno, Some(libc::ENOENT | libc::ENOTSUP | libc::EOPNOTSUPP))
}

/// The macOS ACL API, from <sys/acl.h>. The libc crate does not declare it.
#[cfg(target_os = "macos")]
mod acl {
    use std::ffi::{c_int, c_void};

    // acl_type_t, acl_entry_id_t and acl_tag_t are C enums, so int.
    pub const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    pub const ACL_FIRST_ENTRY: c_int = 0;
    pub const ACL_NEXT_ENTRY: c_int = -1;
    pub const ACL_EXTENDED_ALLOW: c_int = 1;

    unsafe extern "C" {
        pub fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        pub fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        pub fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        pub fn acl_free(object: *mut c_void) -> c_int;
    }

    pub struct Acl(pub *mut c_void);

    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: the ACL came from acl_get_fd_np and this wrapper is its only owner.
            unsafe { acl_free(self.0) };
        }
    }
}

pub(crate) fn lock_file(lock: &File) -> io::Result<()> {
    // The key inode is stable. Its owner keeps this lock through the peer update.
    // SAFETY: lock owns this descriptor until the entire update finishes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn home_dir() -> io::Result<PathBuf> {
    env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = env::var_os("HERDVIA_DIR") {
        return super::override_dir(dir);
    }
    if cfg!(target_os = "macos") {
        Ok(home_dir()?.join("Library/Application Support/herdvia"))
    } else {
        let base = env::var_os("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".local/share"));
        Ok(base.join("herdvia"))
    }
}

pub(crate) fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn reject_root() -> io::Result<()> {
    if uid() == 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "herdvia must not run as root",
        ))
    } else {
        Ok(())
    }
}

/// Refuses an elevated administrator token on Windows. Root is refused by `reject_root`.
pub fn reject_elevated() -> io::Result<()> {
    Ok(())
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

fn open_dir(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(dir)
}

/// Creates `dir` if it is missing, then checks and fixes it through one descriptor, so that it
/// cannot be swapped between the checks and the chmod.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let not_a_dir = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data path must be a real directory",
        )
    };
    let open = || {
        open_dir(dir).map_err(|err| match err.raw_os_error() {
            // A symbolic link, or not a directory.
            Some(libc::ELOOP | libc::ENOTDIR) => not_a_dir(),
            _ => err,
        })
    };
    let file = match open() {
        // A directory that this user cannot read is fixed by path first, as before.
        Err(err) if err.raw_os_error() == Some(libc::EACCES) => {
            let metadata = fs::symlink_metadata(dir)?;
            if !metadata.is_dir() {
                return Err(not_a_dir());
            }
            require_uid(metadata.uid())?;
            chmod_nofollow(dir, 0o700)?;
            open()?
        }
        result => result?,
    };
    require_uid(file.metadata()?.uid())?;
    file.set_permissions(fs::Permissions::from_mode(0o700))?;
    check_acl(&file, dir)
}

/// Sets the mode of `path` without following a symbolic link there, which may have replaced
/// the directory since it was checked. macOS sets the link's own mode; Linux (glibc 2.32 or
/// later, or musl) refuses a link with EOPNOTSUPP.
fn chmod_nofollow(path: &Path, mode: libc::mode_t) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: path is a NUL-terminated string that lives for the call.
    if unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            path.as_ptr(),
            mode,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn lock_dir(dir: &Path) -> io::Result<File> {
    let lock = File::open(dir)?;
    // Lock the directory so two agents cannot race when removing a stale socket.
    // SAFETY: lock owns the descriptor; dropping it releases the lock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err);
        }
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "agent is already running",
        ));
    }
    Ok(lock)
}

pub fn listen(dir: &Path) -> io::Result<Listener> {
    prepare_dir(dir)?;
    let lock = lock_dir(dir)?;
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

/// Held by tests that start a child process, and by a test whose closed socket must not
/// live on in such a child: until exec, a child holds every descriptor of this process.
#[cfg(test)]
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs /bin/chmod in a test, for example to add an ACL with +a.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn test_chmod(args: &[&str], path: &Path) {
    let _spawn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let status = std::process::Command::new("/bin/chmod")
        .args(args)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "chmod {args:?} {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[cfg(target_os = "macos")]
    use test_chmod as chmod;

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
        let dir = temp.path().join("herdvia");
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
        let spawn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        drop(listen(temp.path()).unwrap());
        drop(spawn);
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

    #[test]
    fn prepare_dir_refuses_a_symlink_and_fixes_a_directory_this_user_cannot_read() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        symlink(temp.path(), &link).unwrap();
        let err = prepare_dir(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(err.to_string(), "data path must be a real directory");
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        prepare_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn chmod_nofollow_leaves_the_target_of_a_link_alone() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"x").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        // macOS changes the link itself; Linux refuses to.
        let _ = chmod_nofollow(&link, 0o700);
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
        let dir = temp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        chmod_nofollow(&dir, 0o700).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[cfg(target_os = "macos")]
    fn assert_acl_refused(result: io::Result<impl std::fmt::Debug>, path: &Path) {
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            err.to_string().starts_with(&path.display().to_string())
                && err.to_string().contains("chmod -N"),
            "{err}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_a_new_key_in_a_directory_with_an_inheritable_allow_acl() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        chmod(&["+a", "everyone allow read,file_inherit"], &dir);
        assert_acl_refused(crate::peers::load_key(&dir), &dir);
        assert!(!dir.join("secret.key").exists());
        assert_acl_refused(check_dir(&dir), &dir);
        chmod(&["-N"], &dir);
        crate::peers::load_key(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_key_and_peers_with_an_allow_acl() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let key = crate::peers::load_key(dir).unwrap();
        let peer = crate::peers::Peer {
            name: "work".into(),
            id: key.public(),
            permission: crate::peers::Permission::Both,
        };
        crate::peers::allow(dir, peer.clone()).unwrap();
        let (key_path, peers_path) = (dir.join("secret.key"), dir.join("peers"));
        chmod(&["+a", "everyone allow read"], &key_path);
        assert_acl_refused(crate::peers::load_key(dir), &key_path);
        chmod(&["-N"], &key_path);
        assert_eq!(
            crate::peers::load_key(dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        chmod(&["+a", "everyone allow write"], &peers_path);
        assert_acl_refused(crate::peers::load(dir), &peers_path);
        chmod(&["-N"], &peers_path);
        assert_eq!(crate::peers::load(dir).unwrap(), vec![peer]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn accepts_an_acl_of_deny_entries_only() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        chmod(&["+a", "everyone deny delete"], &dir);
        let key = crate::peers::load_key(&dir).unwrap();
        chmod(&["+a", "everyone deny write"], &dir.join("secret.key"));
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        check_dir(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn treats_a_file_system_without_acls_as_having_no_acl() {
        for errno in [libc::ENOENT, libc::ENOTSUP, libc::EOPNOTSUPP] {
            assert!(no_acl(Some(errno)), "{errno}");
        }
        for errno in [None, Some(libc::EACCES), Some(libc::EBADF), Some(libc::EIO)] {
            assert!(!no_acl(errno), "{errno:?}");
        }
    }
}
