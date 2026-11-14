use crate::{
    frame::{self, *},
    ipc, log,
    network::Network,
    peers,
};
use iroh::EndpointId;
use std::{collections::HashMap, io, path::PathBuf, sync::Arc};
use tokio::task::{AbortHandle, JoinSet};

/// `log_file`: the file that standard error was sent to, whose lines older than 180 days are removed.
/// On Unix they are removed only while standard error still appends to that file.
pub async fn run(log_file: Option<PathBuf>) -> io::Result<()> {
    ipc::reject_root()?;
    let mut shutdown = ipc::Shutdown::new()?;
    let dir = ipc::data_dir()?;
    let network = Network::bind(dir.clone()).await?;
    let mut listener = ipc::listen(&dir)?;
    // The first tick is immediate, so the first trim comes right after listening.
    let mut trim_checks = tokio::time::interval(log::CHECK_EVERY);
    let mut last_trim = None;
    let mut updates = network.updates();
    let mut sessions = JoinSet::new();
    let mut connectors = JoinSet::new();
    let mut workers = HashMap::new();
    refresh_workers(&network, &mut workers, &mut connectors);
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok(stream) => { sessions.spawn(connection(stream, network.clone())); }
                    Err(err) => break Err(err),
                }
            }
            incoming = network.endpoint.accept() => {
                let Some(incoming) = incoming else { break Ok(()); };
                let network = network.clone();
                sessions.spawn(async move {
                    let conn = incoming.await.map_err(io::Error::other)?;
                    network.serve_connection(conn).await;
                    Ok(())
                });
            }
            _ = updates.changed() => refresh_workers(&network, &mut workers, &mut connectors),
            completed = sessions.join_next(), if !sessions.is_empty() => {
                match completed {
                    Some(Ok(Err(err))) => eprintln!("{} herdvia: {err}", log::stamp()),
                    Some(Err(err)) => eprintln!("{} herdvia: session task: {err}", log::stamp()),
                    _ => {}
                }
            }
            _ = connectors.join_next(), if !connectors.is_empty() => {},
            // Awaited here, so the agent stops listening only after a trim finishes.
            _ = trim_checks.tick(), if log_file.is_some() => {
                if let Some(path) = &log_file {
                    log::trim_if_due(path, &mut last_trim).await;
                }
            }
            _ = shutdown.recv() => break Ok(()),
        }
    };
    sessions.abort_all();
    connectors.abort_all();
    while sessions.join_next().await.is_some() {}
    while connectors.join_next().await.is_some() {}
    network.endpoint.close().await;
    result
}

fn refresh_workers(
    network: &Arc<Network>,
    workers: &mut HashMap<EndpointId, AbortHandle>,
    tasks: &mut JoinSet<()>,
) {
    let ids = network.outbound_ids();
    workers.retain(|id, task| {
        if ids.contains(id) {
            true
        } else {
            task.abort();
            false
        }
    });
    for id in ids {
        workers
            .entry(id)
            .or_insert_with(|| tasks.spawn(network.clone().outbound(id)));
    }
}

enum Request {
    Open(String, String),
    Control(String),
}

fn request(frame: frame::Frame) -> io::Result<Request> {
    let text =
        String::from_utf8(frame.payload).map_err(|_| frame::invalid("request must be UTF-8"))?;
    match frame.tag {
        CONTROL => Ok(Request::Control(text)),
        OPEN => {
            let (peer, line) = text
                .split_once('\0')
                .ok_or_else(|| frame::invalid("local OPEN requires peer and line"))?;
            if !peers::valid_name(peer) {
                return Err(frame::invalid("invalid peer name"));
            }
            Ok(Request::Open(peer.to_owned(), line.to_owned()))
        }
        _ => Err(frame::invalid("first frame must be OPEN or CONTROL")),
    }
}

async fn connection(mut stream: ipc::ServerStream, network: Arc<Network>) -> io::Result<()> {
    ipc::check_peer(&stream)?;
    let first = frame::read(&mut stream)
        .await?
        .ok_or_else(|| frame::invalid("missing first frame"))?;
    match request(first)? {
        Request::Control(command) => {
            let result = match command.as_str() {
                "reload" => network.reload().map(|()| "reloaded\n".to_owned()),
                "status" => Ok(network.status()),
                "pending" => Ok(network.pending()),
                "doctor" => Ok(crate::doctor::agent_report(&network.endpoint)),
                _ => Err(frame::invalid("unknown control request")),
            };
            let text = match result {
                Ok(text) => text,
                Err(err) => {
                    frame::write(&mut stream, ERROR, err.to_string().as_bytes()).await?;
                    return Err(err);
                }
            };
            for chunk in text.as_bytes().chunks(MAX_PAYLOAD) {
                frame::write(&mut stream, STDOUT, chunk).await?;
            }
            frame::write(&mut stream, EXIT, &0_i32.to_be_bytes()).await
        }
        Request::Open(peer, line) => {
            let watcher = ipc::watch_disconnect(&stream)?;
            tokio::select! {
                result = network.forward(stream, &peer, &line) => result,
                err = ipc::disconnected(watcher) => Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_open_requires_a_valid_peer_and_separator() {
        for payload in [b"no-separator".as_slice(), b"bad/name\0line".as_slice()] {
            assert!(
                request(frame::Frame {
                    tag: OPEN,
                    payload: payload.to_vec()
                })
                .is_err()
            );
        }
        match request(frame::Frame {
            tag: OPEN,
            payload: b"work\0line".to_vec(),
        })
        .unwrap()
        {
            Request::Open(peer, line) => {
                assert_eq!(peer, "work");
                assert_eq!(line, "line");
            }
            _ => panic!("expected OPEN"),
        }
    }

    #[test]
    fn stdin_cannot_be_a_first_request() {
        assert!(
            request(frame::Frame {
                tag: STDIN,
                payload: vec![]
            })
            .is_err()
        );
    }
}
