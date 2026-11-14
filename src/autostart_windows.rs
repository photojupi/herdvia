use crate::{autostart_logic, ipc};
use std::{
    env,
    fs::{self, OpenOptions},
    io,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::{null, null_mut},
    time::Duration,
};
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_PIPE_BUSY,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::{
            Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle},
            Pipes::GetNamedPipeServerProcessId,
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
                REG_SAM_FLAGS, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
                RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
            },
            SystemInformation::GetSystemDirectoryW,
            Threading::{
                CREATE_BREAKAWAY_FROM_JOB, CreateProcessW, DETACHED_PROCESS, GetCurrentProcess,
                OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE,
                PROCESS_TERMINATE, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::w,
};

const RUN_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: *const u16 = w!("herdvia");
const WAIT: Duration = Duration::from_secs(5);

pub async fn install() -> io::Result<()> {
    reject_elevated_install()?;
    autostart_logic::validate_install(env::var_os("HERDVIA_DIR").is_some())?;
    let executable = env::current_exe()?;
    let command =
        autostart_logic::run_command(&executable.as_os_str().encode_wide().collect::<Vec<_>>())?;
    let log = log_path()?;
    let registered = registered()?;
    let pipe = agent_pipe().await?;
    let stop = autostart_logic::install_requires_stop(registered, pipe.is_some())?;
    if autostart_logic::unstable_path(
        &executable.to_string_lossy(),
        &env::temp_dir().to_string_lossy(),
    ) {
        eprintln!(
            "herdvia: warning: executable is in a temporary or target directory: {}",
            executable.display()
        );
    }
    if stop {
        stop_agent(pipe.unwrap()).await?;
    }
    register(&command)?;
    let started = async {
        start_detached(command)?;
        wait_for_pipe(true).await
    }
    .await;
    started.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "{err}; Run registration remains; check {} and run herdvia agent install again",
                log.display()
            ),
        )
    })
}

pub async fn uninstall() -> io::Result<()> {
    // Automatic startup always uses the default pipe, even with HERDVIA_DIR set.
    unregister()?;
    if let Some(pipe) = agent_pipe().await? {
        stop_agent(pipe).await?;
    }
    Ok(())
}

fn reject_elevated_install() -> io::Result<()> {
    if elevated()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "run herdvia agent install from a normal PowerShell window without administrator privileges",
        ));
    }
    Ok(())
}

/// Whether this process runs with an elevated administrator token.
pub(crate) fn elevated() -> io::Result<bool> {
    let mut token = null_mut();
    // SAFETY: token is writable; only TOKEN_QUERY access is requested for the
    // current process's token, and GetCurrentProcess returns a pseudo-handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful OpenProcessToken transferred one owned handle to us.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    // SAFETY: token is live; elevation is an aligned, writable TOKEN_ELEVATION
    // buffer of the supplied size, and returned is a writable output pointer.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of_val(&elevation) as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

fn log_path() -> io::Result<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|dir| PathBuf::from(dir).join("herdvia/herdvia.log"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))
}

pub fn redirect_log() -> io::Result<()> {
    let path = log_path()?;
    fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    // SAFETY: file owns a writable append handle. On success it is retained
    // for the process lifetime below, including the final error diagnostic.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let handle = file.into_raw_handle();
    // SAFETY: handle remains open until process exit; SetStdHandle does not
    // duplicate or close it. Rust stdout/stderr use these Win32 standard handles.
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn registry_result(result: u32) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result as i32))
    }
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns a successfully opened registry key.
        unsafe { RegCloseKey(self.0) };
    }
}

fn open_run(access: REG_SAM_FLAGS) -> io::Result<Option<Key>> {
    let mut key = null_mut();
    // SAFETY: RUN_KEY is NUL-terminated and key is a writable output pointer.
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, access, &mut key) };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    registry_result(result)?;
    Ok(Some(Key(key)))
}

fn registered() -> io::Result<bool> {
    let Some(key) = open_run(KEY_QUERY_VALUE)? else {
        return Ok(false);
    };
    let mut size = 0;
    // SAFETY: key is live, VALUE is NUL-terminated, and a null data buffer
    // requests only the size. Any existing value counts as a registration.
    let result =
        unsafe { RegQueryValueExW(key.0, VALUE, null(), null_mut(), null_mut(), &mut size) };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    registry_result(result)?;
    Ok(true)
}

/// The Run value's command, or None when none is registered. Only reads the registry.
pub(crate) fn registered_command() -> io::Result<Option<Vec<u16>>> {
    let Some(key) = open_run(KEY_QUERY_VALUE)? else {
        return Ok(None);
    };
    let mut kind = 0;
    let mut size = 0;
    // SAFETY: key is live, VALUE is NUL-terminated, and a null data buffer
    // requests only the value's type and size in bytes.
    let result =
        unsafe { RegQueryValueExW(key.0, VALUE, null(), &mut kind, null_mut(), &mut size) };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    registry_result(result)?;
    if kind != REG_SZ {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the Run value is not a string",
        ));
    }
    let mut data = vec![0_u16; (size as usize).div_ceil(size_of::<u16>())];
    let mut size = size_of_val(data.as_slice()) as u32;
    // SAFETY: data is writable, u16-aligned, and size holds its length in bytes.
    registry_result(unsafe {
        RegQueryValueExW(
            key.0,
            VALUE,
            null(),
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    })?;
    data.truncate(size as usize / size_of::<u16>());
    Ok(Some(data))
}

fn register(command: &[u16]) -> io::Result<()> {
    let mut key = null_mut();
    // SAFETY: all strings are NUL-terminated, key is writable, and the default
    // security descriptor is used for the current user's nonvolatile key.
    registry_result(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        )
    })?;
    let key = Key(key);
    // SAFETY: command is the NUL-terminated UTF-16 REG_SZ generated above;
    // its byte length includes the terminator and fits u32 (259 units plus NUL).
    registry_result(unsafe {
        RegSetValueExW(
            key.0,
            VALUE,
            0,
            REG_SZ,
            command.as_ptr().cast(),
            size_of_val(command) as u32,
        )
    })
}

fn unregister() -> io::Result<()> {
    let Some(key) = open_run(KEY_SET_VALUE)? else {
        return Ok(());
    };
    // SAFETY: key is live and VALUE is NUL-terminated. Only this value is removed.
    let result = unsafe { RegDeleteValueW(key.0, VALUE) };
    if result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        registry_result(result)
    }
}

async fn agent_pipe() -> io::Result<Option<ipc::ClientStream>> {
    match ipc::connect_default().await {
        Ok(pipe) => Ok(Some(pipe)), // connect_default has verified the owner SID.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

async fn wait_for_pipe(present: bool) -> io::Result<()> {
    tokio::time::timeout(WAIT, async {
        loop {
            match agent_pipe().await {
                Ok(pipe) if pipe.is_some() == present => return Ok(()),
                Ok(_) => {}
                Err(err) if !present && err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {}
                Err(err) => return Err(err),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            if present {
                "agent pipe did not appear within 5 seconds"
            } else {
                "agent pipe did not disappear within 5 seconds"
            },
        )
    })?
}

async fn stop_agent(pipe: ipc::ClientStream) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: pipe is a live client whose owner SID was checked by ipc::connect;
    // pid is writable. Holding the pipe does not prevent server PID reuse.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pid came from the verified local pipe. Request only termination
    // and synchronization rights, and do not allow handle inheritance.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let err = io::Error::last_os_error();
        // The server may have exited between the pipe lookup and OpenProcess.
        if err.raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32) {
            return Err(err);
        }
    } else {
        // SAFETY: successful OpenProcess transferred one owned handle to us.
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut current_pid = 0;
        // SAFETY: pipe remains live and current_pid is writable. Recheck after
        // OpenProcess because the PID may have been reused before it opened.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut current_pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if current_pid != pid {
            return Err(io::Error::other(
                "agent pipe server PID changed; refusing to terminate",
            ));
        }
        if !process_exited(&process)? {
            // SAFETY: process owns a handle opened with PROCESS_TERMINATE.
            if unsafe { TerminateProcess(process.as_raw_handle(), 0) } == 0 {
                let err = io::Error::last_os_error();
                if !process_exited(&process)? {
                    return Err(err);
                }
            }
        }
    }
    // A client handle can keep a closed pipe instance alive; release it first.
    drop(pipe);
    wait_for_pipe(false).await
}

fn process_exited(process: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: process is a live owned handle with PROCESS_SYNCHRONIZE access.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn start_detached(mut command: Vec<u16>) -> io::Result<()> {
    // Resolve the system conhost explicitly instead of searching the caller's cwd.
    let mut application = vec![0_u16; 32_768];
    // SAFETY: application is writable and its length is supplied in UTF-16 units.
    let length =
        unsafe { GetSystemDirectoryW(application.as_mut_ptr(), application.len() as u32) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= application.len() {
        return Err(io::Error::other("system directory path is too long"));
    }
    application.truncate(length);
    application.extend("\\conhost.exe".encode_utf16().chain([0]));
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // A terminal host (Herdr, sshd) may run this console in a kill-on-close job;
    // measured: an agent started from a Herdr terminal died when that terminal
    // ended. Leave the job when it allows breakaway, otherwise start inside it.
    let mut flags = DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB;
    loop {
        // SAFETY: application and command are NUL-terminated and live for the call;
        // command is writable. Output structures are correctly initialized. No
        // handles (including standard handles) are inherited, STARTF_USESTDHANDLES
        // is unset, and DETACHED_PROCESS separates the child from this console.
        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                flags,
                null(),
                null(),
                &startup,
                &mut info,
            )
        } != 0
        {
            break;
        }
        let err = io::Error::last_os_error();
        if flags & CREATE_BREAKAWAY_FROM_JOB == 0
            || err.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32)
        {
            return Err(err);
        }
        eprintln!(
            "herdvia: warning: this terminal does not let the agent outlive it; the agent may stop when the terminal closes and will start again at the next logon"
        );
        flags = DETACHED_PROCESS;
    }
    // SAFETY: successful CreateProcessW returned two distinct owned handles;
    // closing them releases our references without terminating the detached child.
    unsafe {
        drop(OwnedHandle::from_raw_handle(info.hThread));
        drop(OwnedHandle::from_raw_handle(info.hProcess));
    }
    Ok(())
}
