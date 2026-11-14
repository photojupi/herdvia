use super::Launch;
use crate::{frame::MAX_PAYLOAD, ipc, template};
use std::{
    ffi::OsStr,
    fs, io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::{null, null_mut},
    sync::Arc,
    time::Duration,
};
use tokio::fs::File;
use windows_sys::Win32::{
    Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Security::SECURITY_ATTRIBUTES,
    System::{
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList,
            EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

pub(super) struct Command {
    home: PathBuf,
    executable: PathBuf,
    session: Option<String>,
}

pub(super) fn prepare(line: &str) -> io::Result<Launch> {
    let session = match template::classify_windows(line) {
        Some(template::WindowsLine::UnixProbe) => {
            return Ok(Launch::Exit(
                b"'sh' is not recognized as an internal or external command,\n",
                1,
            ));
        }
        Some(template::WindowsLine::Connect(session)) => session,
        None => return Ok(Launch::Reject),
    };
    let home = ipc::home_dir()?;
    let executable = home.join("AppData/Local/Programs/herdr-core/herdr-core.exe");
    match fs::metadata(&executable) {
        Ok(_) => Ok(Launch::Run(Box::new(Command {
            home,
            executable,
            session: session.map(str::to_owned),
        }))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Ok(Launch::Exit(b"herdr-core is not installed here\n", 127))
        }
        Err(err) => Err(err),
    }
}

pub(super) struct Child {
    process: Arc<OwnedHandle>,
    pub(super) stdin: Option<File>,
    pub(super) stdout: Option<File>,
    pub(super) stderr: Option<File>,
}

pub(super) struct ChildGuard(Option<Arc<OwnedHandle>>);

impl ChildGuard {
    pub(super) fn kill(&mut self) {
        if let Some(process) = self.0.take() {
            // Only terminate connect itself: its descendants may host live terminals.
            // SAFETY: this handle belongs to the direct child returned by CreateProcessW
            // and stays live through this call; it cannot refer to a reused PID.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
        }
    }

    pub(super) fn finish(&mut self) {
        self.0.take();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

pub(super) fn spawn(command: Command) -> io::Result<(Child, ChildGuard)> {
    let (child_stdin, stdin) = pipe()?;
    let (stdout, child_stdout) = pipe()?;
    let (stderr, child_stderr) = pipe()?;
    let handles = [
        child_stdin.as_raw_handle(),
        child_stdout.as_raw_handle(),
        child_stderr.as_raw_handle(),
    ];
    for handle in handles {
        // SAFETY: each handle is a live pipe end exclusively owned above.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut attributes = Attributes::new()?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.as_ptr();
    let executable = wide(command.executable.as_os_str())?;
    let home = wide(command.home.as_os_str())?;
    // lpApplicationName selects the executable exactly; argv[0] need only name it.
    // Session bytes have already passed the exact template's ASCII allowlist.
    let args = match command.session {
        Some(session) => format!("herdr-core.exe connect --session {session}"),
        None => "herdr-core.exe connect".to_owned(),
    };
    let mut args = wide(OsStr::new(&args))?;
    let mut info = PROCESS_INFORMATION::default();
    // HANDLE_LIST restricts inheritance to these three pipe ends.
    // SAFETY: all strings are NUL-terminated, args is writable, the attribute
    // array and its handles outlive this call, and info is writable storage.
    if unsafe {
        CreateProcessW(
            executable.as_ptr(),
            args.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            null(),
            home.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateProcessW transfers two new valid handles to us.
    let process = Arc::new(unsafe { OwnedHandle::from_raw_handle(info.hProcess) });
    let guard = ChildGuard(Some(process.clone()));
    // SAFETY: hThread is the other newly owned handle returned by CreateProcessW.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    drop(thread);
    drop((child_stdin, child_stdout, child_stderr));
    Ok((
        Child {
            process,
            stdin: Some(async_file(stdin)),
            stdout: Some(async_file(stdout)),
            stderr: Some(async_file(stderr)),
        },
        guard,
    ))
}

fn async_file(handle: OwnedHandle) -> File {
    let mut file = File::from_std(fs::File::from(handle));
    file.set_max_buf_size(MAX_PAYLOAD);
    file
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write) = (null_mut(), null_mut());
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    // SAFETY: output pointers are writable and attributes is valid for this call.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, MAX_PAYLOAD as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreatePipe returns two new valid handles owned by us.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

pub(super) async fn wait(child: &mut Child) -> io::Result<i32> {
    loop {
        // SAFETY: child owns the live process handle; a zero timeout never blocks.
        match unsafe { WaitForSingleObject(child.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: the process handle is live and code is writable storage.
                if unsafe { GetExitCodeProcess(child.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                // Preserve all 32 exit-code bits in the protocol's signed i32 field.
                return Ok(code as i32);
            }
            WAIT_TIMEOUT => tokio::time::sleep(Duration::from_millis(10)).await,
            _ => return Err(io::Error::last_os_error()),
        }
    }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in process path",
        ));
    }
    value.push(0);
    Ok(value)
}

struct Attributes(Vec<usize>);

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list requests its required allocation size in bytes.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: storage has native pointer alignment and at least bytes bytes.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(storage))
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    fn set(&mut self, attribute: u32, handles: &[HANDLE]) -> io::Result<()> {
        // SAFETY: this list is initialized; only handle-array attributes are used,
        // and the caller retains the arrays and handles through CreateProcessW.
        if unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the initialized list is still allocated and is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}
