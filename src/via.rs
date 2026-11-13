use crate::{
    frame::{self, *},
    ipc,
};
use std::io;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
    task::JoinSet,
};

pub async fn run(peer: &str, line: &str) -> io::Result<i32> {
    let stream = UnixStream::connect(ipc::data_dir()?.join("agent.sock")).await?;
    ipc::check_uid(&stream)?;
    relay(
        stream,
        peer,
        line,
        tokio::io::stdin(),
        tokio::io::stdout(),
        tokio::io::stderr(),
    )
    .await
}

pub async fn relay<S, I, O, E>(
    mut stream: S,
    peer: &str,
    line: &str,
    input: I,
    output: O,
    error: E,
) -> io::Result<i32>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    if peer.contains('\0') || line.contains('\0') {
        return Err(frame::invalid("NUL in peer or command line"));
    }
    frame::write(&mut stream, OPEN, format!("{peer}\0{line}").as_bytes()).await?;
    let (reader, writer) = tokio::io::split(stream);
    let mut sender = JoinSet::new();
    sender.spawn(send_input(input, writer));
    let receiver = receive_output(reader, output, error);
    tokio::pin!(receiver);
    let result = tokio::select! {
        biased;
        result = &mut receiver => result,
        sent = sender.join_next() => {
            match sent.expect("input task exists").map_err(io::Error::other)? {
                Ok(()) => receiver.await,
                // A child may exit while input is still arriving. Read its queued EXIT.
                Err(err) if matches!(err.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected) => receiver.await,
                Err(err) => Err(err),
            }
        }
    };
    sender.abort_all();
    while sender.join_next().await.is_some() {}
    result
}

async fn send_input<I: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut input: I,
    mut writer: W,
) -> io::Result<()> {
    let mut buffer = vec![0; MAX_PAYLOAD];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            return frame::write(&mut writer, STDIN_EOF, &[]).await;
        }
        frame::write(&mut writer, STDIN, &buffer[..count]).await?;
    }
}

async fn receive_output<R, O, E>(mut reader: R, mut output: O, mut error: E) -> io::Result<i32>
where
    R: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    loop {
        let f = frame::read(&mut reader).await?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed without EXIT",
            )
        })?;
        match f.tag {
            STDOUT => {
                output.write_all(&f.payload).await?;
                output.flush().await?;
            }
            STDERR => {
                error.write_all(&f.payload).await?;
                error.flush().await?;
            }
            EXIT => {
                return Ok(i32::from_be_bytes(
                    f.payload.try_into().expect("validated EXIT length"),
                ));
            }
            ERROR => {
                return Err(io::Error::other(
                    String::from_utf8(f.payload).expect("validated UTF-8"),
                ));
            }
            _ => return Err(frame::invalid("unexpected output frame")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use std::{io::Cursor, time::Duration};

    #[tokio::test]
    async fn forwards_only_remote_bytes_and_preserves_exit_code() {
        let (client, mut server) = tokio::io::duplex(1024);
        let remote = tokio::spawn(async move {
            let open = frame::read(&mut server).await.unwrap().unwrap();
            assert_eq!(open.tag, OPEN);
            assert_eq!(open.payload, b"peer\0line");
            let stdin = frame::read(&mut server).await.unwrap().unwrap();
            assert_eq!(stdin.tag, STDIN);
            assert_eq!(stdin.payload, [0, 255]);
            assert_eq!(
                frame::read(&mut server).await.unwrap().unwrap().tag,
                STDIN_EOF
            );
            frame::write(&mut server, STDOUT, &[0, 255, 13])
                .await
                .unwrap();
            frame::write(&mut server, STDERR, b"err").await.unwrap();
            frame::write(&mut server, EXIT, &127_i32.to_be_bytes())
                .await
                .unwrap();
        });
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = relay(
            client,
            "peer",
            "line",
            Cursor::new(vec![0, 255]),
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        assert_eq!(code, 127);
        assert_eq!(out, [0, 255, 13]);
        assert_eq!(err, b"err");
        remote.await.unwrap();
    }

    #[tokio::test]
    async fn exit_does_not_wait_for_local_stdin_eof() {
        let (client, mut server) = tokio::io::duplex(1024);
        let (_input_writer, input) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            frame::read(&mut server).await.unwrap();
            frame::write(&mut server, EXIT, &0_i32.to_be_bytes())
                .await
                .unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            relay(client, "p", "l", input, Vec::new(), Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn rejects_error_eof_and_wrong_direction_frames() {
        for response in [
            None,
            Some((ERROR, b"denied".as_slice())),
            Some((STDIN, b"x".as_slice())),
        ] {
            let (client, mut server) = tokio::io::duplex(1024);
            let remote = tokio::spawn(async move {
                frame::read(&mut server).await.unwrap();
                if let Some((tag, payload)) = response {
                    frame::write(&mut server, tag, payload).await.unwrap();
                }
            });
            assert!(
                relay(client, "p", "l", tokio::io::empty(), Vec::new(), Vec::new())
                    .await
                    .is_err()
            );
            remote.await.unwrap();
        }
    }
}
