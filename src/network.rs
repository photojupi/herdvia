use crate::{
    exec,
    frame::{self, *},
    peers::{self, Peer},
};
use iroh::{
    Endpoint, EndpointId,
    endpoint::{
        AfterHandshakeOutcome, Connection, EndpointHooks, RecvStream, SendStream, Side, presets,
    },
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::watch,
    task::JoinSet,
};

pub const ALPN: &[u8] = b"hlink/1";

fn permitted(peers: &[Peer], id: EndpointId, inbound: bool) -> bool {
    peers.iter().any(|p| {
        p.id == id
            && if inbound {
                p.permission.inbound()
            } else {
                p.permission.outbound()
            }
    })
}

fn changed_peers(old: &[Peer], new: &[Peer]) -> HashSet<EndpointId> {
    old.iter()
        .filter(|p| !new.contains(p))
        .chain(new.iter().filter(|p| !old.contains(p)))
        .map(|p| p.id)
        .collect()
}

#[derive(Debug)]
struct Policy(watch::Receiver<Vec<Peer>>);

impl EndpointHooks for Policy {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        if conn.alpn() == ALPN
            && permitted(
                &self.0.borrow(),
                conn.remote_id(),
                conn.side() == Side::Server,
            )
        {
            AfterHandshakeOutcome::Accept
        } else {
            eprintln!("hlink: rejected peer {}", conn.remote_id());
            AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"peer not allowed".to_vec(),
            }
        }
    }
}

pub struct Network {
    pub endpoint: Endpoint,
    dir: PathBuf,
    peers: watch::Sender<Vec<Peer>>,
    connections: Mutex<HashMap<usize, Connection>>,
    changed: watch::Sender<u64>,
}

impl Network {
    pub async fn bind(dir: PathBuf) -> io::Result<Arc<Self>> {
        let key = peers::load_key(&dir)?;
        let (peers, policy) = watch::channel(peers::load(&dir)?);
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .hooks(Policy(policy))
            .bind()
            .await
            .map_err(io::Error::other)?;
        // No tracing subscriber is installed: iroh's internal logs remain off.
        Ok(Arc::new(Self {
            endpoint,
            dir,
            peers,
            connections: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
        }))
    }

    pub fn updates(&self) -> watch::Receiver<Vec<Peer>> {
        self.peers.subscribe()
    }

    pub fn outbound_ids(&self) -> HashSet<EndpointId> {
        self.peers
            .borrow()
            .iter()
            .filter(|p| p.permission.outbound())
            .map(|p| p.id)
            .collect()
    }

    pub fn reload(&self) -> io::Result<()> {
        let connections = self.connections.lock().expect("connections poisoned");
        let new = peers::load(&self.dir)?;
        let changed = changed_peers(&self.peers.borrow(), &new);
        self.peers.send_replace(new);
        for conn in connections
            .values()
            .filter(|c| changed.contains(&c.remote_id()))
        {
            conn.close(403u32.into(), b"peer permissions changed");
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        Ok(())
    }

    pub fn status(&self) -> String {
        let connections = self.connections.lock().expect("connections poisoned");
        let mut text = String::from("name permission connection path rtt\n");
        for peer in self.peers.borrow().iter() {
            let state = if let Some(conn) = connections
                .values()
                .find(|c| c.remote_id() == peer.id && c.close_reason().is_none())
            {
                let paths = conn.paths();
                if let Some(path) = paths
                    .iter()
                    .find(|p| p.is_selected())
                    .or_else(|| paths.iter().next())
                {
                    let route = if path.is_ip() {
                        "direct"
                    } else if path.is_relay() {
                        "relay"
                    } else {
                        "unknown"
                    };
                    format!(
                        "connected {route} {:.2}ms",
                        path.rtt().as_secs_f64() * 1000.0
                    )
                } else {
                    "connected unknown -".into()
                }
            } else {
                "disconnected - -".into()
            };
            text.push_str(&format!(
                "{} {} {state}\n",
                peer.name,
                peer.permission.as_str()
            ));
        }
        text
    }

    pub async fn outbound(self: Arc<Self>, id: EndpointId) {
        let mut delay = 1;
        loop {
            match self.endpoint.connect(id, ALPN).await {
                Ok(conn) => {
                    delay = 1;
                    self.clone().serve_connection(conn).await;
                }
                Err(err) => eprintln!("hlink: connection to {id} failed: {err}"),
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(30);
        }
    }

    pub async fn serve_connection(self: Arc<Self>, conn: Connection) {
        let id = conn.remote_id();
        {
            let mut connections = self.connections.lock().expect("connections poisoned");
            if !permitted(&self.peers.borrow(), id, conn.side() == Side::Server) {
                conn.close(403u32.into(), b"peer not allowed");
                return;
            }
            connections.insert(conn.stable_id(), conn.clone());
        }
        let _registration = Registration {
            network: self.clone(),
            id: conn.stable_id(),
        };
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        eprintln!("hlink: connected {id}");
        let mut streams = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = conn.closed() => break,
                accepted = conn.accept_bi() => {
                    let Ok((send, recv)) = accepted else { break; };
                    if !permitted(&self.peers.borrow(), id, true) {
                        conn.close(403u32.into(), b"inbound permission required");
                        break;
                    }
                    let network = self.clone();
                    let conn = conn.clone();
                    streams.spawn(async move {
                        if !permitted(&network.peers.borrow(), id, true) { return; }
                        let stopped = send.stopped();
                        tokio::select! {
                            biased;
                            _ = conn.closed() => {},
                            _ = stopped => {},
                            result = exec::serve(tokio::io::join(recv, send)) => {
                                if let Err(err) = result { eprintln!("hlink: stream from {id} ended: {err}"); }
                            }
                        }
                    });
                }
                _ = streams.join_next(), if !streams.is_empty() => {},
            }
        }
        streams.abort_all();
        while streams.join_next().await.is_some() {}
        eprintln!("hlink: disconnected {id}");
    }

    async fn connection_for(&self, name: &str) -> io::Result<Connection> {
        let mut changed = self.changed.subscribe();
        loop {
            let peer = self
                .peers
                .borrow()
                .iter()
                .find(|p| p.name == name && p.permission.outbound())
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unknown peer or outbound permission denied",
                    )
                })?;
            if let Some(conn) = self
                .connections
                .lock()
                .expect("connections poisoned")
                .values()
                .find(|c| {
                    c.remote_id() == peer.id
                        && c.side() == Side::Client
                        && c.close_reason().is_none()
                })
                .cloned()
            {
                return Ok(conn);
            }
            changed.changed().await.map_err(io::Error::other)?;
        }
    }

    pub async fn forward<S>(&self, mut local: S, name: &str, line: &str) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let opened = tokio::time::timeout(Duration::from_secs(5), async {
            let conn = self.connection_for(name).await?;
            let (mut send, recv) = conn.open_bi().await.map_err(io::Error::other)?;
            // The remote's accept_bi only completes after these first bytes are sent.
            frame::write(&mut send, OPEN, line.as_bytes()).await?;
            Ok::<_, io::Error>((conn, send, recv))
        })
        .await;
        let (conn, send, recv) = match opened {
            Ok(Ok(streams)) => streams,
            result => {
                let err = match result {
                    Ok(Err(err)) => err,
                    Err(_) => io::Error::new(
                        io::ErrorKind::TimedOut,
                        "peer did not connect within 5 seconds",
                    ),
                    _ => unreachable!(),
                };
                frame::write(&mut local, ERROR, err.to_string().as_bytes()).await?;
                return Err(err);
            }
        };
        let (reader, writer) = tokio::io::split(local);
        let mut input = JoinSet::new();
        input.spawn(forward_input(reader, ResetSend(send)));
        let output = forward_output(recv, writer);
        tokio::pin!(output);
        let result = loop {
            tokio::select! {
                biased;
                result = &mut output => break result,
                _ = conn.closed() => break Err(io::Error::new(io::ErrorKind::ConnectionAborted, "peer connection closed")),
                result = input.join_next(), if !input.is_empty() => {
                    match result.expect("input task exists").map_err(io::Error::other)? {
                        // STOP_SENDING can arrive ahead of the remote's buffered output and EXIT.
                        Err(err) if matches!(err.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe) => {},
                        result => break result,
                    }
                }
            }
        };
        input.abort_all();
        while input.join_next().await.is_some() {}
        result
    }
}

struct Registration {
    network: Arc<Network>,
    id: usize,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(conn) = self
            .network
            .connections
            .lock()
            .expect("connections poisoned")
            .remove(&self.id)
        {
            conn.close(0u32.into(), b"session closed");
        }
        self.network.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

struct ResetSend(SendStream);
impl Drop for ResetSend {
    fn drop(&mut self) {
        let _ = self.0.reset(1u32.into());
    }
}

async fn forward_input<R: AsyncRead + Unpin>(mut reader: R, mut send: ResetSend) -> io::Result<()> {
    let mut eof = false;
    loop {
        let f = frame::read(&mut reader)
            .await?
            .ok_or_else(|| frame::invalid("via disconnected"))?;
        if eof || !matches!(f.tag, STDIN | STDIN_EOF) {
            return Err(frame::invalid("unexpected input frame"));
        }
        eof = f.tag == STDIN_EOF;
        frame::write(&mut send.0, f.tag, &f.payload).await?;
    }
}

async fn forward_output<W: AsyncWrite + Unpin>(
    mut recv: RecvStream,
    mut writer: W,
) -> io::Result<()> {
    loop {
        let f = frame::read(&mut recv)
            .await?
            .ok_or_else(|| frame::invalid("peer stream closed without EXIT"))?;
        if !matches!(f.tag, STDOUT | STDERR | EXIT | ERROR) {
            return Err(frame::invalid("unexpected output frame"));
        }
        frame::write(&mut writer, f.tag, &f.payload).await?;
        if matches!(f.tag, EXIT | ERROR) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peers::Permission;
    use iroh::SecretKey;

    #[test]
    fn authorization_is_directional_and_never_trusts_unknown_keys() {
        let id = SecretKey::generate().public();
        let mut peer = Peer {
            name: "p".into(),
            id,
            permission: Permission::In,
        };
        assert!(permitted(&[peer.clone()], id, true));
        assert!(!permitted(&[peer.clone()], id, false));
        assert!(!permitted(
            &[peer.clone()],
            SecretKey::generate().public(),
            true
        ));
        peer.permission = Permission::Out;
        assert!(!permitted(&[peer.clone()], id, true));
        assert!(permitted(&[peer.clone()], id, false));
        peer.permission = Permission::Both;
        assert!(permitted(&[peer.clone()], id, true));
        assert!(permitted(&[peer], id, false));
    }

    #[test]
    fn reload_invalidates_removed_changed_and_replaced_peers() {
        let id = SecretKey::generate().public();
        let old = Peer {
            name: "p".into(),
            id,
            permission: Permission::Both,
        };
        let only_old = std::slice::from_ref(&old);
        assert!(changed_peers(only_old, only_old).is_empty());
        assert!(changed_peers(only_old, &[]).contains(&id));
        let mut new = old.clone();
        new.permission = Permission::Out;
        assert!(changed_peers(only_old, std::slice::from_ref(&new)).contains(&id));
        new.id = SecretKey::generate().public();
        assert_eq!(
            changed_peers(&[old], &[new.clone()]),
            HashSet::from([id, new.id])
        );
    }
}
