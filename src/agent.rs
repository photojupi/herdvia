use crate::{exec, frame, ipc};
use std::{
    io::{self, Cursor},
    os::fd::AsFd,
};
use tokio::{
    io::AsyncReadExt,
    net::UnixStream,
    signal::unix::{SignalKind, signal},
    task::JoinSet,
};

pub async fn run() -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let listener = ipc::listen(&ipc::data_dir()?)?;
    let mut sessions = JoinSet::new();
    let result = loop {
        tokio::select! {
            accepted = listener.socket.accept() => {
                match accepted {
                    Ok((stream, _)) => { sessions.spawn(connection(stream)); }
                    Err(err) => break Err(err),
                }
            }
            completed = sessions.join_next(), if !sessions.is_empty() => {
                match completed {
                    Some(Ok(Err(err))) => eprintln!("hlink: {err}"),
                    Some(Err(err)) => eprintln!("hlink: session task: {err}"),
                    _ => {}
                }
            }
            _ = interrupt.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
        }
    };
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
    result
}

async fn connection(mut stream: UnixStream) -> io::Result<()> {
    ipc::check_uid(&stream)?;
    let open = frame::read(&mut stream)
        .await?
        .ok_or_else(|| frame::invalid("missing OPEN"))?;
    if open.tag != frame::OPEN {
        return Err(frame::invalid("first frame must be OPEN"));
    }
    let payload = std::str::from_utf8(&open.payload).map_err(|_| frame::invalid("invalid OPEN"))?;
    let (_, line) = payload
        .split_once('\0')
        .ok_or_else(|| frame::invalid("local OPEN requires peer and line"))?;
    // Replace only OPEN. The remaining bytes flow directly through the original stream.
    let mut prefix = Vec::new();
    frame::write(&mut prefix, frame::OPEN, line.as_bytes()).await?;
    let watcher = stream.as_fd().try_clone_to_owned()?;
    let (reader, writer) = tokio::io::split(stream);
    tokio::select! {
        result = exec::serve(tokio::io::join(Cursor::new(prefix).chain(reader), writer)) => result,
        err = ipc::disconnected(watcher) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{self, *};

    #[tokio::test]
    async fn strips_peer_before_applying_command_validation() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(connection(server));
        frame::write(&mut client, OPEN, b"arbitrary-peer\0id")
            .await
            .unwrap();
        let response = frame::read(&mut client).await.unwrap().unwrap();
        assert_eq!(response.tag, ERROR);
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn rejects_local_open_without_separator() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(connection(server));
        frame::write(&mut client, OPEN, b"no-separator")
            .await
            .unwrap();
        assert!(task.await.unwrap().is_err());
    }
}
