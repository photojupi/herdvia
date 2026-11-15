use crate::{
    frame::{self, *},
    ipc, log,
    network::Network,
    peers,
};
use iroh::{EndpointId, endpoint::ConnectingError};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::task::{AbortHandle, JoinSet};

/// `log_file`: the file that standard error was sent to, whose lines older than 180 days are removed.
/// On Unix they are removed only while standard error still appends to that file.
pub async fn run(log_file: Option<PathBuf>) -> io::Result<()> {
    ipc::reject_root()?;
    ipc::reject_elevated()?;
    let mut shutdown = ipc::Shutdown::new()?;
    let dir = ipc::data_dir()?;
    let network = Network::bind(dir.clone()).await?;
    let mut listener = listen(&dir, &network)?;
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
                    match incoming.await {
                        Ok(conn) => {
                            if conn.alpn() == crate::pair::codec::PAIR_ALPN {
                                crate::pair::session::serve_incoming(network, conn).await;
                            } else {
                                network.serve_connection(conn).await;
                            }
                        }
                        // The hook has already logged the peer it rejected.
                        Err(ConnectingError::LocallyRejected { .. }) => {}
                        Err(err) => log::line(
                            log::Kind::Unauthenticated,
                            &format!("incoming connection failed: {err}"),
                        ),
                    }
                    Ok(())
                });
            }
            _ = updates.changed() => refresh_workers(&network, &mut workers, &mut connectors),
            completed = sessions.join_next(), if !sessions.is_empty() => {
                match completed {
                    Some(Ok(Err(err))) => log::line(log::Kind::Other, &err.to_string()),
                    Some(Err(err)) => log::line(log::Kind::Other, &format!("session task: {err}")),
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
    network.stop();
    sessions.abort_all();
    connectors.abort_all();
    while sessions.join_next().await.is_some() {}
    while connectors.join_next().await.is_some() {}
    network.streams_ended().await;
    network.endpoint.close().await;
    result
}

/// Opens the local channel, then reads peers again. allow and revoke treat a missing channel as
/// "no agent", so a change made after bind read peers would otherwise never reach this agent.
/// Changes made after the channel opens queue a reload request instead.
fn listen(dir: &Path, network: &Network) -> io::Result<ipc::Listener> {
    let listener = ipc::listen(dir)?;
    network.reload()?;
    Ok(listener)
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
        Request::Control(command) if matches!(command.as_str(), "listen" | "connect") => {
            let watcher = ipc::watch_disconnect(&stream)?;
            tokio::select! {
                result = async {
                    if command == "listen" {
                        crate::pair::session::serve_listen(stream, network).await
                    } else {
                        crate::pair::session::serve_connect(stream, network).await
                    }
                } => result,
                err = ipc::disconnected(watcher) => Err(err),
            }
        }
        Request::Control(command) => {
            let result = match command.as_str() {
                "reload" => network.reload().map(|()| "reloaded\n".to_owned()),
                "status" => Ok(network.status()),
                "status-detail" => Ok(network.status_detail()),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_revoke_before_the_local_channel_opens_still_takes_effect() {
        let dir = tempfile::tempdir().unwrap();
        let id = iroh::SecretKey::generate().public();
        peers::allow(
            dir.path(),
            peers::Peer {
                name: "p".into(),
                id,
                permission: peers::Permission::Both,
            },
        )
        .unwrap();
        let network = Network::bind(dir.path().to_owned()).await.unwrap();
        assert!(network.outbound_ids().contains(&id));
        // The agent has read peers but has no local channel yet: revoke finds no agent to tell.
        peers::revoke(dir.path(), "p").unwrap();
        crate::control::reload_if_running(dir.path()).await.unwrap();
        assert!(network.outbound_ids().contains(&id));
        let _listener = listen(dir.path(), &network).unwrap();
        assert!(network.outbound_ids().is_empty());
        assert_eq!(network.status(), "name permission connection path rtt\n");
        network.endpoint.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_agent_answers_status_detail_next_to_the_plain_status() {
        let dir = tempfile::tempdir().unwrap();
        peers::allow(
            dir.path(),
            peers::Peer {
                name: "tab".into(),
                id: iroh::SecretKey::generate().public(),
                permission: peers::Permission::In,
            },
        )
        .unwrap();
        let network = Network::bind(dir.path().to_owned()).await.unwrap();
        let mut listener = listen(dir.path(), &network).unwrap();
        let serving = tokio::spawn({
            let network = network.clone();
            async move {
                loop {
                    let stream = listener.accept().await.unwrap();
                    let _ = connection(stream, network.clone()).await;
                }
            }
        });
        assert_eq!(
            crate::control::request(dir.path(), "status-detail")
                .await
                .unwrap(),
            "herdvia-status-detail 1\ntab in waiting - - - -\n"
        );
        assert_eq!(
            crate::control::request(dir.path(), "status").await.unwrap(),
            "name permission connection path rtt\ntab in disconnected - -\n"
        );
        serving.abort();
        network.endpoint.close().await;
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
