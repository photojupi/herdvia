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
        ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, HANDLE, LocalFree,
        STATUS_SUCCESS,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE,
            SetSecurityInfo,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, LOCKFILE_EXCLUSIVE_LOCK, LockFileEx,
        READ_CONTROL, WRITE_DAC,
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
    let sid = current_sid()?;
    let name = name_for_dir(&sid, dir)?;
    let next = create_pipe(&name, &sid, true).map_err(|err| {
        if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            io::Error::new(io::ErrorKind::AddrInUse, "agent is already running")
        } else {
            err
        }
    })?;
    Ok(Listener { next, name, sid })
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

const ADMINISTRATORS: &str = "S-1-5-32-544";
const LOCAL_SYSTEM: &str = "S-1-5-18";

/// Whether an owner can be trusted with the data: the owner of an object can always
/// change its DACL back. An elevated user's new objects belong to Administrators.
fn trusted_owner(owner: &str, sid: &str) -> bool {
    owner == sid || owner == ADMINISTRATORS || owner == LOCAL_SYSTEM
}

/// The counterpart of chmod 700: checks the owner first, then sets a protected DACL.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let sid = current_sid()?;
    let handle = match open_entry(dir, READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES) {
        Ok(handle) => handle,
        Err(err) => {
            // Explain a directory that another account owns, if it can be read.
            check_dir(dir)?;
            return Err(err);
        }
    };
    check_entry(&handle, dir, true, &sid)?;
    protect_dir(&handle, &sid)
}

/// The read-only checks that `prepare_dir` makes before it sets the DACL.
pub(crate) fn check_dir(dir: &Path) -> io::Result<()> {
    let handle = open_entry(dir, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    check_entry(&handle, dir, true, &current_sid()?)
}

pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    check_entry(&file, path, false, &current_sid()?)?;
    Ok(file)
}

/// Opens a file or directory itself, not the target of a link or junction.
fn open_entry(path: &Path, access: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Refuses a reparse point (a junction or link), the other kind of entry, and an owner
/// that `trusted_owner` does not accept. `file` needs READ_CONTROL and FILE_READ_ATTRIBUTES.
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
    if !trusted_owner(&owner, sid) {
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

/// Only this user, SYSTEM, and Administrators, inherited by everything created inside
/// (including the key and peers, written through temporary files) and not from the parent.
fn protect_dir(dir: &File, sid: &str) -> io::Result<()> {
    let descriptor = security_descriptor(&format!(
        "D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))?;
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
        return Err(io::Error::other("data directory DACL is missing"));
    }
    // SAFETY: dir owns a handle opened with WRITE_DAC; dacl is a valid ACL, and the
    // owner, group, and SACL are left unchanged (null).
    let result = unsafe {
        SetSecurityInfo(
            dir.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
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
    let mut overlapped = OVERLAPPED::default();
    // Windows locks are mandatory. Lock the fixed byte at 4 GiB, well beyond
    // the 32-byte key and read_to_end's read buffer, so id/agent can still read it.
    overlapped.Anonymous.Anonymous.Offset = 0;
    overlapped.Anonymous.Anonymous.OffsetHigh = 1;
    // SAFETY: lock owns a synchronous file handle and overlapped remains alive
    // for this blocking call. Closing lock releases the byte-range lock.
    if unsafe {
        LockFileEx(
            lock.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &mut overlapped,
        )
    } == 0
    {
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
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;

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
            assert!(trusted_owner(owner, "S-1-5-21-1"), "{owner}");
        }
        for owner in ["S-1-5-21-2", "S-1-1-0", "S-1-5-32-545", "S-1-5-11"] {
            assert!(!trusted_owner(owner, "S-1-5-21-1"), "{owner}");
        }
    }

    #[test]
    fn protects_the_directory_and_its_new_files_inherit_that() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        prepare_dir(&dir).unwrap();
        let sid = current_sid().unwrap();
        // SetSecurityInfo also marks the DACL auto-inherited (AI).
        let protected = dacl(&dir);
        assert!(protected.starts_with("D:P"), "{protected}");
        assert_eq!(protected.matches("(A;").count(), 3, "{protected}");
        for ace in [
            format!("(A;OICI;FA;;;{sid})"),
            "(A;OICI;FA;;;SY)".into(),
            "(A;OICI;FA;;;BA)".into(),
        ] {
            assert!(protected.contains(&ace), "{protected}");
        }
        check_dir(&dir).unwrap();
        // The key and peers are written through temporary files and renamed.
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
            let file = dacl(&path);
            assert!(
                file.contains(&format!(";;;{sid})"))
                    && !file.contains(";;;WD)")
                    && !file.contains(";;;BU)")
                    && !file.contains(";;;AU)"),
                "{name}: {file}"
            );
        }
        // Again on an existing directory.
        prepare_dir(&dir).unwrap();
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
        for result in [prepare_dir(&link), check_dir(&link)] {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
        let file = temp.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert_eq!(
            check_dir(&file).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            private_file(&target).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            private_file(&temp.path().join("none")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
