use crate::{
    frame::{self, *},
    ipc,
};
use std::{io, path::Path};
use tokio::net::UnixStream;

pub async fn request(dir: &Path, command: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(dir.join("agent.sock")).await?;
    ipc::check_uid(&stream)?;
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
    match request(dir, "reload").await {
        Ok(_) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
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
}
