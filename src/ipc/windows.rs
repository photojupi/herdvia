use super::{pipe_name::pipe_name, pipe_state};
use std::{
    env,
    ffi::c_void,
    fs::{self, File},
    io,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::null_mut,
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
        ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, LocalFree, STATUS_SUCCESS,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_KERNEL_OBJECT,
        },
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx},
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
    match env::var_os("HLINK_DIR") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(env_path("APPDATA")?.join("hlink")),
    }
}

pub fn reject_root() -> io::Result<()> {
    Ok(())
}

pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    // M5a retains the permissions inherited from the user's profile.
    File::open(path)
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
    let directory = env::var_os("HLINK_DIR")
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
            "missing pipe owner SID",
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

fn create_pipe(name: &str, sid: &str, first: bool) -> io::Result<NamedPipeServer> {
    // Set the owner explicitly, including for elevated users whose token's
    // default owner might otherwise be the Administrators group.
    let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor = null_mut();
    // SAFETY: sddl is NUL-terminated; descriptor is a valid output location.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(descriptor);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
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

fn check_owner(stream: &ClientStream, sid: &str) -> io::Result<()> {
    let mut owner = null_mut();
    let mut descriptor = null_mut();
    // ClientOptions opens with GENERIC_READ, which includes READ_CONTROL.
    // SAFETY: stream owns a valid pipe handle; owner and descriptor are writable
    // outputs, and all unused optional outputs are null.
    let result = unsafe {
        GetSecurityInfo(
            stream.as_raw_handle(),
            SE_KERNEL_OBJECT,
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
    let _allocation = LocalAllocation(descriptor);
    if sid_string(owner)? != sid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pipe owner SID does not match",
        ));
    }
    Ok(())
}
