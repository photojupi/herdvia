use super::{
    dir_security::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
        ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE, ADMINISTRATORS, Ace,
        LOCAL_SYSTEM, Label, Refusal, ancestor_refusal, needs_label,
    },
    pipe_name::pipe_name,
    pipe_state,
};
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
    path::{Component, Path, PathBuf, Prefix},
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
        GetSecurityDescriptorDacl, GetSecurityDescriptorSacl, GetTokenInformation,
        INHERIT_ONLY_ACE, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SYSTEM_MANDATORY_LABEL_ACE, TOKEN_ELEVATION_TYPE, TOKEN_QUERY,
        TOKEN_USER, TokenElevationType, TokenElevationTypeFull, TokenUser,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        LOCK_FILE_FLAGS, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        READ_CONTROL, SECURITY_ANONYMOUS, WRITE_DAC, WRITE_OWNER,
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
    check_ancestors(dir, &current_sid()?)?;
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

/// The data directory that `data_dir` gives without HERDVIA_DIR.
fn default_dir() -> io::Result<PathBuf> {
    Ok(env_path("APPDATA")?.join("herdvia"))
}

pub fn data_dir() -> io::Result<PathBuf> {
    match env::var_os("HERDVIA_DIR") {
        Some(dir) => super::override_dir(dir),
        None => default_dir(),
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

/// Whether an account can be trusted with the data: an owner can always change the DACL
/// back, and an elevated user's new objects belong to Administrators. The same three are
/// the only accounts that a DACL may let in.
fn trusted_account(account: &str, sid: &str) -> bool {
    account == sid || account == ADMINISTRATORS || account == LOCAL_SYSTEM
}

/// The files that `private_file` opens in the data directory. `prepare_dir` restricts them
/// when their own DACL lets other accounts in and this user may change it.
const PRIVATE_FILES: [&str; 2] = ["secret.key", "peers"];

/// What `inspect_dir` and `inspect_file` say about a DACL that `prepare_dir` will restrict.
const RESTRICTED_AT_NEXT_RUN: &str = "it allows access to accounts other than this user, SYSTEM, and Administrators; herdvia restricts it the next time it runs (herdvia id, or the agent when it starts)";

/// The counterpart of chmod 700: checks the folders above it (`check_ancestors`) and the
/// owner first, then sets a protected DACL unless the directory has the one that `protect_dir`
/// sets already. Then gives the key and peers a protected DACL of their own if their DACL lets
/// others in. Apart from that, gives the directory and the two files the integrity label that
/// `label` sets, where it can.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    let sid = current_sid()?;
    // Before anything is created or changed, as far as the folders exist.
    let complete = check_ancestors(dir, &sid)?;
    fs::create_dir_all(dir)?;
    if !complete {
        // The folders that create_dir_all made above the data directory.
        check_ancestors(dir, &sid)?;
    }
    let handle = open_entry(dir, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    check_entry(&handle, dir, true, &sid)?;
    let dacl = read_dacl(&handle, &sid)?;
    if !dacl.target {
        match reopen(&handle, dir, READ_CONTROL | WRITE_DAC) {
            // SetSecurityInfo passes over an entry inside that this user cannot change;
            // private_file refuses the key or peers if they are left open.
            Ok(writable) => protect_dir(&writable, &sid)?,
            // Private already, though not as protect_dir sets it: used as it is.
            Err(err) if denied(&err) && dacl.private => {}
            Err(err) if denied(&err) => return Err(loose_dir(dir, &sid)),
            Err(err) => return Err(err),
        }
    }
    // Apart from the DACL, and best effort: what cannot be labeled is used as it is.
    label(&handle, dir, true);
    for name in PRIVATE_FILES {
        let path = dir.join(name);
        restrict_file(&path, &sid);
        label_file(&path, &sid);
    }
    Ok(())
}

/// For doctor, read-only: the checks that `prepare_dir` makes, and a note when the DACL lets
/// other accounts in but `prepare_dir` will restrict it.
pub(crate) fn inspect_dir(dir: &Path) -> io::Result<Option<String>> {
    let sid = current_sid()?;
    check_ancestors(dir, &sid)?;
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
        return Err(loose_file(path, &sid));
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
    // prepare_dir restricts it whenever this user may change its DACL.
    if writable(&file, path)? {
        Ok((file, Some(RESTRICTED_AT_NEXT_RUN.to_owned())))
    } else {
        Err(loose_file(path, &sid))
    }
}

/// `private_file` for a command that reads the file without running `prepare_dir` first
/// (`herdvia peers`, `herdvia register`): a DACL that lets other accounts in is still
/// refused, but when `prepare_dir` would restrict it, the refusal says to let herdvia do so.
pub(crate) fn unprepared_private_file(path: &Path) -> io::Result<File> {
    let sid = current_sid()?;
    let file = open_file(path, &sid)?;
    if read_dacl(&file, &sid)?.private {
        return Ok(file);
    }
    // The same test as inspect_file.
    if writable(&file, path)? {
        Err(unrestricted_file(path))
    } else {
        Err(loose_file(path, &sid))
    }
}

/// Opens the key or peers after `check_ancestors` accepts the folders above the data
/// directory that holds it.
fn open_file(path: &Path, sid: &str) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        check_ancestors(dir, sid)?;
    }
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
            "data directory allows access to accounts other than this user, SYSTEM, and Administrators, and this user cannot change its permissions; in an administrator terminal run icacls \"{}\" /grant \"*{sid}:(OI)(CI)F\" and run herdvia again, or use a directory that this user created",
            dir.display()
        ),
    )
}

/// The next run gives the file a protected DACL of its own once this user has full control
/// of it, whatever the directory passes on.
fn loose_file(path: &Path, sid: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} allows access to accounts other than this user, SYSTEM, and Administrators, and herdvia cannot restrict it; in an administrator terminal run icacls \"{}\" /grant \"*{sid}:F\" and run herdvia again",
            path.display(),
            path.display()
        ),
    )
}

/// A file that `prepare_dir` would restrict, refused by a command that does not run it.
fn unrestricted_file(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} allows access to accounts other than this user, SYSTEM, and Administrators, and this command does not restrict it; run herdvia id in a normal terminal (or start the agent), which restricts it, and run this command again",
            path.display()
        ),
    )
}

/// Gives a data file a protected DACL that lets in only this user, SYSTEM, and
/// Administrators, when its DACL lets other accounts in. It does not depend on the directory,
/// which may have been replaced or pass on a deny entry, and it is set through the handle that
/// was checked. A file that cannot be opened with WRITE_DAC or checked is left as it is for
/// `private_file` to refuse.
fn restrict_file(path: &Path, sid: &str) {
    let Ok(file) = open_entry(path, READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES) else {
        return;
    };
    let loose = read_dacl(&file, sid).is_ok_and(|dacl| !dacl.private);
    if loose && check_entry(&file, path, false, sid).is_ok() {
        let _ = set_dacl(
            &file,
            &format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)"),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        );
    }
}

// The mandatory label entry type, from winnt.h.
const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 17;

/// The integrity label that `prepare_dir` gives the data directory and passes on to the files
/// created in it: Medium with NO_WRITE_UP and NO_READ_UP, so that a Low integrity process of
/// this user can neither write nor newly open and read them.
const DIR_LABEL: &str = "S:(ML;OICI;NWNR;;;ME)";
/// The same label for the key and peers themselves, which may not have been given the
/// directory's: it is not always passed on to entries that exist already.
const FILE_LABEL: &str = "S:(ML;;NWNR;;;ME)";

/// Gives the key or peers `FILE_LABEL` when `needs_label` says so, once it passes the kind and
/// owner checks that `restrict_file` makes. Best effort, as `label`.
fn label_file(path: &Path, sid: &str) {
    let Ok(file) = open_entry(path, READ_CONTROL | FILE_READ_ATTRIBUTES) else {
        return;
    };
    if check_entry(&file, path, false, sid).is_ok() {
        label(&file, path, false);
    }
}

/// Sets `DIR_LABEL` or `FILE_LABEL` as the label of `entry`, which is open at `path` with
/// READ_CONTROL and FILE_READ_ATTRIBUTES, when `needs_label` says so: WRITE_OWNER is only
/// asked for then, on an entry that `reopen` confirms. Best effort: an entry whose label
/// cannot be read or set (no WRITE_OWNER, as in a directory that SYSTEM owns and this user
/// may only modify) keeps its label, and nothing fails because of it.
fn label(entry: &File, path: &Path, directory: bool) {
    if !read_labels(entry).is_ok_and(|labels| needs_label(&labels, directory)) {
        return;
    }
    if let Ok(writable) = reopen(entry, path, READ_CONTROL | WRITE_OWNER) {
        let _ = set_label(&writable, if directory { DIR_LABEL } else { FILE_LABEL });
    }
}

/// The mandatory label entries of a file or directory, in order, through a handle with
/// READ_CONTROL access. Only the label is asked for, not the rest of the SACL, which takes a
/// privilege to read.
fn read_labels(file: &File) -> io::Result<Vec<Label>> {
    let mut acl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: file owns a live handle; acl and descriptor are writable outputs, and the
    // unused optional outputs are null.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut acl,
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    // acl points into descriptor, which lives until this function returns.
    let _allocation = LocalAllocation(descriptor);
    let mut labels = Vec::new();
    // No SACL: no label.
    if acl.is_null() {
        return Ok(labels);
    }
    // SAFETY: acl is a valid ACL inside descriptor.
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        // SAFETY: acl is valid and index is below its entry count; ace is writable.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        if header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE {
            continue;
        }
        let ace = ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>();
        // SAFETY: an entry of this type is a SYSTEM_MANDATORY_LABEL_ACE: the policy follows
        // the header, and the SID starts at SidStart and lies within the entry.
        let (policy, sid) = unsafe { ((*ace).Mask, &raw mut (*ace).SidStart) };
        labels.push(Label {
            flags: header.AceFlags,
            policy,
            sid: sid_string(sid.cast())?,
        });
    }
    Ok(labels)
}

/// Sets the label in `sddl`, an S: string of one mandatory label entry, through a handle
/// opened with WRITE_OWNER. Only the label changes: not the owner, the DACL, or the audit
/// entries of the SACL. A directory passes an inheritable label on to what is inside.
fn set_label(file: &File, sddl: &str) -> io::Result<()> {
    let descriptor = security_descriptor(sddl)?;
    let mut present = 0;
    let mut sacl = null_mut();
    let mut defaulted = 0;
    // SAFETY: descriptor is a live security descriptor; the outputs are writable. sacl
    // points into descriptor, which outlives its use below.
    if unsafe { GetSecurityDescriptorSacl(descriptor.0, &mut present, &mut sacl, &mut defaulted) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || sacl.is_null() {
        return Err(io::Error::other("SACL is missing"));
    }
    // SAFETY: file owns a handle opened with WRITE_OWNER; sacl is a valid ACL, and the
    // owner, group, and DACL are left unchanged (null).
    let result = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null(),
            sacl,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
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
pub(crate) fn file_id(file: impl AsHandle) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file borrows a live handle (a null one fails); info is a writable output of the
    // right type.
    if unsafe { GetFileInformationByHandle(file.as_handle().as_raw_handle(), &mut info) } == 0 {
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

/// Refuses a folder above the data directory `dir`, from the drive root down to its parent,
/// that would let another account swap the data directory for another after it is checked: a
/// reparse point, or a folder that `ancestor_refusal` refuses. Read-only. Only a path that
/// starts with a drive letter (C:\ or \\?\C:\) is checked; UNC and other paths are not.
/// Returns whether all of those folders exist: the check stops at the first missing one.
///
/// The built-in data directory (`default_dir`) is accepted without this check: an account
/// that could change the folders above the profile could already run programs as this user,
/// and refusing it there (a profile moved through a junction, for one) would leave the agent
/// no way to start.
fn check_ancestors(dir: &Path, sid: &str) -> io::Result<bool> {
    check_ancestors_unless(dir, sid, default_dir().ok().as_deref())
}

/// For doctor, read-only: `check_ancestors`, the check that the agent makes of the folders
/// above the data directory.
pub(crate) fn check_parents(dir: &Path) -> io::Result<()> {
    check_ancestors(dir, &current_sid()?).map(|_| ())
}

/// `check_ancestors`, except for `dir` spelled as `default`, the built-in data directory.
fn check_ancestors_unless(dir: &Path, sid: &str, default: Option<&Path>) -> io::Result<bool> {
    if default == Some(dir) {
        return Ok(true);
    }
    // As Win32 resolves the path for the calls that use it.
    let dir = std::path::absolute(dir)?;
    match dir.components().next() {
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
        _ => return Ok(true),
    }
    let mut folders: Vec<&Path> = dir.ancestors().skip(1).collect();
    folders.reverse();
    for folder in folders {
        let cannot_check = |err: io::Error| {
            io::Error::new(
                err.kind(),
                format!(
                    "cannot check {}, a folder above the data directory ({err}); use a data directory whose parent folders this user can read",
                    folder.display()
                ),
            )
        };
        let handle = match open_entry(folder, READ_CONTROL | FILE_READ_ATTRIBUTES) {
            Ok(handle) => handle,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(cannot_check(err)),
        };
        if handle.metadata().map_err(cannot_check)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a junction, symbolic link, or other reparse point above the data directory; use a data directory whose parent folders are all real directories",
                    folder.display()
                ),
            ));
        }
        let (owner, dacl) = read_owner_and_dacl(&handle).map_err(cannot_check)?;
        if let Some(refusal) = ancestor_refusal(&owner, dacl.as_deref(), sid) {
            return Err(unsafe_ancestor(folder, refusal));
        }
    }
    Ok(true)
}

/// What a refused folder above the data directory lets another account do, and the way out.
fn unsafe_ancestor(folder: &Path, refusal: Refusal) -> io::Error {
    let folder = folder.display();
    let reason = match refusal {
        Refusal::Owner(owner) => format!(
            "{folder} is owned by {owner}, not this user, SYSTEM, Administrators, or TrustedInstaller, so that account"
        ),
        Refusal::NullDacl => format!("{folder} has a NULL DACL, so any account"),
        Refusal::Grants(account) => format!(
            "{folder} lets {account} write to it (enough to turn it into a junction), or delete, rename, or change the permissions of it or of what is inside it, so that account"
        ),
        Refusal::Unexamined(kind) => format!(
            "{folder} has an allow entry of a type that herdvia does not examine ({kind}), so another account"
        ),
    };
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{reason} could swap the data directory inside it for another; remove that access, or use a data directory whose parent folders only this user, SYSTEM, Administrators, and TrustedInstaller can change"
        ),
    )
}

/// The owner and DACL entries (None for a NULL DACL) of a file or directory, through a handle
/// with READ_CONTROL access.
fn read_owner_and_dacl(file: &File) -> io::Result<(String, Option<Vec<Ace>>)> {
    let mut owner = null_mut();
    let mut acl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: file owns a live handle; owner, acl, and descriptor are writable outputs, and
    // the unused optional outputs are null.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    // owner and acl point into descriptor, which lives until this function returns.
    let _allocation = LocalAllocation(descriptor);
    let owner = sid_string(owner)?;
    if acl.is_null() {
        return Ok((owner, None));
    }
    let mut aces = Vec::new();
    // SAFETY: acl is a valid ACL inside descriptor.
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        // SAFETY: acl is valid and index is below its entry count; ace is writable.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let allowed = if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
            let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: an entry of this type is an ACCESS_ALLOWED_ACE: the mask follows the
            // header, and the SID starts at SidStart and lies within the entry.
            let (mask, trustee) = unsafe { ((*ace).Mask, &raw mut (*ace).SidStart) };
            Some((mask, sid_string(trustee.cast())?))
        } else {
            None
        };
        aces.push(Ace {
            kind: header.AceType,
            flags: header.AceFlags,
            allowed,
        });
    }
    Ok((owner, Some(aces)))
}

struct Dacl {
    /// Present, and every entry that allows access names an account that
    /// `trusted_account` accepts, inheritable entries included. Deny entries are fine.
    private: bool,
    /// Private and protected (not inherited from the parent), and gives full control to this
    /// user, SYSTEM, and Administrators, passed on to everything inside, as `protect_dir`
    /// sets it. A deny entry passed on to what is inside could lock this user out of it, so
    /// only deny entries that are not passed on are allowed.
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
    let inherit = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    let mut passes_deny = false;
    // SAFETY: acl is a valid ACL inside descriptor.
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        // SAFETY: acl is valid and index is below its entry count; ace is writable.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let trusted = match header.AceType {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {
                passes_deny |= u32::from(header.AceFlags) & inherit != 0;
                true
            }
            ACCESS_ALLOWED_ACE_TYPE => {
                let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                // SAFETY: an entry of this type is an ACCESS_ALLOWED_ACE, whose SID
                // starts at SidStart and lies within the entry.
                let trustee = sid_string(unsafe { &raw mut (*ace).SidStart }.cast())?;
                // SAFETY: as above; the header and mask come first in the entry.
                let (flags, mask) = unsafe { (u32::from((*ace).Header.AceFlags), (*ace).Mask) };
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
        target: protected && granted == [true; 3] && !passes_deny,
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
        sddl(path, DACL_SECURITY_INFORMATION)
    }

    /// The integrity label of a file or directory in SDDL, empty when it has none.
    fn label_text(path: &Path) -> String {
        sddl(path, LABEL_SECURITY_INFORMATION)
    }

    /// Part of the security descriptor of a file or directory in SDDL.
    fn sddl(path: &Path, information: u32) -> String {
        let handle = open_entry(path, READ_CONTROL).unwrap();
        let mut descriptor = null_mut();
        // SAFETY: handle is live with READ_CONTROL; descriptor is a writable output.
        let result = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                information,
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
                    information,
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
        let text = String::from_utf16(units).unwrap();
        // SDDL writes the built-in Administrator (RID 500) as LA, and GitHub's Windows runner
        // signs in as that account. Spell the SID out so that the tests can look for it.
        let sid = current_sid().unwrap();
        if sid.ends_with("-500") {
            text.replace(";;;LA)", &format!(";;;{sid})"))
        } else {
            text
        }
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

    /// A temporary folder for a test, in %LOCALAPPDATA%\herdvia-tests rather than %TEMP%.
    /// Every data directory needs parent folders that no other account may change, and %TEMP%
    /// may let other accounts change it (a sandbox's accounts, for one); %LOCALAPPDATA% has
    /// the same parents as the default data directory in %APPDATA%. A folder that a test left
    /// undeletable stays in herdvia-tests.
    fn test_temp() -> tempfile::TempDir {
        let base = env_path("LOCALAPPDATA").unwrap().join("herdvia-tests");
        fs::create_dir_all(&base).unwrap();
        if let Err(err) = check_ancestors(&base.join("herdvia"), &current_sid().unwrap()) {
            panic!("the Windows tests need a folder whose parent folders pass the check: {err}");
        }
        tempfile::tempdir_in(&base).unwrap()
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

    /// The fix that a refusal of the key or peers names.
    fn file_fix(path: &Path, sid: &str) -> String {
        format!(
            "icacls \"{}\" /grant \"*{sid}:F\" and run herdvia again",
            path.display()
        )
    }

    /// The protected DACL of its own that `restrict_file` gives a file.
    fn assert_restricted(path: &Path, sid: &str) {
        let text = dacl(path);
        assert!(text.starts_with("D:P"), "{}: {text}", path.display());
        assert!(!text.contains("ID;"), "{}: {text}", path.display());
        assert_private(path, sid);
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
        let temp = test_temp();
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
        let temp = test_temp();
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
            assert!(err.to_string().ends_with(&file_fix(path, &sid)), "{err}");
            let (_, note) = inspect_file(path).unwrap();
            assert_eq!(note.as_deref(), Some(RESTRICTED_AT_NEXT_RUN));
        }
        // Read-only: inspecting and refusing changed nothing.
        assert_eq!([dacl(&secret), dacl(&peers)], before);
        // The next run restricts both with a protected DACL of their own.
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        for path in [&secret, &peers] {
            assert_restricted(path, &sid);
            assert_eq!(inspect_file(path).unwrap().1, None);
        }
        assert_eq!(crate::peers::load(&dir).unwrap().len(), 1);
    }

    #[test]
    fn a_command_that_does_not_restrict_says_what_does() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        let key = crate::peers::load_key(&dir).unwrap();
        crate::peers::allow(
            &dir,
            crate::peers::Peer {
                name: "work".into(),
                id: key.public(),
                permission: crate::peers::Permission::Out,
            },
        )
        .unwrap();
        let sid = current_sid().unwrap();
        let peers = dir.join("peers");
        // As after an upgrade from v0.1.2: open, and this user may change its DACL.
        icacls(&peers, &["/grant", "*S-1-5-32-545:R"]);
        let before = dacl(&peers);
        let advice = "and this command does not restrict it; run herdvia id in a normal terminal (or start the agent), which restricts it, and run this command again";
        for err in [
            unprepared_private_file(&peers).unwrap_err(),
            crate::peers::load_unprepared(&dir).unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                err.to_string(),
                format!(
                    "{} allows access to accounts other than this user, SYSTEM, and Administrators, {advice}",
                    peers.display()
                )
            );
        }
        // What runs after prepare_dir keeps the administrator command.
        for err in [
            private_file(&peers).unwrap_err(),
            crate::peers::load(&dir).unwrap_err(),
        ] {
            assert!(err.to_string().ends_with(&file_fix(&peers, &sid)), "{err}");
        }
        // Refusing changed nothing.
        assert_eq!(dacl(&peers), before);
        // herdvia id restricts it, and the command reads it then.
        crate::peers::load_key(&dir).unwrap();
        assert_restricted(&peers, &sid);
        assert_eq!(crate::peers::load_unprepared(&dir).unwrap().len(), 1);
    }

    #[test]
    fn leaves_a_protected_directory_alone() {
        let temp = test_temp();
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
        let temp = test_temp();
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
        let temp = test_temp();
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
        // What the directory passes on is not protect_dir's, but the key's own DACL does not
        // depend on it: doctor says the next run restricts it, and it does.
        icacls(&secret, &["/grant", "*S-1-5-32-545:R"]);
        assert!(private_file(&secret).is_err());
        assert_eq!(
            inspect_file(&secret).unwrap().1.as_deref(),
            Some(RESTRICTED_AT_NEXT_RUN)
        );
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        assert_eq!(dacl(&dir), before);
        assert_restricted(&secret, &sid);
        assert_eq!(inspect_file(&secret).unwrap().1, None);
    }

    #[test]
    fn restricts_a_key_in_a_directory_that_passes_a_deny_entry_on() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        let secret = dir.join("secret.key");
        let key = iroh::SecretKey::generate();
        fs::write(&secret, key.to_bytes()).unwrap();
        // A protected key with a grant to Users of its own.
        set_test_dacl(&secret, &format!("D:P(A;;FA;;;{sid})(A;;FR;;;BU)"));
        // A private directory, which this user may not change, that passes on to the files
        // inside a deny of reading to Everyone.
        set_test_dacl(
            &dir,
            &format!(
                "D:P(D;OIIO;FR;;;WD)(D;;WD;;;{sid})(D;;WD;;;OW)(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
            ),
        );
        let before = dacl(&dir);
        let state = read_dacl(&open_entry(&dir, READ_CONTROL).unwrap(), &sid).unwrap();
        assert!(state.private && !state.target, "{before}");
        assert!(denied(
            &open_entry(&dir, READ_CONTROL | WRITE_DAC).unwrap_err()
        ));
        assert!(private_file(&secret).is_err());
        assert_eq!(
            inspect_file(&secret).unwrap().1.as_deref(),
            Some(RESTRICTED_AT_NEXT_RUN)
        );
        // The key keeps its own entries only, so this user still reads it.
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        assert_eq!(dacl(&dir), before);
        assert_restricted(&secret, &sid);
    }

    #[test]
    fn rewrites_a_directory_that_passes_a_deny_entry_on() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // As protect_dir sets it, except for a deny of reading to Everyone that the files
        // inside inherit, which would lock this user out of a new key.
        set_test_dacl(
            &dir,
            &format!("D:P(D;OIIO;FR;;;WD)(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
        );
        let state = read_dacl(&open_entry(&dir, READ_CONTROL).unwrap(), &sid).unwrap();
        assert!(state.private && !state.target, "{}", dacl(&dir));
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        prepare_dir(&dir).unwrap();
        let text = dacl(&dir);
        assert!(!text.contains("(D;"), "{text}");
        assert!(
            read_dacl(&open_entry(&dir, READ_CONTROL).unwrap(), &sid)
                .unwrap()
                .target,
            "{text}"
        );
        let key = crate::peers::load_key(&dir).unwrap();
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        assert_private(&dir.join("secret.key"), &sid);
    }

    #[test]
    fn restricts_a_file_with_a_null_dacl() {
        let temp = test_temp();
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
        assert!(err.to_string().ends_with(&file_fix(&secret, &sid)), "{err}");
        assert_eq!(
            inspect_file(&secret).unwrap().1.as_deref(),
            Some(RESTRICTED_AT_NEXT_RUN)
        );
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        assert_restricted(&secret, &sid);
    }

    #[test]
    fn refuses_an_open_directory_it_cannot_restrict() {
        let temp = test_temp();
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
                    "icacls \"{}\" /grant \"*{sid}:(OI)(CI)F\" and run herdvia again",
                    dir.display()
                )),
                "{err}"
            );
        }
        assert_eq!(dacl(&dir), before);
    }

    #[test]
    fn a_key_that_cannot_be_changed_is_refused_with_the_fix() {
        let temp = test_temp();
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
        let expected = file_fix(&secret, &sid);
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
            // Without prepare_dir, which could not restrict it either.
            unprepared_private_file(&secret).unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(err.to_string().ends_with(&expected), "{err}");
        }
    }

    #[test]
    fn the_icacls_commands_paste_into_powershell_and_cmd() {
        use std::os::windows::process::CommandExt;
        let temp = test_temp();
        let sid = current_sid().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let file = temp.path().join("secret.key");
        fs::write(&file, b"x").unwrap();
        // The command in a message, as a user copies it.
        let command = |message: io::Error| {
            let message = message.to_string();
            let start = message.find("run icacls ").unwrap() + "run ".len();
            let end = message.find(" and run herdvia again").unwrap();
            message[start..end].to_owned()
        };
        // This user is only let open them (SYNCHRONIZE, READ_CONTROL, FILE_READ_ATTRIBUTES)
        // but, as the owner, may change their DACL all the same.
        let cases = [
            (
                &dir,
                command(loose_dir(&dir, &sid)),
                format!("D:P(A;;0x120080;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                format!("(A;OICI;FA;;;{sid})"),
            ),
            (
                &file,
                command(loose_file(&file, &sid)),
                format!("D:P(A;;0x120080;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)"),
                format!("(A;;FA;;;{sid})"),
            ),
        ];
        for (path, command, start, granted) in cases {
            for shell in ["powershell", "cmd"] {
                set_test_dacl(path, &start);
                assert!(!dacl(path).contains(&granted), "{}", dacl(path));
                let mut run = std::process::Command::new(shell);
                if shell == "powershell" {
                    run.args(["-NoProfile", "-Command"]).arg(&command);
                } else {
                    // As typed: cmd /C keeps the quotes of a line that does not start with one.
                    run.arg("/C").raw_arg(&command);
                }
                let output = run.output().unwrap();
                assert!(
                    output.status.success(),
                    "{shell}: {command}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(dacl(path).contains(&granted), "{shell}: {}", dacl(path));
            }
        }
    }

    #[test]
    fn refuses_a_junction_and_entries_of_the_wrong_kind() {
        let temp = test_temp();
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
        let temp = test_temp();
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
        let temp = test_temp();
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

    #[test]
    fn the_folder_rules_use_the_values_in_winnt_h() {
        use super::super::dir_security as rules;
        use windows_sys::Win32::{
            Foundation::{GENERIC_ALL, GENERIC_WRITE},
            Storage::FileSystem::{
                DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_APPEND_DATA, FILE_DELETE_CHILD,
                FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, WRITE_OWNER,
            },
        };
        assert_eq!(
            rules::PATH_CHANGING,
            DELETE
                | FILE_DELETE_CHILD
                | FILE_WRITE_DATA
                | FILE_WRITE_ATTRIBUTES
                | WRITE_DAC
                | WRITE_OWNER
                | GENERIC_ALL
                | GENERIC_WRITE
        );
        // Adding files is writing a folder's data; adding folders is appending to it, which
        // the rules let other accounts do.
        assert_eq!(FILE_ADD_FILE, FILE_WRITE_DATA);
        assert_eq!(FILE_ADD_SUBDIRECTORY, FILE_APPEND_DATA);
        assert_eq!(rules::PATH_CHANGING & FILE_ADD_SUBDIRECTORY, 0);
        assert_eq!(u32::from(rules::INHERIT_ONLY_ACE), INHERIT_ONLY_ACE);
    }

    fn junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J {}", link.display());
    }

    /// Every entry point that reads or writes the key, peers, or agent.lock in `dir`.
    fn entry_points(dir: &Path) -> Vec<io::Error> {
        vec![
            prepare_dir(dir).unwrap_err(),
            inspect_dir(dir).unwrap_err(),
            private_file(&dir.join("secret.key")).unwrap_err(),
            inspect_file(&dir.join("secret.key")).unwrap_err(),
            // Refused before the missing file would be reported as not found.
            inspect_file(&dir.join("peers")).unwrap_err(),
            lock_agent(dir).unwrap_err(),
            crate::peers::load_key(dir).unwrap_err(),
            crate::peers::load(dir).unwrap_err(),
        ]
    }

    #[test]
    fn refuses_a_parent_folder_that_another_account_may_change() {
        let temp = test_temp();
        let parent = temp.path().join("parent");
        let dir = parent.join("herdvia");
        let key = crate::peers::load_key(&dir).unwrap();
        let users = "S-1-5-32-545";
        // Modify, and each of the rights in it or Full control that let Users rename or
        // re-permission the folder or what is inside it, or set a reparse point on it while it
        // is empty: writing its data (adding files), writing its attributes, or generic write.
        for right in [
            "M", "(D)", "(DC)", "(WDAC)", "(WO)", "(GA)", "(WD)", "(WA)", "(GW)",
        ] {
            icacls(&parent, &["/grant:r", &format!("*{users}:{right}")]);
            let new = parent.join("new");
            let mut errors = entry_points(&dir);
            errors.push(prepare_dir(&new).unwrap_err());
            for err in errors {
                assert_eq!(
                    err.kind(),
                    io::ErrorKind::PermissionDenied,
                    "{right}: {err}"
                );
                assert!(
                    err.to_string()
                        .starts_with(&format!("{} lets {users} ", parent.display())),
                    "{right}: {err}"
                );
            }
            // Refused before anything is created.
            assert!(!new.exists(), "{right}");
        }
        // Without the grant, the same data directory is used again.
        icacls(&parent, &["/remove:g", &format!("*{users}")]);
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        lock_agent(&dir).unwrap();
    }

    #[test]
    fn accepts_parent_folders_that_others_may_only_read_or_add_folders_to() {
        let temp = test_temp();
        let parent = temp.path().join("parent");
        fs::create_dir(&parent).unwrap();
        // As on C:\ and above a profile: reading for Users and Everyone, adding folders for
        // Authenticated Users, and entries that pass on to new entries only (CREATOR OWNER).
        for grant in [
            "*S-1-5-32-545:(OI)(CI)RX",
            "*S-1-1-0:RX",
            "*S-1-5-11:(AD)",
            "*S-1-5-11:(OI)(CI)(IO)M",
            "*S-1-3-0:(OI)(CI)(IO)F",
        ] {
            icacls(&parent, &["/grant", grant]);
        }
        let dir = parent.join("herdvia");
        let key = crate::peers::load_key(&dir).unwrap();
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        assert_eq!(inspect_file(&dir.join("secret.key")).unwrap().1, None);
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        lock_agent(&dir).unwrap();
    }

    #[test]
    fn refuses_a_junction_above_the_data_directory() {
        let temp = test_temp();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        junction(&link, &real);
        let dir = link.join("herdvia");
        for err in entry_points(&dir) {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            assert!(
                err.to_string().starts_with(&format!(
                    "{} is a junction, symbolic link, or other reparse point above the data directory",
                    link.display()
                )),
                "{err}"
            );
        }
        assert!(!real.join("herdvia").exists());
        // An existing data directory reached through it is refused the same way.
        crate::peers::load_key(&real.join("herdvia")).unwrap();
        for err in entry_points(&dir) {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        }
    }

    #[test]
    fn accepts_the_default_and_test_folders() {
        let sid = current_sid().unwrap();
        // %APPDATA%\herdvia is not checked at all, so nothing is read there.
        let appdata = env_path("APPDATA").unwrap();
        assert!(check_ancestors(&appdata.join("herdvia"), &sid).unwrap());
        let temp = test_temp();
        assert!(check_ancestors(&temp.path().join("herdvia"), &sid).unwrap());
        // Folders that do not exist yet are checked once prepare_dir makes them.
        let dir = temp.path().join("a").join("b").join("herdvia");
        assert!(!check_ancestors(&dir, &sid).unwrap());
        prepare_dir(&dir).unwrap();
        assert!(check_ancestors(&dir, &sid).unwrap());
        // The same folders, spelled as a verbatim path.
        let verbatim = PathBuf::from(format!(r"\\?\{}", dir.display()));
        assert!(check_ancestors(&verbatim, &sid).unwrap());
        prepare_dir(&verbatim).unwrap();
    }

    #[test]
    fn accepts_the_default_data_directory_without_checking_its_folders() {
        let sid = current_sid().unwrap();
        let temp = test_temp();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        junction(&link, &real);
        let default = link.join("herdvia");
        assert!(check_ancestors_unless(&default, &sid, Some(&default)).unwrap());
        // As private_file passes it, the parent of the key.
        let key = default.join("secret.key");
        assert!(check_ancestors_unless(key.parent().unwrap(), &sid, Some(&default)).unwrap());
        // Another folder there, the same one spelled another way, and the folder without a
        // default (APPDATA is not set) are checked.
        let verbatim = PathBuf::from(format!(r"\\?\{}", default.display()));
        for dir in [&link.join("other"), &verbatim] {
            let err = check_ancestors_unless(dir, &sid, Some(&default)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        }
        let err = check_ancestors_unless(&default, &sid, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(!real.join("herdvia").exists());
    }

    #[test]
    fn checks_only_paths_that_start_with_a_drive_letter() {
        let sid = current_sid().unwrap();
        // Nothing is opened, so the server need not exist.
        for path in [
            r"\\herdvia-test.invalid\share\herdvia",
            r"\\?\UNC\herdvia-test.invalid\share\herdvia",
            r"\\.\C:\herdvia",
        ] {
            assert!(check_ancestors(Path::new(path), &sid).unwrap(), "{path}");
        }
        // A drive letter is checked from its root.
        let temp = test_temp();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        junction(&link, &real);
        let verbatim = PathBuf::from(format!(r"\\?\{}", link.join("herdvia").display()));
        assert_eq!(
            check_ancestors(&verbatim, &sid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    /// Whether `prepare_dir` leaves the label of `path` as it is.
    fn labeled(path: &Path) -> bool {
        let entry = open_entry(path, READ_CONTROL).unwrap();
        !needs_label(&read_labels(&entry).unwrap(), path.is_dir())
    }

    fn set_test_label(path: &Path, sddl: &str) {
        set_label(&open_entry(path, READ_CONTROL | WRITE_OWNER).unwrap(), sddl).unwrap();
    }

    fn test_peer(key: &iroh::SecretKey) -> crate::peers::Peer {
        crate::peers::Peer {
            name: "work".into(),
            id: key.public(),
            permission: crate::peers::Permission::Both,
        }
    }

    #[test]
    fn labels_the_directory_and_what_is_created_in_it() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        prepare_dir(&dir).unwrap();
        let text = label_text(&dir);
        assert!(text.contains("(ML;OICI;NWNR;;;ME)"), "{text}");
        assert!(labeled(&dir));
        // The key and peers are written through temporary files, which take the label from
        // the directory as they are created, as does any new file.
        let key = crate::peers::load_key(&dir).unwrap();
        crate::peers::allow(&dir, test_peer(&key)).unwrap();
        let new = dir.join("new");
        fs::write(&new, b"x").unwrap();
        for path in [dir.join("secret.key"), dir.join("peers"), new] {
            let text = label_text(&path);
            assert!(text.contains("NWNR;;;ME)"), "{}: {text}", path.display());
            assert!(labeled(&path), "{}: {text}", path.display());
        }
        // doctor says nothing about labels.
        assert_eq!(inspect_dir(&dir).unwrap(), None);
        assert_eq!(inspect_file(&dir.join("secret.key")).unwrap().1, None);
    }

    #[test]
    fn labels_an_installation_whose_dacl_is_set_already() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // As an older herdvia left it: the DACL that protect_dir sets, and no label.
        set_test_dacl(
            &dir,
            &format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
        );
        let key = iroh::SecretKey::generate();
        let (secret, peers) = (dir.join("secret.key"), dir.join("peers"));
        fs::write(&secret, key.to_bytes()).unwrap();
        fs::write(&peers, b"").unwrap();
        assert!(
            read_dacl(&open_entry(&dir, READ_CONTROL).unwrap(), &sid)
                .unwrap()
                .target
        );
        for path in [&dir, &secret, &peers] {
            assert!(!labeled(path), "{}: {}", path.display(), label_text(path));
        }
        let before = [dacl(&dir), dacl(&secret), dacl(&peers)];
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        assert!(
            label_text(&dir).contains("(ML;OICI;NWNR;;;ME)"),
            "{}",
            label_text(&dir)
        );
        for path in [&secret, &peers] {
            assert!(labeled(path), "{}: {}", path.display(), label_text(path));
        }
        // Only the labels changed.
        assert_eq!([dacl(&dir), dacl(&secret), dacl(&peers)], before);
    }

    #[test]
    fn labels_the_key_in_a_directory_it_cannot_label() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // As protect_dir sets it, but this user may not change the directory's label: OWNER
        // RIGHTS (OW) takes the owner's implicit rights away.
        set_test_dacl(
            &dir,
            &format!(
                "D:P(D;;WO;;;{sid})(D;;WO;;;OW)(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
            ),
        );
        assert!(denied(
            &open_entry(&dir, READ_CONTROL | WRITE_OWNER).unwrap_err()
        ));
        prepare_dir(&dir).unwrap();
        assert!(!labeled(&dir), "{}", label_text(&dir));
        let secret = dir.join("secret.key");
        let key = crate::peers::load_key(&dir).unwrap();
        // The directory passes no label on, so the new key has none until the next run.
        assert!(!labeled(&secret), "{}", label_text(&secret));
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        let text = label_text(&secret);
        assert!(text.contains("(ML;;NWNR;;;ME)"), "{text}");
        assert!(!labeled(&dir), "{}", label_text(&dir));
    }

    #[test]
    fn uses_a_directory_it_may_only_modify_without_a_label() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // As when SYSTEM owns it and this user may only modify it (0x1301bf): no WRITE_OWNER,
        // on the directory or on what is created in it.
        set_test_dacl(
            &dir,
            &format!("D:P(D;;WD;;;{sid})(D;;WD;;;OW)(A;OICI;0x1301bf;;;{sid})(A;OICI;FA;;;SY)"),
        );
        prepare_dir(&dir).unwrap();
        let key = crate::peers::load_key(&dir).unwrap();
        assert_eq!(
            crate::peers::load_key(&dir).unwrap().to_bytes(),
            key.to_bytes()
        );
        crate::peers::allow(&dir, test_peer(&key)).unwrap();
        assert_eq!(crate::peers::load(&dir).unwrap().len(), 1);
        for path in [dir.clone(), dir.join("secret.key"), dir.join("peers")] {
            assert!(!labeled(&path), "{}: {}", path.display(), label_text(&path));
        }
    }

    #[test]
    fn leaves_a_label_that_is_enough_alone_and_raises_a_low_one() {
        let temp = test_temp();
        let dir = temp.path().join("herdvia");
        crate::peers::load_key(&dir).unwrap();
        let secret = dir.join("secret.key");
        // Enough already, with NO_EXECUTE_UP besides, which a rewrite would drop.
        set_test_label(&dir, "S:(ML;OICI;NWNRNX;;;ME)");
        set_test_label(&secret, "S:(ML;;NWNRNX;;;ME)");
        let before = [label_text(&dir), label_text(&secret)];
        assert!(before.iter().all(|text| text.contains("NX;")), "{before:?}");
        prepare_dir(&dir).unwrap();
        assert_eq!([label_text(&dir), label_text(&secret)], before);
        // Low lets a Low integrity process read and write it: raised to Medium.
        set_test_label(&secret, "S:(ML;;NW;;;LW)");
        assert!(!labeled(&secret), "{}", label_text(&secret));
        prepare_dir(&dir).unwrap();
        let text = label_text(&secret);
        assert!(text.contains("(ML;;NWNR;;;ME)"), "{text}");
    }

    #[test]
    fn a_low_integrity_process_cannot_read_the_key_or_peers() {
        use std::os::windows::process::CommandExt;
        let temp = test_temp();
        // A process started from a program labeled Low runs at Low integrity.
        let low_cmd = temp.path().join("cmd.exe");
        fs::copy(
            env_path("SystemRoot").unwrap().join(r"System32\cmd.exe"),
            &low_cmd,
        )
        .unwrap();
        icacls(&low_cmd, &["/setintegritylevel", "low"]);
        let read = |path: &Path| {
            std::process::Command::new(&low_cmd)
                .args(["/d", "/c"])
                .raw_arg(format!("type \"{}\"", path.display()))
                .output()
                .unwrap()
        };
        // It reads a file of this user's without a label.
        let plain = temp.path().join("plain");
        fs::write(&plain, b"plain").unwrap();
        let output = read(&plain);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"plain");
        let dir = temp.path().join("herdvia");
        let key = crate::peers::load_key(&dir).unwrap();
        crate::peers::allow(&dir, test_peer(&key)).unwrap();
        for path in [dir.join("secret.key"), dir.join("peers")] {
            let output = read(&path);
            assert!(!output.status.success(), "{}: {output:?}", path.display());
            assert!(output.stdout.is_empty(), "{}: {output:?}", path.display());
        }
    }
}
