use crate::{
    frame::{self, *},
    ipc, template,
};
use std::{io, os::unix::process::ExitStatusExt, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{ChildStdin, Command},
    task::JoinSet,
};

/// Receives a network-format OPEN (just the command line), independent of transport.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    mut stream: S,
) -> io::Result<()> {
    let open = frame::read(&mut stream)
        .await?
        .ok_or_else(|| frame::invalid("missing OPEN"))?;
    if open.tag != OPEN {
        return Err(frame::invalid("first frame must be OPEN"));
    }
    let line = std::str::from_utf8(&open.payload).map_err(|_| frame::invalid("invalid OPEN"))?;
    if !template::allowed(line) {
        eprintln!("hlink: rejected command: {line:?}");
        frame::write(
            &mut stream,
            ERROR,
            b"command does not match an allowed connect template",
        )
        .await?;
        return Err(frame::invalid("command rejected"));
    }
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(line).current_dir(ipc::home_dir()?);
    execute(stream, command).await
}

async fn execute<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    mut stream: S,
    mut command: Command,
) -> io::Result<()> {
    let spawned = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            frame::write(
                &mut stream,
                ERROR,
                format!("cannot start child: {err}").as_bytes(),
            )
            .await?;
            return Err(err);
        }
    };
    let mut group = ProcessGroup(child.id().expect("new child has a pid") as i32);
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (reader, mut writer) = tokio::io::split(stream);
    // JoinSet aborts the input task even if this entire session is cancelled.
    let mut input = JoinSet::new();
    input.spawn(receive_input(reader, stdin));
    let result = tokio::select! {
        result = input.join_next() => {
            result.expect("input task exists").map_err(io::Error::other)?
        }
        result = async {
            send_output(&mut writer, stdout, stderr).await?;
            let status = child.wait().await?;
            let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            frame::write(&mut writer, EXIT, &code.to_be_bytes()).await
        } => result,
    };
    input.abort_all();
    while input.join_next().await.is_some() {}
    if result.is_err() {
        group.kill();
        let _ = child.wait().await;
    } else {
        group.0 = 0;
    }
    result
}

struct ProcessGroup(i32);

impl ProcessGroup {
    fn kill(&mut self) {
        if self.0 > 0 {
            // SAFETY: the child was placed in its own process group with this pid.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
            self.0 = 0;
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn receive_input<R: AsyncRead + Unpin>(mut reader: R, stdin: ChildStdin) -> io::Result<()> {
    let mut stdin = Some(stdin);
    let mut eof = false;
    loop {
        let f = frame::read(&mut reader)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "peer disconnected"))?;
        match f.tag {
            STDIN if !eof => {
                if let Some(pipe) = stdin.as_mut() {
                    match pipe.write_all(&f.payload).await {
                        Ok(()) => pipe.flush().await?,
                        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                            stdin.take();
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
            STDIN_EOF if !eof => {
                eof = true;
                stdin.take();
            }
            _ => return Err(frame::invalid("unexpected input frame")),
        }
    }
}

async fn send_output<W, O, E>(writer: &mut W, mut stdout: O, mut stderr: E) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
{
    let mut out = vec![0; MAX_PAYLOAD];
    let mut err = vec![0; MAX_PAYLOAD];
    let (mut out_done, mut err_done) = (false, false);
    while !out_done || !err_done {
        tokio::select! {
            count = stdout.read(&mut out), if !out_done => {
                let count = count?;
                if count == 0 { out_done = true; }
                else { frame::write(writer, STDOUT, &out[..count]).await?; }
            }
            count = stderr.read(&mut err), if !err_done => {
                let count = count?;
                if count == 0 { err_done = true; }
                else { frame::write(writer, STDERR, &err[..count]).await?; }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use std::time::Duration;
    use tokio::time::timeout;

    fn command(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[tokio::test]
    async fn rejects_unapproved_line_with_error_frame() {
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(serve(server));
        frame::write(&mut client, OPEN, b"touch /must-not-run")
            .await
            .unwrap();
        let response = frame::read(&mut client).await.unwrap().unwrap();
        assert_eq!(response.tag, ERROR);
        assert!(frame::read(&mut client).await.unwrap().is_none());
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn rejects_missing_open() {
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(serve(server));
        frame::write(&mut client, STDIN, b"x").await.unwrap();
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn rejects_duplicate_open_and_input_after_eof() {
        for after_eof in [false, true] {
            let (mut client, server) = tokio::io::duplex(1024);
            let task = tokio::spawn(execute(server, command("printf ready; sleep 30")));
            frame::read(&mut client).await.unwrap().unwrap();
            if after_eof {
                frame::write(&mut client, STDIN_EOF, &[]).await.unwrap();
                frame::write(&mut client, STDIN, b"late").await.unwrap();
            } else {
                frame::write(&mut client, OPEN, b"duplicate").await.unwrap();
            }
            let err = timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn relays_binary_input_and_drains_output_before_exit() {
        timeout(Duration::from_secs(5), async {
            let (mut client, server) = tokio::io::duplex(128);
            let task = tokio::spawn(execute(server, command("cat; printf err >&2; exit 23")));
            frame::write(&mut client, STDIN, &[0, 255, 10, 13])
                .await
                .unwrap();
            frame::write(&mut client, STDIN_EOF, &[]).await.unwrap();
            let mut out = Vec::new();
            let mut err = Vec::new();
            loop {
                let f = frame::read(&mut client).await.unwrap().unwrap();
                match f.tag {
                    STDOUT => out.extend(f.payload),
                    STDERR => err.extend(f.payload),
                    EXIT => {
                        assert_eq!(f.payload, 23_i32.to_be_bytes());
                        break;
                    }
                    _ => panic!("unexpected frame {f:?}"),
                }
            }
            assert_eq!(out, [0, 255, 10, 13]);
            assert_eq!(err, b"err");
            task.await.unwrap().unwrap();
            assert!(frame::read(&mut client).await.unwrap().is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn blocked_child_stdin_does_not_block_output() {
        timeout(Duration::from_secs(5), async {
            let (mut client, server) = tokio::io::duplex(131_072);
            let task = tokio::spawn(execute(server, command("printf ready; sleep 30")));
            frame::write(&mut client, STDIN, &vec![b'x'; MAX_PAYLOAD])
                .await
                .unwrap();
            let f = frame::read(&mut client).await.unwrap().unwrap();
            assert_eq!(f.tag, STDOUT);
            assert_eq!(f.payload, b"ready");
            task.abort();
            let _ = task.await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn disconnect_during_stdin_backpressure_still_cleans_up() {
        use std::os::fd::AsFd;
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let watcher = server.as_fd().try_clone_to_owned().unwrap();
        let mut task = tokio::spawn(async move {
            tokio::select! {
                result = execute(server, command("printf ready; sleep 30")) => result,
                err = ipc::disconnected(watcher) => Err(err),
            }
        });
        let ready = frame::read(&mut client).await.unwrap().unwrap();
        assert_eq!(ready.payload, b"ready");
        let sender = tokio::spawn(async move {
            for _ in 0..3 {
                let _ = frame::write(&mut client, STDIN, &vec![b'x'; MAX_PAYLOAD]).await;
            }
            std::future::pending::<()>().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        sender.abort();
        let _ = sender.await;
        let completed = timeout(Duration::from_secs(2), &mut task).await;
        if completed.is_err() {
            task.abort();
            let _ = task.await;
        }
        assert!(
            completed.is_ok(),
            "disconnect was hidden behind a blocked child stdin"
        );
    }

    #[tokio::test]
    async fn signal_exit_is_128_plus_signal() {
        let (mut client, server) = tokio::io::duplex(1024);
        let task = tokio::spawn(execute(server, command("kill -TERM $$")));
        let f = frame::read(&mut client).await.unwrap().unwrap();
        assert_eq!(f.tag, EXIT);
        assert_eq!(f.payload, 143_i32.to_be_bytes());
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn disconnect_kills_child_process_group() {
        timeout(Duration::from_secs(5), async {
            let (mut client, server) = tokio::io::duplex(1024);
            let task = tokio::spawn(execute(
                server,
                command("sleep 30 & printf '%s %s' $$ $!; wait"),
            ));
            let f = frame::read(&mut client).await.unwrap().unwrap();
            let pids: Vec<i32> = std::str::from_utf8(&f.payload)
                .unwrap()
                .split_whitespace()
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(pids.len(), 2);
            drop(client);
            assert!(task.await.unwrap().is_err());
            for pid in pids {
                loop {
                    // SAFETY: signal 0 only checks existence of this test's child.
                    if unsafe { libc::kill(pid, 0) } == -1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        })
        .await
        .unwrap();
    }
}
