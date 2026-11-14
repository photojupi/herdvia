use super::{pipe_name::pipe_name, pipe_state};
use std::{
    env,
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    signal::windows::{CtrlC, ctrl_c},
};
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_PIPE_CONNECTED_STATE, FILE_PIPE_LOCAL_INFORMATION, FilePipeLocalInformation,
    NtQueryInformationFile,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_LOCK_VIOLATION, ERROR_PIPE_BUSY,
        HANDLE, LocalFree, STATUS_SUCCESS,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE,
            SetSecurityInfo,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetTokenInformation, INHERIT_ONLY_ACE, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, TOKEN_ELEVATION_TYPE, TOKEN_QUERY, TOKEN_USER, TokenElevationType,
        TokenElevationTypeFull, TokenUser, UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        LOCK_FILE_FLAGS, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        READ_CONTROL, SECURITY_ANONYMOUS, WRITE_DAC,
    },
    System::{
        IO::{IO_STATUS_BLOCK, OVERLAPPED},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

pub type ServerStream = NamedPipeServer;
pub type ClientStream = NamedPipeClient;
pub(crate) type Watcher = OwnedHandle;

pub struct Listener {
    next: NamedPipeServer,
    name: String,
    sid: String,
    _lock: File,
}

impl Listener {
    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        self.next.connect().await?;
        // Keep an instance alive continuously, including between connections.
        let next = create_pipe(&self.name, &self.sid, false)?;
        Ok(std::mem::replace(&mut self.next, next))
    }
}

pub fn listen(dir: &Path) -> io::Result<Listener> {
    prepare_dir(dir)?;
    // The pipe name follows how HERDVIA_DIR is spelled; this lock follows the directory.
    let lock = lock_agent(dir)?;
    let sid = current_sid()?;
    let name = name_for_dir(&sid, dir)?;
    let next = create_pipe(&name, &sid, true).map_err(|err| {
        if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            in_use(pipe_owner(&name).as_deref(), &sid)
        } else {
            err
        }
    })?;
    Ok(Listener {
        next,
        name,
        sid,
        _lock: lock,
    })
}

/// Holds agent.lock in the data directory for as long as the agent runs.
fn lock_agent(dir: &Path) -> io::Result<File> {
    // No FILE_SHARE_DELETE: the name cannot be removed while the lock is held.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir.join("agent.lock"))?;
    match lock_byte(&lock, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY) {
        Ok(()) => Ok(lock),
        Err(err) if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "agent is already running",
        )),
        Err(err) => Err(err),
    }
}

/// The error for a pipe name that already has a first instance, given its owner SID or why
/// it cannot be read. Only this user's own pipe means an agent: agent.lock is already held,
/// so it is an older agent that holds no lock.
fn in_use(owner: Result<&str, &io::Error>, sid: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        match owner {
            Ok(owner) if owner == sid => "agent is already running".to_owned(),
            Ok(owner) => format!(
                "another account ({owner}) holds this agent's pipe name; the agent cannot start until that account's process closes it"
            ),
            Err(err) => format!(
                "another process holds this agent's pipe name, and its owner cannot be read ({err}); the agent cannot start until that process closes it"
            ),
        },
    )
}

/// The owner SID of an existing pipe, connecting as an anonymous client: the server
/// may belong to another account, so it must not be able to impersonate this one.
/// Only READ_CONTROL is asked for, so a pipe that admits no reader still names its owner.
fn pipe_owner(name: &str) -> io::Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let pipe = loop {
        match OpenOptions::new()
            .access_mode(READ_CONTROL)
            .security_qos_flags(SECURITY_ANONYMOUS)
            .open(name)
        {
            Ok(pipe) => break pipe,
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && std::time::Instant::now() < deadline =>
            {
                // Every instance is connected, as between an agent's accept and its next one.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    };
    owner_sid(pipe.as_raw_handle(), SE_KERNEL_OBJECT)
}

pub async fn connect(dir: &Path) -> io::Result<ClientStream> {
    let sid = current_sid()?;
    let name = name_for_dir(&sid, dir)?;
    connect_named(&name, &sid).await
}

pub(crate) async fn connect_default() -> io::Result<ClientStream> {
    let sid = current_sid()?;
    connect_named(&pipe_name(&sid, None), &sid).await
}

async fn connect_named(name: &str, sid: &str) -> io::Result<ClientStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let stream = loop {
        match ClientOptions::new().open(name) {
            Ok(stream) => break stream,
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && tokio::time::Instant::now() < deadline =>
            {
                // Another client connected before the listener created its next instance.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(err) => return Err(err),
        }
    };
    check_owner(&stream, sid)?;
    Ok(stream)
}

pub(crate) fn check_peer(_stream: &ServerStream) -> io::Result<()> {
    // The pipe's protected DACL admits only this user; remote clients are rejected.
    Ok(())
}

pub(crate) fn watch_disconnect(stream: &ServerStream) -> io::Result<Watcher> {
    stream.as_handle().try_clone_to_owned()
}

pub(crate) async fn disconnected(watcher: Watcher) -> io::Error {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let state = {
            let mut info = FILE_PIPE_LOCAL_INFORMATION::default();
            let mut status_block = IO_STATUS_BLOCK::default();
            // This synchronous query consumes no data and reports CLOSING even
            // when unread bytes remain. No event or IOCP operation is required.
            // SAFETY: watcher owns the pipe handle. Both outputs are correctly
            // aligned windows-sys structures that live until the query completes;
            // the information class and buffer size match FILE_PIPE_LOCAL_INFORMATION.
            let status = unsafe {
                NtQueryInformationFile(
                    watcher.as_raw_handle(),
                    &mut status_block,
                    (&mut info as *mut FILE_PIPE_LOCAL_INFORMATION).cast(),
                    size_of::<FILE_PIPE_LOCAL_INFORMATION>() as u32,
                    FilePipeLocalInformation,
                )
            };
            if status == STATUS_SUCCESS {
                Ok(info.NamedPipeState == FILE_PIPE_CONNECTED_STATE)
            } else {
                Err(io::Error::other(format!(
                    "NtQueryInformationFile returned NTSTATUS 0x{:08X}",
                    status as u32
                )))
            }
        };
        if !pipe_state::connected(state).await {
            return io::Error::new(io::ErrorKind::ConnectionAborted, "peer disconnected");
        }
    }
}

pub(crate) struct Shutdown(CtrlC);

impl Shutdown {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self(ctrl_c()?))
    }

    pub(crate) async fn recv(&mut self) {
        self.0.recv().await;
    }
}

fn env_path(name: &str) -> io::Result<PathBuf> {
    env::var_os(name)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} is not set")))
}

pub fn home_dir() -> io::Result<PathBuf> {
    env_path("USERPROFILE")
}

pub fn data_dir() -> io::Result<PathBuf> {
    match env::var_os("HERDVIA_DIR") {
        Some(dir) => super::override_dir(dir),
        None => Ok(env_path("APPDATA")?.join("herdvia")),
    }
}

pub fn reject_root() -> io::Result<()> {
    Ok(())
}

/// Refuses the elevated half of a token that UAC split, as Unix refuses root. Without UAC (or
/// as the built-in Administrator) the token is always elevated and has no normal half to run
/// from instead, so the agent runs.
pub fn reject_elevated() -> io::Result<()> {
    if refused_elevation(elevation_type()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "herdvia agent must not run as administrator; run it from a normal terminal without administrator privileges",
        ));
    }
    Ok(())
}

fn refused_elevation(kind: TOKEN_ELEVATION_TYPE) -> bool {
    kind == TokenElevationTypeFull
}

/// The elevation type of this process's token: default, full or limited.
fn elevation_type() -> io::Result<TOKEN_ELEVATION_TYPE> {
    let mut token = null_mut();
    // SAFETY: GetCurrentProcess needs no arguments; token is writable and only
    // TOKEN_QUERY access is requested for the current process's token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken succeeded and transfers one owned handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut kind: TOKEN_ELEVATION_TYPE = 0;
    let mut returned = 0;
    // SAFETY: token is live; kind is an aligned, writable TOKEN_ELEVATION_TYPE
    // of the supplied size, and returned is a writable output pointer.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevationType,
            (&mut kind as *mut TOKEN_ELEVATION_TYPE).cast(),
            size_of_val(&kind) as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(kind)
}

const ADMINISTRATORS: &str = "S-1-5-32-544";
const LOCAL_SYSTEM: &str = "S-1-5-18";

/// Whether an account can be trusted with the data: an owner can always change the DACL
/// back, and an elevated user's new objects belong to Administrators. The same three are
/// the only accounts that a DACL may let in.
fn trusted_account(account: &str, sid: &str) -> bool {
    account == sid || account == ADMINISTRATORS || account == LOCAL_SYSTEM
}

/// The files that `private_file` opens in the data directory. `prepare_dir` restricts them
/// when their own DACL lets other accounts in.
const PRIVATE_FILES: [&str; 2] = ["secret.key", "peers"];

/// What `inspect_dir` and `inspect_file` say about a DACL that `prepare_dir` will restrict.
const RESTRICTED_AT_NEXT_RUN: &str = "it allows access to accounts other than this user, SYSTEM, and Administrators; herdvia restricts it the next time it runs (herdvia id, or the agent when it starts)";

/// The counterpart of chmod 700: checks the owner first, then sets a protected DACL unless
/// the directory has the one that `protect_dir` sets already. Then, when the directory has
/// it, makes the key and peers inherit only from it if their own DACL lets others in.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let sid = current_sid()?;
    let handle = open_entry(dir, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    check_entry(&handle, dir, true, &sid)?;
    let dacl = read_dacl(&handle, &sid)?;
    let protected = dacl.target || {
        match reopen(&handle, dir, READ_CONTROL | WRITE_DAC) {
            // SetSecurityInfo passes over an entry inside that this user cannot change;
            // private_file refuses the key or peers if they are left open.
            Ok(writable) => {
                protect_dir(&writable, &sid)?;
                true
            }
            // Private already, though not as protect_dir sets it: what it passes on to the
            // files inside is not known to admit this user, so they are left as they are.
            Err(err) if denied(&err) && dacl.private => false,
            Err(err) if denied(&err) => return Err(loose_dir(dir, &sid)),
            Err(err) => return Err(err),
        }
    };
    if protected {
        for name in PRIVATE_FILES {
            restrict_file(&dir.join(name), &sid);
        }
    }
    Ok(())
}

/// For doctor, read-only: the checks that `prepare_dir` makes, and a note when the DACL lets
/// other accounts in but `prepare_dir` will restrict it.
pub(crate) fn inspect_dir(dir: &Path) -> io::Result<Option<String>> {
    let sid = current_sid()?;
    let handle = open_entry(dir, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    check_entry(&handle, dir, true, &sid)?;
    if read_dacl(&handle, &sid)?.private {
        return Ok(None);
    }
    if writable(&handle, dir)? {
        Ok(Some(RESTRICTED_AT_NEXT_RUN.to_owned()))
    } else {
        Err(loose_dir(dir, &sid))
    }
}

/// Opens the key or peers for reading, refusing another kind of entry, an untrusted owner,
/// and a DACL that lets other accounts in. Changes nothing: `prepare_dir` restricts them.
pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let sid = current_sid()?;
    let file = open_file(path, &sid)?;
    if !read_dacl(&file, &sid)?.private {
        return Err(loose_file(path));
    }
    Ok(file)
}

/// For doctor, read-only: `private_file`, except that a DACL that `prepare_dir` will restrict
/// gives a note instead of an error.
pub(crate) fn inspect_file(path: &Path) -> io::Result<(File, Option<String>)> {
    let sid = current_sid()?;
    let file = open_file(path, &sid)?;
    if read_dacl(&file, &sid)?.private {
        return Ok((file, None));
    }
    // prepare_dir restricts it only when it leaves the directory as protect_dir sets it.
    let restricted = match path.parent() {
        Some(dir) => {
            let dir_handle = open_entry(dir, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
            read_dacl(&dir_handle, &sid)?.target || writable(&dir_handle, dir)?
        }
        None => false,
    };
    if restricted && writable(&file, path)? {
        Ok((file, Some(RESTRICTED_AT_NEXT_RUN.to_owned())))
    } else {
        Err(loose_file(path))
    }
}

fn open_file(path: &Path, sid: &str) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    check_entry(&file, path, false, sid)?;
    Ok(file)
}

/// Whether this user may change the DACL of `entry`, which is open at `path`.
fn writable(entry: &File, path: &Path) -> io::Result<bool> {
    match reopen(entry, path, READ_CONTROL | WRITE_DAC) {
        Ok(_) => Ok(true),
        Err(err) if denied(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

fn denied(err: &io::Error) -> bool {
    err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

fn loose_dir(dir: &Path, sid: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "data directory allows access to accounts other than this user, SYSTEM, and Administrators, and this user cannot change its permissions; in an administrator terminal run icacls \"{}\" /grant *{sid}:(OI)(CI)F and run herdvia again, or use a directory that this user created",
            dir.display()
        ),
    )
}

fn loose_file(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} allows access to accounts other than this user, SYSTEM, and Administrators, and herdvia cannot restrict it; in an administrator terminal run icacls \"{}\" /reset",
            path.display(),
            path.display()
        ),
    )
}

/// Makes a data file inherit only from the data directory, which has the DACL that
/// `protect_dir` sets, dropping its own entries and its protection, when its DACL lets other
/// accounts in. A file that cannot be opened, checked,
/// or changed is left as it is for `private_file` to refuse.
fn restrict_file(path: &Path, sid: &str) {
    let Ok(file) = open_entry(path, READ_CONTROL | FILE_READ_ATTRIBUTES) else {
        return;
    };
    let loose = read_dacl(&file, sid).is_ok_and(|dacl| !dacl.private);
    if !loose || check_entry(&file, path, false, sid).is_err() {
        return;
    }
    if let Ok(writable) = reopen(&file, path, READ_CONTROL | WRITE_DAC) {
        // An empty DACL that is not protected: only the entries the directory passes on.
        let _ = set_dacl(
            &writable,
            "D:",
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
        );
    }
}

/// Opens a file or directory itself, not the target of a link or junction.
fn open_entry(path: &Path, access: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens `path`, which `file` has open, again with other access, refusing it if the name
/// refers to another entry by now. (ReOpenFile is refused the WRITE_DAC that a new open by
/// path is given.)
fn reopen(file: &File, path: &Path, access: u32) -> io::Result<File> {
    let again = open_entry(path, access | FILE_READ_ATTRIBUTES)?;
    if file_id(&again)? != file_id(file)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} was replaced while herdvia checked it", path.display()),
        ));
    }
    Ok(again)
}

/// The volume and file index, which name one entry. `file` needs FILE_READ_ATTRIBUTES.
fn file_id(file: &File) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle; info is a writable output of the right type.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// Refuses a reparse point (a junction or link), the other kind of entry, and an owner
/// that `trusted_account` does not accept. `file` needs READ_CONTROL and FILE_READ_ATTRIBUTES.
fn check_entry(file: &File, path: &Path, directory: bool, sid: &str) -> io::Result<()> {
    let attributes = file.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            if directory {
                "data path must be a real directory, not a junction, symbolic link, or other reparse point".to_owned()
            } else {
                format!(
                    "{} must be a regular file, not a directory, link, or other reparse point",
                    path.display()
                )
            },
        ));
    }
    let owner = owner_sid(file.as_raw_handle(), SE_FILE_OBJECT)?;
    if !trusted_account(&owner, sid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            if directory {
                format!(
                    "data directory must be owned by this user, Administrators, or SYSTEM, not {owner}; use a directory that this user created"
                )
            } else {
                format!(
                    "{} must be owned by this user, Administrators, or SYSTEM, not {owner}",
                    path.display()
                )
            },
        ));
    }
    Ok(())
}

// Access control entry types, from winnt.h.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;

struct Dacl {
    /// Present, and every entry that allows access names an account that
    /// `trusted_account` accepts, inheritable entries included. Deny entries are fine.
    private: bool,
    /// Private and protected (not inherited from the parent), and gives full control to this
    /// user, SYSTEM, and Administrators, passed on to everything inside, as `protect_dir`
    /// sets it.
    target: bool,
}

/// The DACL of a file or directory, through a handle with READ_CONTROL access.
fn read_dacl(file: &File, sid: &str) -> io::Result<Dacl> {
    let mut acl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: file owns a live handle; acl and descriptor are writable outputs, and the
    // unused optional outputs are null.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    // acl points into descriptor, which lives until this function returns.
    let _allocation = LocalAllocation(descriptor);
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is a live security descriptor; both outputs are writable.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let protected = control & SE_DACL_PROTECTED != 0;
    let open = Dacl {
        private: false,
        target: false,
    };
    // A NULL DACL allows everyone everything.
    if acl.is_null() {
        return Ok(open);
    }
    // Full control, passed on to files and directories, for this user, SYSTEM, Administrators.
    let mut granted = [false; 3];
    // SAFETY: acl is a valid ACL inside descriptor.
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        // SAFETY: acl is valid and index is below its entry count; ace is writable.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every entry starts with an ACE_HEADER.
        let kind = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
        let trusted = match kind {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => true,
            ACCESS_ALLOWED_ACE_TYPE => {
                let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                // SAFETY: an entry of this type is an ACCESS_ALLOWED_ACE, whose SID
                // starts at SidStart and lies within the entry.
                let trustee = sid_string(unsafe { &raw mut (*ace).SidStart }.cast())?;
                // SAFETY: as above; the header and mask come first in the entry.
                let (flags, mask) = unsafe { (u32::from((*ace).Header.AceFlags), (*ace).Mask) };
                let inherit = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
                if flags & (inherit | INHERIT_ONLY_ACE) == inherit && mask == FILE_ALL_ACCESS {
                    for (found, account) in
                        granted.iter_mut().zip([sid, LOCAL_SYSTEM, ADMINISTRATORS])
                    {
                        *found |= trustee == account;
                    }
                }
                trusted_account(&trustee, sid)
            }
            // Other allow types (object, callback, compound) are not examined.
            _ => false,
        };
        if !trusted {
            return Ok(open);
        }
    }
    Ok(Dacl {
        private: true,
        target: protected && granted == [true; 3],
    })
}

/// Only this user, SYSTEM, and Administrators, inherited by everything created inside
/// (including the key and peers, written through temporary files) and not from the parent.
fn protect_dir(dir: &File, sid: &str) -> io::Result<()> {
    set_dacl(
        dir,
        &format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
    )
}

/// Sets the DACL in `sddl` through a handle opened with WRITE_DAC, which also passes
/// inheritable entries on to what is inside a directory.
fn set_dacl(file: &File, sddl: &str, information: u32) -> io::Result<()> {
    let descriptor = security_descriptor(sddl)?;
    let mut present = 0;
    let mut dacl = null_mut();
    let mut defaulted = 0;
    // SAFETY: descriptor is a live security descriptor; the outputs are writable. dacl
    // points into descriptor, which outlives its use below.
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() {
        return Err(io::Error::other("DACL is missing"));
    }
    // SAFETY: file owns a handle opened with WRITE_DAC; dacl is a valid ACL, and the
    // owner, group, and SACL are left unchanged (null).
    let result = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            information,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

pub(crate) fn lock_file(lock: &File) -> io::Result<()> {
    lock_byte(lock, LOCKFILE_EXCLUSIVE_LOCK)
}

fn lock_byte(lock: &File, flags: LOCK_FILE_FLAGS) -> io::Result<()> {
    let mut overlapped = OVERLAPPED::default();
    // Windows locks are mandatory. Lock the fixed byte at 4 GiB, well beyond
    // the 32-byte key and read_to_end's read buffer, so id/agent can still read it.
    overlapped.Anonymous.Anonymous.Offset = 0;
    overlapped.Anonymous.Anonymous.OffsetHigh = 1;
    // SAFETY: lock owns a synchronous file handle and overlapped remains alive
    // for this synchronous call. Closing lock releases the byte-range lock.
    if unsafe { LockFileEx(lock.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn name_for_dir(sid: &str, dir: &Path) -> io::Result<String> {
    let directory = env::var_os("HERDVIA_DIR")
        .map(|_| std::path::absolute(dir).map(|p| p.as_os_str().encode_wide().collect::<Vec<_>>()))
        .transpose()?;
    Ok(pipe_name(sid, directory.as_deref()))
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: this pointer was allocated by a Win32 API requiring LocalFree,
        // and this wrapper is its only owner.
        unsafe { LocalFree(self.0) };
    }
}

fn sid_string(sid: PSID) -> io::Result<String> {
    if sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "missing owner SID",
        ));
    }
    let mut text = null_mut();
    // SAFETY: callers supply a SID from a live token/security descriptor;
    // text points to writable storage for the API-allocated string pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(text.cast());
    // SAFETY: successful ConvertSidToStringSidW returns a NUL-terminated UTF-16
    // string, retained by _allocation until this function returns.
    let string = unsafe {
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        std::slice::from_raw_parts(text, length)
    };
    String::from_utf16(string).map_err(io::Error::other)
}

fn current_sid() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: GetCurrentProcess needs no arguments; token is writable and only
    // TOKEN_QUERY access is requested for the current process's token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken succeeded and transfers one owned handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0;
    // SAFETY: a null buffer and zero length request the required buffer size;
    // token is valid and needed is writable.
    let result = unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed)
    };
    if result != 0
        || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
    {
        return Err(io::Error::last_os_error());
    }
    // usize alignment accommodates TOKEN_USER, which contains a SID pointer.
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and has at least needed bytes; the token and
    // returned length pointer remain valid for the call.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size_of_val(buffer.as_slice()) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser query initialized TOKEN_USER at the start
    // of the aligned buffer; its embedded SID stays alive through sid_string.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    sid_string(user.User.Sid)
}

fn security_descriptor(sddl: &str) -> io::Result<LocalAllocation> {
    let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut descriptor = null_mut();
    // SAFETY: sddl is NUL-terminated; descriptor is a valid output location.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

fn create_pipe(name: &str, sid: &str, first: bool) -> io::Result<NamedPipeServer> {
    // Set the owner explicitly, including for elevated users whose token's
    // default owner might otherwise be the Administrators group.
    let descriptor = security_descriptor(&format!("O:{sid}D:P(A;;GA;;;{sid})"))?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: attributes and its valid security descriptor remain alive for the
    // call. CreateNamedPipe copies the descriptor and returns a non-inheritable handle.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .in_buffer_size(65_536)
            .out_buffer_size(65_536)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

/// The owner SID of an object, through a handle with READ_CONTROL access.
fn owner_sid(handle: HANDLE, kind: SE_OBJECT_TYPE) -> io::Result<String> {
    let mut owner = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: callers pass a live handle of this object type; owner and descriptor
    // are writable outputs, and all unused optional outputs are null.
    let result = unsafe {
        GetSecurityInfo(
            handle,
            kind,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    // owner points into descriptor, which lives until sid_string returns.
    let _allocation = LocalAllocation(descriptor);
    sid_string(owner)
}

fn check_owner(stream: &ClientStream, sid: &str) -> io::Result<()> {
    // ClientOptions opens with GENERIC_READ, which includes READ_CONTROL.
    if owner_sid(stream.as_raw_handle(), SE_KERNEL_OBJECT)? != sid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pipe owner SID does not match",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        Foundation::ERROR_PRIVILEGE_NOT_HELD,
        Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW,
    };

    /// The DACL of a file or directory in SDDL.
    fn dacl(path: &Path) -> String {
        let handle = open_entry(path, READ_CONTROL).unwrap();
        let mut descriptor = null_mut();
        // SAFETY: handle is live with READ_CONTROL; descriptor is a writable output.
        let result = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(result, 0);
        let _descriptor = LocalAllocation(descriptor);
        let mut text = null_mut();
        // SAFETY: descriptor is live; text is a writable output freed below.
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    null_mut(),
                )
            },
            0
        );
        let _text = LocalAllocation(text.cast());
        // SAFETY: a successful conversion returns a NUL-terminated UTF-16 string.
        let units = unsafe {
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            std::slice::from_raw_parts(text, length)
        };
        String::from_utf16(units).unwrap()
    }

    #[test]
    fn trusts_this_user_administrators_and_system_only() {
        for owner in ["S-1-5-21-1", ADMINISTRATORS, LOCAL_SYSTEM] {
            assert!(trusted_account(owner, "S-1-5-21-1"), "{owner}");
        }
        for owner in ["S-1-5-21-2", "S-1-1-0", "S-1-5-32-545", "S-1-5-11"] {
            assert!(!trusted_account(owner, "S-1-5-21-1"), "{owner}");
        }
    }

    #[test]
    fn names_another_account_that_holds_the_pipe_name() {
        assert_eq!(
            in_use(Ok("S-1-5-21-1"), "S-1-5-21-1").to_string(),
            "agent is already running"
        );
        let other = in_use(Ok("S-1-5-21-2"), "S-1-5-21-1");
        assert_eq!(other.kind(), io::ErrorKind::AddrInUse);
        assert!(
            other.to_string().contains("another account (S-1-5-21-2)"),
            "{other}"
        );
        // An owner that cannot be read is not taken for this user's agent.
        let denied = io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);
        let unknown = in_use(Err(&denied), "S-1-5-21-1");
        assert_eq!(unknown.kind(), io::ErrorKind::AddrInUse);
        assert!(
            unknown.to_string().starts_with(&format!(
                "another process holds this agent's pipe name, and its owner cannot be read ({denied})"
            )),
            "{unknown}"
        );
    }

    /// A pipe name of its own, never the agent's.
    fn test_pipe_name() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(r"\\.\pipe\herdvia-test-{}-{nanos}", std::process::id())
    }

    #[tokio::test]
    async fn reads_the_owner_of_a_pipe_with_read_control_only() {
        let sid = current_sid().unwrap();
        let name = test_pipe_name();
        let _own = create_pipe(&name, &sid, true).unwrap();
        assert_eq!(pipe_owner(&name).unwrap(), sid);
        let squat = |name: &str, sddl: &str| {
            let descriptor = security_descriptor(sddl).unwrap();
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            // SAFETY: attributes and its descriptor live for the call.
            unsafe {
                ServerOptions::new().create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
            }
            .unwrap()
        };
        // A pipe that admits no reader (CC is FILE_READ_DATA) still names its owner.
        let name = test_pipe_name();
        let _no_reader = squat(&name, &format!("O:{sid}D:P(D;;CC;;;{sid})(A;;GA;;;{sid})"));
        assert_eq!(pipe_owner(&name).unwrap(), sid);
        // A pipe that lets no one read its security descriptor, as another account's can.
        let name = test_pipe_name();
        let _squatter = squat(
            &name,
            &format!("O:{sid}D:P(D;;RC;;;OW)(D;;RC;;;{sid})(A;;GA;;;{sid})"),
        );
        let err = create_pipe(&name, &sid, true).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
        let owner = pipe_owner(&name);
        assert_eq!(
            owner.as_ref().unwrap_err().raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32)
        );
        assert!(
            in_use(owner.as_deref(), &sid)
                .to_string()
                .contains("its owner cannot be read")
        );
    }

    #[tokio::test]
    async fn waits_for_a_busy_pipe_to_read_its_owner() {
        let sid = current_sid().unwrap();
        let name = test_pipe_name();
        let _first = create_pipe(&name, &sid, true).unwrap();
        // A client takes the only instance, as between an agent's accept and its next one.
        let _client = ClientOptions::new().open(&name).unwrap();
        let runtime = tokio::runtime::Handle::current();
        let next = std::thread::spawn({
            let (name, sid) = (name.clone(), sid.clone());
            move || {
                std::thread::sleep(Duration::from_millis(200));
                let _runtime = runtime.enter();
                create_pipe(&name, &sid, false).unwrap()
            }
        });
        assert_eq!(pipe_owner(&name).unwrap(), sid);
        drop(next.join().unwrap());
    }

    /// Sets a protected DACL exactly, replacing the explicit copies of its parent's entries
    /// that a test directory may carry (icacls /inheritance:r keeps those).
    fn set_test_dacl(path: &Path, sddl: &str) {
        let entry = open_entry(path, READ_CONTROL | WRITE_DAC).unwrap();
        set_dacl(
            &entry,
            sddl,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        )
        .unwrap();
    }

    fn icacls(path: &Path, args: &[&str]) {
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "icacls {} {args:?}", path.display());
    }

    /// Allow entries for this user, SYSTEM, and Administrators only.
    fn assert_private(path: &Path, sid: &str) {
        let text = dacl(path);
        assert_eq!(text.matches("(A;").count(), 3, "{}: {text}", path.display());
        for account in [sid, "SY", "BA"] {
            assert!(
                text.contains(&format!(";;;{account})")),
                "{}: {text}",
                path.display()
            );
        }
    }

    #[test]
    fn protects_the_directory_and_its_new_files_inherit_that() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        // An installation from before: the directory, the key, and peers inherit a grant
        // to Users from the parent.
        icacls(&dir, &["/grant", "*S-1-5-32-545:(OI)(CI)R"]);
        let old = dir.join("old");
        fs::write(&old, b"x").unwrap();
        let key = iroh::SecretKey::generate();
        fs::write(dir.join("secret.key"), key.to_bytes()).unwrap();
        fs::write(dir.join("peers"), b"").unwrap();
        assert!(dacl(&old).contains(";;;BU)"), "{}", dacl(&old));
        // doctor tells, and changes nothing.
        assert_eq!(
            inspect_dir(&dir).unwrap().as_deref(),
            Some(RESTRICTED_AT_NEXT_RUN)
        );
        let (_, note) = inspect_file(&dir.join("secret.key")).unwrap();
        assert_eq!(note.as_deref(), Some(RESTRICTED_AT_NEXT_RUN));
        assert!(private_file(&dir.join("secret.key")).is_err());
        assert!(dacl(&dir).contains(";;;BU)"), "{}", dacl(&dir));
        prepare_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // SetSecurityInfo propagates the new inheritable ACEs to existing children.
        for path in [old.clone(), dir.join("secret.key"), dir.join("peers")] {
            assert_private(&path, &sid);
        }
        // SetSecurityInfo also marks the DACL auto-inherited (AI).
        let protected = dacl(&dir);
        assert!(protected.starts_with("D:P"), "{protected}");
        assert_private(&dir, &sid);
        for ace in [
            format!("(A;OICI;FA;;;{sid})"),
            "(A;OICI;FA;;;SY)".into(),
            "(A;OICI;FA;;;BA)".into(),
        ] {
            assert!(protected.contains(&ace), "{protected}");
        }
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        // The key and peers are written through temporary files and renamed.
        fs::remove_file(dir.join("secret.key")).unwrap();
        let key = crate::peers::load_key(&dir).unwrap();
        let peer = crate::peers::Peer {
            name: "work".into(),
            id: key.public(),
            permission: crate::peers::Permission::Both,
        };
        crate::peers::allow(&dir, peer.clone()).unwrap();
        assert_eq!(crate::peers::load(&dir).unwrap(), vec![peer]);
        for name in ["secret.key", "peers"] {
            let path = dir.join(name);
            private_file(&path).unwrap();
            assert_eq!(inspect_file(&path).unwrap().1, None);
            assert_private(&path, &sid);
        }
        // Again on an existing directory.
        prepare_dir(&dir).unwrap();
    }

    #[test]
    fn restricts_a_file_with_entries_of_its_own() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        let key = crate::peers::load_key(&dir).unwrap();
        crate::peers::allow(
            &dir,
            crate::peers::Peer {
                name: "work".into(),
                id: key.public(),
                permission: crate::peers::Permission::Both,
            },
        )
        .unwrap();
        let sid = current_sid().unwrap();
        let (secret, peers) = (dir.join("secret.key"), dir.join("peers"));
        // An explicit grant to Users, which propagation from the directory leaves alone.
        icacls(&secret, &["/grant", "*S-1-5-32-545:R"]);
        // Inheritance removed: the directory reaches it no longer.
        icacls(&peers, &["/inheritance:r", "/grant", "*S-1-5-32-545:R"]);
        let before = [dacl(&secret), dacl(&peers)];
        assert!(before[1].starts_with("D:P"), "{}", before[1]);
        for path in [&secret, &peers] {
            let err = private_file(path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                err.to_string()
                    .ends_with(&format!("icacls \"{}\" /reset", path.display())),
                "{err}"
            );
            let (_, note) = inspect_file(path).unwrap();
            assert_eq!(note.as_deref(), Some(RESTRICTED_AT_NEXT_RUN));
        }
        // Read-only: inspecting and refusing changed nothing.
        assert_eq!([dacl(&secret), dacl(&peers)], before);
        // The next run restricts both, and they inherit from the directory only.
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        for path in [&secret, &peers] {
            let text = dacl(path);
            assert!(!text.starts_with("D:P"), "{text}");
            assert_eq!(text.matches("(A;ID;").count(), 3, "{text}");
            assert_private(path, &sid);
            assert_eq!(inspect_file(path).unwrap().1, None);
        }
        assert_eq!(crate::peers::load(&dir).unwrap().len(), 1);
    }

    #[test]
    fn leaves_a_protected_directory_alone() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        prepare_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // Private, but this user may not change its permissions, as when SYSTEM or
        // Administrators own it and this user may only modify it. OWNER RIGHTS takes the
        // owner's implicit WRITE_DAC away.
        icacls(
            &dir,
            &[
                "/deny",
                &format!("*{sid}:(WDAC)"),
                "/deny",
                "*S-1-3-4:(WDAC)",
            ],
        );
        assert!(denied(
            &open_entry(&dir, READ_CONTROL | WRITE_DAC).unwrap_err()
        ));
        let before = dacl(&dir);
        // Deny entries leave it as protect_dir sets it.
        assert!(
            read_dacl(&open_entry(&dir, READ_CONTROL).unwrap(), &sid)
                .unwrap()
                .target,
            "{before}"
        );
        prepare_dir(&dir).unwrap();
        assert_eq!(dacl(&dir), before);
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        crate::peers::load_key(&dir).unwrap();
        let secret = dir.join("secret.key");
        private_file(&secret).unwrap();
        // The key, which this user may change, is still restricted.
        icacls(&secret, &["/grant", "*S-1-5-32-545:R"]);
        assert_eq!(
            inspect_file(&secret).unwrap().1.as_deref(),
            Some(RESTRICTED_AT_NEXT_RUN)
        );
        prepare_dir(&dir).unwrap();
        private_file(&secret).unwrap();
        assert_private(&secret, &sid);
    }

    #[test]
    fn rewrites_a_protected_directory_that_passes_nothing_on() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // Private, but nothing created inside inherits this user's access.
        set_test_dacl(&dir, &format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)"));
        let before = dacl(&dir);
        let state = read_dacl(&open_entry(&dir, READ_CONTROL).unwrap(), &sid).unwrap();
        assert!(state.private && !state.target, "{before}");
        // Safe as it is, so doctor does not complain.
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        prepare_dir(&dir).unwrap();
        assert_private(&dir, &sid);
        assert!(
            dacl(&dir).contains(&format!("(A;OICI;FA;;;{sid})")),
            "{}",
            dacl(&dir)
        );
        // The new key inherits from it, and a second run still reads it.
        let key = crate::peers::load_key(&dir).unwrap();
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        private_file(&dir.join("secret.key")).unwrap();
        assert_private(&dir.join("secret.key"), &sid);
    }

    #[test]
    fn uses_a_private_directory_it_cannot_change_as_it_is() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // As when SYSTEM owns it and this user may only modify it: OWNER RIGHTS (OW) takes
        // the owner's implicit WRITE_DAC away.
        set_test_dacl(
            &dir,
            &format!("D:P(D;;WD;;;{sid})(D;;WD;;;OW)(A;OICI;0x1301bf;;;{sid})(A;OICI;FA;;;SY)"),
        );
        let before = dacl(&dir);
        assert!(denied(
            &open_entry(&dir, READ_CONTROL | WRITE_DAC).unwrap_err()
        ));
        prepare_dir(&dir).unwrap();
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        let key = crate::peers::load_key(&dir).unwrap();
        let secret = dir.join("secret.key");
        private_file(&secret).unwrap();
        // What the directory passes on is not protect_dir's, so the key is not made to
        // inherit from it alone; the agent and doctor both refuse it with the fix.
        icacls(&secret, &["/grant", "*S-1-5-32-545:R"]);
        let open = dacl(&secret);
        prepare_dir(&dir).unwrap();
        assert_eq!(dacl(&dir), before);
        assert_eq!(dacl(&secret), open);
        let expected = format!("icacls \"{}\" /reset", secret.display());
        for err in [
            crate::peers::load_key(&dir).unwrap_err(),
            inspect_file(&secret).unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(err.to_string().ends_with(&expected), "{err}");
        }
        icacls(&secret, &["/remove:g", "*S-1-5-32-545"]);
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
    }

    #[test]
    fn restricts_a_file_with_a_null_dacl() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        let key = crate::peers::load_key(&dir).unwrap();
        let sid = current_sid().unwrap();
        let secret = dir.join("secret.key");
        let file = open_entry(&secret, READ_CONTROL | WRITE_DAC).unwrap();
        // SAFETY: file is open with WRITE_DAC; a null DACL lets everyone in.
        let result = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null(),
                null(),
            )
        };
        assert_eq!(result, 0);
        drop(file);
        assert!(
            dacl(&secret).contains("NO_ACCESS_CONTROL"),
            "{}",
            dacl(&secret)
        );
        let err = private_file(&secret).unwrap_err();
        assert!(
            err.to_string()
                .ends_with(&format!("icacls \"{}\" /reset", secret.display())),
            "{err}"
        );
        assert_eq!(
            inspect_file(&secret).unwrap().1.as_deref(),
            Some(RESTRICTED_AT_NEXT_RUN)
        );
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        assert_eq!(dacl(&secret).matches("(A;ID;").count(), 3);
        assert_private(&secret, &sid);
    }

    #[test]
    fn refuses_an_open_directory_it_cannot_restrict() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        icacls(&dir, &["/grant", "*S-1-5-32-545:(OI)(CI)R"]);
        icacls(
            &dir,
            &[
                "/deny",
                &format!("*{sid}:(WDAC)"),
                "/deny",
                "*S-1-3-4:(WDAC)",
            ],
        );
        let before = dacl(&dir);
        for err in [
            prepare_dir(&dir).unwrap_err(),
            inspect_dir(&dir).unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                err.to_string().contains(&format!(
                    "icacls \"{}\" /grant *{sid}:(OI)(CI)F",
                    dir.display()
                )),
                "{err}"
            );
        }
        assert_eq!(dacl(&dir), before);
    }

    #[test]
    fn a_key_that_cannot_be_changed_is_refused_with_the_fix() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        icacls(&dir, &["/grant", "*S-1-5-32-545:(OI)(CI)R"]);
        let sid = current_sid().unwrap();
        let secret = dir.join("secret.key");
        fs::write(&secret, iroh::SecretKey::generate().to_bytes()).unwrap();
        icacls(
            &secret,
            &[
                "/deny",
                &format!("*{sid}:(WDAC)"),
                "/deny",
                "*S-1-3-4:(WDAC)",
            ],
        );
        let expected = format!("icacls \"{}\" /reset", secret.display());
        let err = inspect_file(&secret).unwrap_err();
        assert!(err.to_string().ends_with(&expected), "{err}");
        // The directory is restricted; the key it cannot change keeps the grant to Users.
        prepare_dir(&dir).unwrap();
        assert_private(&dir, &sid);
        assert!(dacl(&secret).contains(";;;BU)"), "{}", dacl(&secret));
        for err in [
            crate::peers::load_key(&dir).unwrap_err(),
            private_file(&secret).unwrap_err(),
            inspect_file(&secret).unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(err.to_string().ends_with(&expected), "{err}");
        }
    }

    #[test]
    fn refuses_a_junction_and_entries_of_the_wrong_kind() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::create_dir(&target).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            prepare_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            inspect_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let file = temp.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert_eq!(
            inspect_dir(&file).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // Without FILE_FLAG_BACKUP_SEMANTICS, CreateFileW itself refuses a directory.
        assert_eq!(
            private_file(&target).unwrap_err().raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32)
        );
        assert_eq!(
            private_file(&temp.path().join("none")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn refuses_a_file_symbolic_link() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"x").unwrap();
        let link = temp.path().join("secret.key");
        if let Err(err) = std::os::windows::fs::symlink_file(&target, &link) {
            // Creating symbolic links needs elevation or Developer Mode.
            assert_eq!(err.raw_os_error(), Some(ERROR_PRIVILEGE_NOT_HELD as i32));
            eprintln!("skipped: {err}");
            return;
        }
        let err = private_file(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("must be a regular file"), "{err}");
        private_file(&target).unwrap();
    }

    #[test]
    fn agent_lock_follows_the_directory_not_its_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        prepare_dir(&dir).unwrap();
        let lock = lock_agent(&dir).unwrap();
        let spelled = dir.to_string_lossy();
        for other in [
            PathBuf::from(spelled.to_uppercase()),
            PathBuf::from(format!("{spelled}\\")),
        ] {
            let err = lock_agent(&other).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{}", other.display());
            assert_eq!(err.to_string(), "agent is already running");
        }
        drop(lock);
        lock_agent(&dir).unwrap();
    }

    #[test]
    fn rejects_only_the_elevated_half_of_a_split_token() {
        use windows_sys::Win32::Security::{TokenElevationTypeDefault, TokenElevationTypeLimited};
        assert!(refused_elevation(TokenElevationTypeFull));
        // Without UAC, as the built-in Administrator, or as a standard user.
        assert!(!refused_elevation(TokenElevationTypeDefault));
        assert!(!refused_elevation(TokenElevationTypeLimited));
        assert_eq!(
            reject_elevated().is_err(),
            elevation_type().unwrap() == TokenElevationTypeFull
        );
    }
}
