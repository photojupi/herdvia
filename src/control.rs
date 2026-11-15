use crate::{
    frame::{self, *},
    ipc,
};
use std::{io, path::Path, time::Duration};

pub async fn request(dir: &Path, command: &str) -> io::Result<String> {
    let mut stream = ipc::connect(dir).await?;
    frame::write(&mut stream, CONTROL, command.as_bytes()).await?;
    let mut output = Vec::new();
    loop {
        let f = frame::read(&mut stream)
            .await?
            .ok_or_else(|| frame::invalid("agent closed without EXIT"))?;
        match f.tag {
            STDOUT => output.extend_from_slice(&f.payload),
            EXIT if f.payload == 0_i32.to_be_bytes() => {
                return String::from_utf8(output)
                    .map_err(|_| frame::invalid("invalid control response"));
            }
            ERROR => {
                return Err(io::Error::other(
                    String::from_utf8(f.payload).expect("validated UTF-8"),
                ));
            }
            _ => return Err(frame::invalid("unexpected control response")),
        }
    }
}

pub async fn reload_if_running(dir: &Path) -> io::Result<()> {
    // macOS also refuses a connection while a running agent's listen queue is full, the same
    // error as for a socket left by an agent that died. A running agent holds the lock on the
    // data directory, so a refusal without it is no agent; with it, the refusal is retried
    // for a second and then reported, since the agent would keep the previous peer list.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut running = None;
    loop {
        let err = match request(dir, "reload").await {
            Ok(_) => return Ok(()),
            Err(err) => err,
        };
        match err.kind() {
            io::ErrorKind::NotFound => return Ok(()),
            io::ErrorKind::ConnectionRefused => {}
            _ => return Err(err),
        }
        if running.is_none() {
            running = Some(agent_running(dir)?);
        }
        if running == Some(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return refused_reload();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether an agent holds the lock on `dir`. Taking it for a moment may make an agent that
/// starts at that instant fail with "agent is already running".
#[cfg(unix)]
fn agent_running(dir: &Path) -> io::Result<bool> {
    match ipc::lock_dir(dir) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => Ok(true),
        Err(err) => Err(err),
    }
}

/// Windows does not look at the agent's lock here: a refusal that lasts a second counts as no
/// agent, as before.
#[cfg(windows)]
fn agent_running(_dir: &Path) -> io::Result<bool> {
    Ok(true)
}

#[cfg(unix)]
fn refused_reload() -> io::Result<()> {
    Err(io::Error::other(
        "the peers file is updated, but the running agent did not take the reload request and still uses the previous peer list; run the same command again, or restart the agent",
    ))
}

#[cfg(windows)]
fn refused_reload() -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn reads_control_response_until_exit() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("agent.sock")).unwrap();
        let remote = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = frame::read(&mut stream).await.unwrap().unwrap();
            assert_eq!(request.tag, CONTROL);
            assert_eq!(request.payload, b"status");
            frame::write(&mut stream, STDOUT, b"one\n").await.unwrap();
            frame::write(&mut stream, STDOUT, b"two\n").await.unwrap();
            frame::write(&mut stream, EXIT, &0_i32.to_be_bytes())
                .await
                .unwrap();
        });
        assert_eq!(request(dir.path(), "status").await.unwrap(), "one\ntwo\n");
        remote.await.unwrap();
    }

    #[tokio::test]
    async fn offline_reload_succeeds_but_status_fails() {
        let dir = tempfile::tempdir().unwrap();
        reload_if_running(dir.path()).await.unwrap();
        assert!(request(dir.path(), "status").await.is_err());
    }

    #[tokio::test]
    async fn reload_succeeds_at_once_on_a_socket_left_by_a_dead_agent() {
        let dir = tempfile::tempdir().unwrap();
        drop(UnixListener::bind(dir.path().join("agent.sock")).unwrap());
        let started = std::time::Instant::now();
        reload_if_running(dir.path()).await.unwrap();
        // No agent holds the lock, so the refusal is not retried.
        assert!(started.elapsed() < Duration::from_millis(500));
        // The lock is let go again.
        drop(ipc::lock_dir(dir.path()).unwrap());
    }

    /// A listener on agent.sock in `dir` whose listen queue is full, and the connections that
    /// fill it. macOS refuses a connection while the queue is full, the same error as a socket
    /// left by a dead agent.
    #[cfg(target_os = "macos")]
    fn full_listener(dir: &Path) -> (UnixListener, Vec<std::os::unix::net::UnixStream>) {
        let path = dir.join("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Shrink the backlog so a couple of connections fill it, not somaxconn worth of fds.
        let fd = std::os::fd::AsRawFd::as_raw_fd(&listener);
        assert_eq!(unsafe { libc::listen(fd, 1) }, 0);
        let mut queued = Vec::new();
        loop {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => queued.push(stream),
                Err(err) => {
                    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
                    break;
                }
            }
        }
        (listener, queued)
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn reload_fails_while_a_running_agent_keeps_its_listen_queue_full() {
        let dir = tempfile::tempdir().unwrap();
        // As a running agent holds it.
        let _lock = ipc::lock_dir(dir.path()).unwrap();
        let (listener, queued) = full_listener(dir.path());
        let started = std::time::Instant::now();
        let err = reload_if_running(dir.path()).await.unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert_eq!(
            err.to_string(),
            "the peers file is updated, but the running agent did not take the reload request and still uses the previous peer list; run the same command again, or restart the agent"
        );
        // No reload request was queued behind the connections that fill the queue.
        drop(queued);
        let mut accepted = 0;
        while let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
        {
            accepted += 1;
            assert!(frame::read(&mut stream).await.unwrap().is_none());
        }
        assert!(accepted >= 1);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn reload_reaches_an_agent_whose_listen_queue_is_briefly_full() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = ipc::lock_dir(dir.path()).unwrap();
        let (listener, queued) = full_listener(dir.path());
        let agent = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(queued);
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some(request) = frame::read(&mut stream).await.unwrap() else {
                    continue;
                };
                assert_eq!(request.tag, CONTROL);
                assert_eq!(request.payload, b"reload");
                frame::write(&mut stream, EXIT, &0_i32.to_be_bytes())
                    .await
                    .unwrap();
                return;
            }
        });
        reload_if_running(dir.path()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), agent)
            .await
            .expect("reload never reached the agent")
            .unwrap();
    }
}
