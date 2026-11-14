use super::Launch;
use crate::{ipc, template};
use std::{io, os::unix::process::ExitStatusExt, process::Stdio};
use tokio::process::Child;
pub(super) use tokio::process::Command;

pub(super) fn prepare(line: &str) -> io::Result<Launch> {
    if !template::allowed(line) {
        return Ok(Launch::Reject);
    }
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(line).current_dir(ipc::home_dir()?);
    Ok(Launch::Run(Box::new(command)))
}

pub(super) fn spawn(mut command: Command) -> io::Result<(Child, ProcessGroup)> {
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let group = ProcessGroup(child.id().expect("new child has a pid") as i32);
    Ok((child, group))
}

pub(super) async fn wait(child: &mut Child) -> io::Result<i32> {
    let status = child.wait().await?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

pub(super) struct ProcessGroup(i32);

impl ProcessGroup {
    pub(super) fn kill(&mut self) {
        if self.0 > 0 {
            // SAFETY: the child was placed in its own process group with this pid.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
            self.0 = 0;
        }
    }

    pub(super) fn finish(&mut self) {
        self.0 = 0;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}
