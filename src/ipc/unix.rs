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
    // A missing directory or socket still fails with NotFound below, a dead socket with
    // ConnectionRefused: callers read those two as "no agent".
    check_parents(dir)?;
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
    if let Some(dir) = path.parent() {
        check_parents(dir)?;
    }
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

/// The read-only checks that `prepare_dir` makes: parent directories that no other account
/// can change, and after its chmod, on macOS, no ACL entry that allows access. Mode bits do
/// not limit what an ACL allows.
#[cfg(target_os = "macos")]
pub(crate) fn check_dir(dir: &Path) -> io::Result<()> {
    check_parents(dir)?;
    check_acl(&open_dir(dir)?, dir)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn check_dir(dir: &Path) -> io::Result<()> {
    check_parents(dir)
}

/// The most symbolic links that `check_parents` follows, as many as macOS follows in a path.
const MAX_LINKS: usize = 32;

const SAFE_PARENTS: &str =
    "use a data directory whose parent directories only this user or root can change";

/// Why another account could change the path to the data directory at an entry on it.
#[derive(Debug, PartialEq)]
enum Unsafe {
    /// Owned by this uid, neither this user nor root.
    Owner(u32),
    /// A directory with this mode, writable by group or others, without the sticky bit.
    Writable(u32),
    /// A directory with an ACL entry that allows changing its entries (macOS).
    Acl,
}

/// This user and root may change the path; any other owner could replace what is there.
fn trusted(owner: u32, me: u32) -> bool {
    owner == me || owner == 0
}

/// Whether a directory on the path with `owner` and `mode` keeps others from changing its
/// entries. Ok(true) for a trusted sticky directory that others can write to (/tmp): each
/// entry passed in it must then be trusted too, since the sticky bit lets only the owner of
/// an entry (or of the directory, or root) remove or rename it.
fn judge_parent(owner: u32, mode: u32, me: u32) -> Result<bool, Unsafe> {
    if !trusted(owner, me) {
        Err(Unsafe::Owner(owner))
    } else if mode & 0o022 == 0 {
        Ok(false)
    } else if mode & 0o1000 != 0 {
        Ok(true)
    } else {
        Err(Unsafe::Writable(mode & 0o7777))
    }
}

/// An entry with `owner` that the path passes in a directory; `shared` is what
/// `judge_parent` returned for that directory.
fn judge_entry(shared: bool, owner: u32, me: u32) -> Result<(), Unsafe> {
    if shared && !trusted(owner, me) {
        Err(Unsafe::Owner(owner))
    } else {
        Ok(())
    }
}

fn path_refusal(entry: &Path, why: Unsafe, dir: &Path) -> io::Error {
    let (entry, dir) = (entry.display(), dir.display());
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        match why {
            Unsafe::Owner(owner) => format!(
                "{entry} is owned by another user (uid {owner}), who could change the path to the data directory {dir}; {SAFE_PARENTS}"
            ),
            Unsafe::Writable(mode) => format!(
                "{entry} is writable by other users (mode {mode:o}, no sticky bit), who could change the path to the data directory {dir}; run chmod go-w on it, or {SAFE_PARENTS}"
            ),
            Unsafe::Acl => format!(
                "{entry} has an access control list (ACL) entry that allows adding, deleting, or changing entries or permissions there, so another account could change the path to the data directory {dir}; remove that entry (see ls -led '{}'), or {SAFE_PARENTS}",
                entry.to_string().replace('\'', r"'\''")
            ),
        },
    )
}

/// Refuses a data directory `dir` whose path another account could change after the checks
/// that use it: walks from the root down and requires every directory above `dir` to be owned
/// by this user or root, without write permission for group or others unless it has the
/// sticky bit, and on macOS without an ACL entry that allows changing its entries. An entry
/// passed in a sticky directory must be owned by this user or root as well. A symbolic link
/// on the path, `dir` included, is followed and its target checked the same way; a link as
/// `dir` itself is still refused by `prepare_dir`. Where `dir` or a parent is missing, only
/// the existing part is checked, and a ".." after the missing entry is refused. Creates and
/// changes nothing. This user and root are trusted: a change they make is not prevented.
///
/// The built-in data directory (`default_dir`) is accepted without this check: an account
/// that could change the folders above the home directory could already run programs as this
/// user, and refusing it there would leave the agent no way to start.
pub(crate) fn check_parents(dir: &Path) -> io::Result<()> {
    check_parents_unless(dir, default_dir().ok().as_deref())
}

/// `check_parents`, except for `dir` spelled as `default`, the built-in data directory.
fn check_parents_unless(dir: &Path, default: Option<&Path>) -> io::Result<()> {
    if default == Some(dir) {
        return Ok(());
    }
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        env::current_dir()?.join(dir)
    };
    let me = uid();
    let root = PathBuf::from("/");
    let shared = check_parent(&root, &fs::metadata(&root)?, me, &dir)?;
    // The checked directories from the root down, with what `judge_parent` returned for them.
    let mut chain = vec![(root, shared)];
    let mut names = path_names(&dir);
    let mut links = 0;
    while let Some(name) = names.pop_front() {
        if name == ".." {
            if chain.len() > 1 {
                chain.pop();
            }
            continue;
        }
        let (parent, shared) = chain.last().expect("the root stays");
        let path = parent.join(&name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Nothing exists below a missing entry yet, unless a ".." after it leads back to
            // directories that the walk has not reached, which creating it would pass through.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if names.iter().any(|name| name == "..") {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "the path to the data directory {} has \"..\" after {}, which does not exist, so the directories that \"..\" leads back to cannot be checked; write the path without \"..\"",
                            dir.display(),
                            path.display()
                        ),
                    ));
                }
                return Ok(());
            }
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("{}: {err}", path.display()),
                ));
            }
        };
        judge_entry(*shared, metadata.uid(), me).map_err(|why| path_refusal(&path, why, &dir))?;
        // A link is followed even as the last name: the kernel follows it there for a path
        // that ends in "/" or "/.", and for the files that private_file and connect open in
        // it. prepare_dir still refuses a link as the data directory.
        if metadata.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS {
                return Err(io::Error::other(format!(
                    "the path to the data directory {} has more than {MAX_LINKS} symbolic links",
                    dir.display()
                )));
            }
            let target = fs::read_link(&path)?;
            if target.is_absolute() {
                chain.truncate(1);
            }
            for name in path_names(&target).into_iter().rev() {
                names.push_front(name);
            }
            continue;
        }
        if names.is_empty() {
            // The data directory itself, which prepare_dir and private_file check.
            return Ok(());
        }
        if !metadata.is_dir() {
            // Opening a path through it fails with ENOTDIR.
            return Ok(());
        }
        let shared = check_parent(&path, &metadata, me, &dir)?;
        chain.push((path, shared));
    }
    Ok(())
}

/// The names in `path` after its root, with "." left out and ".." kept.
fn path_names(path: &Path) -> std::collections::VecDeque<std::ffi::OsString> {
    use std::path::Component;
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some("..".into()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// A directory above the data directory, by `judge_parent` and then its ACL (macOS).
fn check_parent(path: &Path, metadata: &fs::Metadata, me: u32, dir: &Path) -> io::Result<bool> {
    let shared = judge_parent(metadata.uid(), metadata.mode(), me)
        .map_err(|why| path_refusal(path, why, dir))?;
    if parent_acl_changes(path)? {
        return Err(path_refusal(path, Unsafe::Acl, dir));
    }
    Ok(shared)
}

/// The macOS ACL permissions that let another account replace an entry of a directory, or
/// the directory itself: delete, delete_child, add_file, add_subdirectory, writesecurity,
/// and chown. Read and search permissions do not.
#[cfg(target_os = "macos")]
const PATH_CHANGES: u64 = 1 << 2 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 12 | 1 << 13;

/// Whether an ACL entry with `tag` and permission `mask` lets an account change the path.
#[cfg(target_os = "macos")]
fn changes_path(tag: std::ffi::c_int, mask: u64) -> bool {
    tag == acl::ACL_EXTENDED_ALLOW && mask & PATH_CHANGES != 0
}

/// Whether the extended ACL of the directory at `path`, not followed if it is a link, has
/// an allow entry with a permission that changes the path. Read and search allow entries
/// and deny entries are accepted. An error that means no ACL is handled as by `check_acl`.
/// The path is read by name: the directories above it were checked first, so no other
/// account can replace it.
#[cfg(target_os = "macos")]
fn parent_acl_changes(path: &Path) -> io::Result<bool> {
    use acl::*;
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: c_path is a NUL-terminated string that lives for the call; the returned ACL
    // is freed by Acl below.
    let list = unsafe { acl_get_link_np(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
    if list.is_null() {
        let err = io::Error::last_os_error();
        return if no_acl(err.raw_os_error()) {
            Ok(false)
        } else {
            Err(io::Error::new(
                err.kind(),
                format!("{}: {err}", path.display()),
            ))
        };
    }
    let _list = Acl(list);
    let mut id = ACL_FIRST_ENTRY;
    let mut entry = std::ptr::null_mut();
    // SAFETY: as in check_acl.
    while unsafe { acl_get_entry(list, id, &mut entry) } == 0 {
        id = ACL_NEXT_ENTRY;
        let (mut tag, mut mask) = (0, 0);
        // SAFETY: entry came from acl_get_entry on the live list; tag and mask are writable.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0
            || unsafe { acl_get_permset_mask_np(entry, &mut mask) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if changes_path(tag, mask) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Linux checks the mode only: the group bits of a directory with a POSIX ACL hold its mask,
/// the most that any named user or group entry allows.
#[cfg(not(target_os = "macos"))]
fn parent_acl_changes(_path: &Path) -> io::Result<bool> {
    Ok(false)
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
    use std::ffi::{c_char, c_int, c_void};

    // acl_type_t, acl_entry_id_t and acl_tag_t are C enums, so int.
    pub const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    pub const ACL_FIRST_ENTRY: c_int = 0;
    pub const ACL_NEXT_ENTRY: c_int = -1;
    pub const ACL_EXTENDED_ALLOW: c_int = 1;

    unsafe extern "C" {
        pub fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        pub fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        pub fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        pub fn acl_get_link_np(path: *const c_char, kind: c_int) -> *mut c_void;
        pub fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> c_int;
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

/// The data directory that `data_dir` gives without HERDVIA_DIR, and on Linux without
/// XDG_DATA_HOME as well.
fn default_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(if cfg!(target_os = "macos") {
        "Library/Application Support/herdvia"
    } else {
        ".local/share/herdvia"
    }))
}

pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = env::var_os("HERDVIA_DIR") {
        return super::override_dir(dir);
    }
    if cfg!(target_os = "macos") {
        default_dir()
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
/// cannot be swapped between the checks and the chmod. Its parent directories are checked
/// before anything is created, and again after, since another account may have created a
/// missing one in a sticky directory first.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    check_parents(dir)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    check_parents(dir)?;
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
/// later, or musl) refuses a link with EOPNOTSUPP. Where Linux does not support the flag at
/// all, a path that is still a real directory gets the path-based chmod instead.
fn chmod_nofollow(path: &Path, mode: libc::mode_t) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: c_path is a NUL-terminated string that lives for the call.
    if unsafe {
        libc::fchmodat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            mode,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let err = io::Error::last_os_error();
        // glibc before 2.32 refuses the flag for any path, and later glibc or musl without
        // /proc refuses it for a directory too. Fall back to the path-based chmod for what is
        // still a real directory, as before.
        let code = err.raw_os_error();
        if (code == Some(libc::ENOTSUP) || code == Some(libc::EOPNOTSUPP))
            && fs::symlink_metadata(path)?.is_dir()
        {
            // SAFETY: as above.
            if unsafe { libc::chmod(c_path.as_ptr(), mode) } != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        return Err(err);
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

    #[test]
    fn trusts_parent_directories_that_only_this_user_or_root_can_change() {
        let (me, other) = (501, 502);
        assert_eq!(judge_parent(me, 0o40755, me), Ok(false));
        assert_eq!(judge_parent(0, 0o40755, me), Ok(false));
        assert_eq!(judge_parent(me, 0o40700, me), Ok(false));
        assert_eq!(judge_parent(other, 0o40755, me), Err(Unsafe::Owner(other)));
        for mode in [0o777, 0o775, 0o757, 0o770] {
            assert_eq!(
                judge_parent(me, 0o40000 | mode, me),
                Err(Unsafe::Writable(mode)),
                "{mode:o}"
            );
        }
        // A sticky directory that others can write to (/tmp) is trusted when this user or
        // root owns it, and then so must be each entry passed in it.
        assert_eq!(judge_parent(0, 0o41777, me), Ok(true));
        assert_eq!(judge_parent(me, 0o41777, me), Ok(true));
        assert_eq!(judge_parent(me, 0o41755, me), Ok(false));
        assert_eq!(judge_parent(other, 0o41777, me), Err(Unsafe::Owner(other)));
        assert_eq!(judge_entry(true, me, me), Ok(()));
        assert_eq!(judge_entry(true, 0, me), Ok(()));
        assert_eq!(judge_entry(true, other, me), Err(Unsafe::Owner(other)));
        assert_eq!(judge_entry(false, other, me), Ok(()));
    }

    fn mkdir(path: &Path, mode: u32) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// `entry` is the directory as the check reached it, after links (/var is /private/var
    /// on macOS).
    #[track_caller]
    fn assert_path_refused(result: io::Result<impl std::fmt::Debug>, entry: &Path, why: &str) {
        let entry = fs::canonicalize(entry).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("{} {why}", entry.display()))
                && err.to_string().contains(
                    "use a data directory whose parent directories only this user or root can change"
                ),
            "{err}"
        );
    }

    #[test]
    fn accepts_safe_parents_and_checks_only_what_exists() {
        let temp = tempfile::tempdir().unwrap();
        // The temporary directory of the OS passes, as /tmp or /var/folders on macOS does.
        check_parents(&temp.path().join("herdvia")).unwrap();
        let parent = temp.path().join("parent");
        mkdir(&parent, 0o755);
        check_parents(&parent.join("herdvia")).unwrap();
        check_dir(&parent).unwrap();
        let missing = parent.join("a/b/herdvia");
        check_parents(&missing).unwrap();
        assert!(!parent.join("a").exists());
    }

    #[tokio::test]
    async fn refuses_parents_that_others_can_write_and_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        for mode in [0o777, 0o775, 0o757] {
            let open = temp.path().join(format!("open{mode:o}"));
            mkdir(&open, mode);
            let dir = open.join("a/herdvia");
            let why = format!("is writable by other users (mode {mode:o}, no sticky bit)");
            assert_path_refused(check_parents(&dir), &open, &why);
            assert_path_refused(prepare_dir(&dir), &open, &why);
            assert_path_refused(crate::peers::load_key(&dir), &open, &why);
            assert_path_refused(crate::peers::load(&dir), &open, &why);
            assert_path_refused(connect(&dir).await, &open, &why);
            assert!(listen(&dir).is_err());
            assert!(!open.join("a").exists(), "{mode:o}");
            // An existing data directory and its files are refused there too.
            let dir = open.join("herdvia");
            mkdir(&dir, 0o700);
            fs::write(dir.join("peers"), b"").unwrap();
            fs::set_permissions(dir.join("peers"), fs::Permissions::from_mode(0o600)).unwrap();
            assert_path_refused(check_dir(&dir), &open, &why);
            assert_path_refused(crate::peers::load(&dir), &open, &why);
            assert_path_refused(connect(&dir).await, &open, &why);
            assert!(listen(&dir).is_err());
            assert!(!dir.join("secret.key").exists() && !dir.join("agent.sock").exists());
        }
    }

    #[tokio::test]
    async fn refuses_dot_dot_after_a_missing_entry_and_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        // ".." after the missing entry leads back to shared, which the walk never reached.
        let shared = temp.path().join("shared");
        mkdir(&shared, 0o777);
        let dir = temp.path().join("missing/../shared/new/data");
        let missing = fs::canonicalize(temp.path()).unwrap().join("missing");
        let expected = format!(
            "the path to the data directory {} has \"..\" after {}, which does not exist, so the directories that \"..\" leads back to cannot be checked; write the path without \"..\"",
            dir.display(),
            missing.display()
        );
        let errors = [
            check_parents(&dir).unwrap_err(),
            check_dir(&dir).unwrap_err(),
            prepare_dir(&dir).unwrap_err(),
            crate::peers::load_key(&dir).unwrap_err(),
            crate::peers::load(&dir).unwrap_err(),
            connect(&dir).await.unwrap_err(),
            listen(&dir).err().unwrap(),
        ];
        for err in errors {
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
            assert_eq!(err.to_string(), expected);
        }
        assert!(!temp.path().join("missing").exists());
        assert!(fs::read_dir(&shared).unwrap().next().is_none());
        // Whatever the ".." leads back to, and after a missing link target too.
        let safe = temp.path().join("safe");
        mkdir(&safe, 0o755);
        let err = check_parents(&temp.path().join("missing/x/../../safe/herdvia")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        std::os::unix::fs::symlink("missing/../safe", temp.path().join("link")).unwrap();
        let err = check_parents(&temp.path().join("link/herdvia")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        // ".." between existing directories is followed as before, and a missing entry
        // without ".." after it still ends the check.
        check_parents(&safe.join("../safe/a/b/herdvia")).unwrap();
        check_parents(&safe.join("a/b/herdvia")).unwrap();
        assert!(!safe.join("a").exists());
    }

    #[test]
    fn accepts_the_default_data_directory_without_checking_its_parents() {
        let temp = tempfile::tempdir().unwrap();
        let open = temp.path().join("open");
        mkdir(&open, 0o777);
        let default = open.join("herdvia");
        let why = "is writable by other users (mode 777, no sticky bit)";
        check_parents_unless(&default, Some(&default)).unwrap();
        // As private_file passes it, the parent of the key.
        let key = default.join("secret.key");
        check_parents_unless(key.parent().unwrap(), Some(&default)).unwrap();
        // Another directory there, and the same one spelled another way, are checked.
        assert_path_refused(
            check_parents_unless(&open.join("other"), Some(&default)),
            &open,
            why,
        );
        let spelled = temp.path().join("open/../open/herdvia");
        assert_path_refused(check_parents_unless(&spelled, Some(&default)), &open, why);
        // Without a default (HOME is not set), and through check_parents, it is checked.
        assert_path_refused(check_parents_unless(&default, None), &open, why);
        assert_path_refused(check_parents(&default), &open, why);
    }

    #[tokio::test]
    async fn no_agent_stays_not_found_below_safe_parents() {
        let temp = tempfile::tempdir().unwrap();
        let err = connect(&temp.path().join("herdvia")).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        let err = connect(temp.path()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    #[test]
    fn accepts_this_users_directory_in_a_sticky_directory() {
        let temp = tempfile::tempdir().unwrap();
        let sticky = temp.path().join("sticky");
        mkdir(&sticky, 0o1777);
        check_parents(&sticky.join("herdvia")).unwrap();
        let mine = sticky.join("mine");
        mkdir(&mine, 0o700);
        let dir = mine.join("herdvia");
        prepare_dir(&dir).unwrap();
        crate::peers::load_key(&dir).unwrap();
        check_dir(&dir).unwrap();
    }

    #[test]
    fn follows_links_above_the_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        mkdir(&real, 0o755);
        let open = temp.path().join("open");
        mkdir(&open, 0o777);
        mkdir(&open.join("inner"), 0o755);
        // A link in a safe directory to a safe directory, by absolute or relative target.
        symlink(&real, temp.path().join("absolute")).unwrap();
        symlink("real", temp.path().join("relative")).unwrap();
        symlink("../real/.", real.join("up")).unwrap();
        for link in ["absolute", "relative", "real/up", "relative/up/up"] {
            let dir = temp.path().join(link).join("herdvia");
            check_parents(&dir).unwrap_or_else(|err| panic!("{link}: {err}"));
        }
        prepare_dir(&temp.path().join("relative/herdvia")).unwrap();
        assert!(real.join("herdvia").is_dir());
        // A link placed in a directory that others can write to, which they could replace.
        symlink(&real, open.join("link")).unwrap();
        let why = "is writable by other users (mode 777, no sticky bit)";
        assert_path_refused(check_parents(&open.join("link/herdvia")), &open, why);
        // A safe link whose target passes a directory that others can write to.
        symlink(open.join("inner"), temp.path().join("into")).unwrap();
        assert_path_refused(check_parents(&temp.path().join("into/herdvia")), &open, why);
        symlink("open/inner", temp.path().join("into2")).unwrap();
        assert_path_refused(
            check_parents(&temp.path().join("into2/herdvia")),
            &open,
            why,
        );
        // A link at the data directory to a safe directory passes; prepare_dir refuses it.
        check_parents(&temp.path().join("absolute")).unwrap();
        // A loop ends with an error.
        symlink("loop", temp.path().join("loop")).unwrap();
        let err = check_parents(&temp.path().join("loop/herdvia")).unwrap_err();
        assert!(err.to_string().contains("symbolic links"), "{err}");
        let err = check_parents(&temp.path().join("loop")).unwrap_err();
        assert!(err.to_string().contains("symbolic links"), "{err}");
    }

    #[tokio::test]
    async fn follows_a_link_at_the_data_directory() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let open = temp.path().join("open");
        mkdir(&open, 0o777);
        let target = open.join("x");
        mkdir(&target, 0o700);
        // A link in a safe directory whose target lies in a directory that others can write
        // to. A trailing "/" or "/." makes the kernel follow it where the path ends.
        let link = temp.path().join("final");
        symlink(&target, &link).unwrap();
        let why = "is writable by other users (mode 777, no sticky bit)";
        for suffix in ["", "/", "/."] {
            let mut dir = link.as_os_str().as_bytes().to_vec();
            dir.extend_from_slice(suffix.as_bytes());
            let dir = PathBuf::from(std::ffi::OsStr::from_bytes(&dir));
            assert_path_refused(check_parents(&dir), &open, why);
            assert_path_refused(prepare_dir(&dir), &open, why);
            assert_path_refused(check_dir(&dir), &open, why);
            assert_path_refused(crate::peers::load_key(&dir), &open, why);
            assert_path_refused(crate::peers::load(&dir), &open, why);
            assert_path_refused(connect(&dir).await, &open, why);
            assert!(listen(&dir).is_err(), "{suffix}");
        }
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        // The same link to a safe directory passes the check, with or without the slash.
        let real = temp.path().join("real");
        mkdir(&real, 0o700);
        fs::remove_file(&link).unwrap();
        symlink(&real, &link).unwrap();
        check_parents(&link).unwrap();
        check_parents(&link.join("")).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_a_parent_acl_that_allows_changing_its_entries() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        mkdir(&parent, 0o755);
        let dir = parent.join("herdvia");
        let why = "has an access control list (ACL) entry that allows adding, deleting, or changing entries or permissions there";
        for perm in [
            "add_file",
            "add_subdirectory",
            "delete",
            "delete_child",
            "writesecurity",
            "chown",
        ] {
            chmod(&["+a", &format!("everyone allow {perm}")], &parent);
            assert_path_refused(check_parents(&dir), &parent, why);
            assert_path_refused(prepare_dir(&dir), &parent, why);
            assert!(!dir.exists(), "{perm}");
            chmod(&["-N"], &parent);
        }
        // Read and search entries, and deny entries, leave the path to this user.
        chmod(
            &[
                "+a",
                "everyone allow list,search,readattr,readextattr,readsecurity",
            ],
            &parent,
        );
        chmod(&["+a", "everyone deny delete,add_file"], &parent);
        prepare_dir(&dir).unwrap();
        check_dir(&dir).unwrap();
        // The data directory keeps the stricter rule: any allow entry is refused.
        chmod(&["+a", "everyone allow search"], &dir);
        assert_acl_refused(check_dir(&dir), &dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_allow_entries_that_change_entries_change_the_path() {
        use acl::ACL_EXTENDED_ALLOW as ALLOW;
        let deny = 2;
        for bit in [2, 4, 5, 6, 12, 13] {
            assert!(changes_path(ALLOW, 1 << bit), "{bit}");
            assert!(!changes_path(deny, 1 << bit), "{bit}");
        }
        // list, search, readattr, writeattr, readextattr, writeextattr, readsecurity
        for bit in [1, 3, 7, 8, 9, 10, 11] {
            assert!(!changes_path(ALLOW, 1 << bit), "{bit}");
        }
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
