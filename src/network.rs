use crate::{
    exec,
    frame::{self, *},
    peers::{self, Peer},
};
use iroh::{
    Endpoint, EndpointId,
    endpoint::{
        AfterHandshakeOutcome, Connection, EndpointHooks, QuicTransportConfig, RecvStream,
        SendStream, Side, VarInt, presets,
    },
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::watch,
    task::JoinSet,
};

pub const ALPN: &[u8] = b"herdvia/1";

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

const MAX_ATTEMPTS: usize = 16;
const ATTEMPT_TTL: Duration = Duration::from_secs(600);

/// Inbound connections rejected for lacking inbound permission: attempts and last seen.
/// Kept in memory only, so that `accept` can pick a peer by its fingerprint.
#[derive(Debug, Default)]
struct Attempts(HashMap<EndpointId, (u64, Instant)>);

impl Attempts {
    fn expire(&mut self, now: Instant) {
        self.0
            .retain(|_, (_, seen)| now.saturating_duration_since(*seen) < ATTEMPT_TTL);
    }

    fn record(&mut self, id: EndpointId, now: Instant) {
        self.expire(now);
        if !self.0.contains_key(&id)
            && self.0.len() >= MAX_ATTEMPTS
            && let Some(oldest) = self
                .0
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(id, _)| *id)
        {
            self.0.remove(&oldest);
        }
        let (count, seen) = self.0.entry(id).or_insert((0, now));
        *count += 1;
        *seen = now;
    }

    fn forget_permitted(&mut self, peers: &[Peer]) {
        self.0.retain(|id, _| !permitted(peers, *id, true));
    }

    /// Most recently seen first.
    fn list(&mut self, now: Instant) -> Vec<(EndpointId, u64, Duration)> {
        self.expire(now);
        let mut list: Vec<_> = self
            .0
            .iter()
            .map(|(id, (count, seen))| (*id, *count, now.saturating_duration_since(*seen)))
            .collect();
        list.sort_by_key(|(_, _, age)| *age);
        list
    }
}

/// Seconds to wait before the next outbound attempt. Only a connection that stayed up resets
/// the backoff: a receiver that rejects us right after the handshake still counts as a failure.
fn retry_delay(previous: u64, connected_for: Duration) -> u64 {
    if previous == 0 || connected_for >= Duration::from_secs(10) {
        1
    } else {
        (previous * 2).min(30)
    }
}

#[derive(Debug)]
struct Policy {
    peers: watch::Receiver<Vec<Peer>>,
    attempts: Arc<Mutex<Attempts>>,
}

impl EndpointHooks for Policy {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let server = conn.side() == Side::Server;
        if conn.alpn() == ALPN && permitted(&self.peers.borrow(), conn.remote_id(), server) {
            AfterHandshakeOutcome::Accept
        } else {
            if server && conn.alpn() == ALPN {
                let mut attempts = self.attempts.lock().expect("attempts poisoned");
                // Checked again under the lock: a reload may have granted inbound meanwhile.
                if !permitted(&self.peers.borrow(), conn.remote_id(), true) {
                    attempts.record(conn.remote_id(), Instant::now());
                }
            }
            eprintln!("herdvia: rejected peer {}", conn.remote_id());
            AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"peer not allowed".to_vec(),
            }
        }
    }
}

/// herdvia only uses bidirectional streams. Refusing unidirectional streams and datagrams keeps
/// any connected peer, even an outbound-only one, from parking data in streams nobody accepts.
/// The builder starts from iroh's defaults, so its keep-alive and path settings stay.
fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .max_concurrent_bidi_streams(VarInt::from_u32(16))
        .datagram_receive_buffer_size(None)
        .build()
}

pub struct Network {
    pub endpoint: Endpoint,
    dir: PathBuf,
    peers: watch::Sender<Vec<Peer>>,
    connections: Mutex<HashMap<usize, Connection>>,
    attempts: Arc<Mutex<Attempts>>,
    changed: watch::Sender<u64>,
}

impl Network {
    pub async fn bind(dir: PathBuf) -> io::Result<Arc<Self>> {
        let key = peers::load_key(&dir)?;
        let (peers, policy) = watch::channel(peers::load(&dir)?);
        let attempts = Arc::new(Mutex::new(Attempts::default()));
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .transport_config(transport_config())
            .hooks(Policy {
                peers: policy,
                attempts: attempts.clone(),
            })
            .bind()
            .await
            .map_err(io::Error::other)?;
        // No tracing subscriber is installed: iroh's internal logs remain off.
        Ok(Arc::new(Self {
            endpoint,
            dir,
            peers,
            connections: Mutex::new(HashMap::new()),
            attempts,
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
        self.peers.send_replace(new.clone());
        self.attempts
            .lock()
            .expect("attempts poisoned")
            .forget_permitted(&new);
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

    pub fn pending(&self) -> String {
        let mut text = String::from("fingerprint id attempts last-seen\n");
        for (id, attempts, age) in self
            .attempts
            .lock()
            .expect("attempts poisoned")
            .list(Instant::now())
        {
            text.push_str(&format!(
                "{} {id} {attempts} {}s\n",
                peers::fingerprint(id),
                age.as_secs()
            ));
        }
        text
    }

    pub async fn outbound(self: Arc<Self>, id: EndpointId) {
        let mut delay = 0;
        loop {
            let connected_for = match self.endpoint.connect(id, ALPN).await {
                Ok(conn) => {
                    let connected = Instant::now();
                    self.clone().serve_connection(conn).await;
                    connected.elapsed()
                }
                Err(err) => {
                    eprintln!("herdvia: connection to {id} failed: {err}");
                    Duration::ZERO
                }
            };
            delay = retry_delay(delay, connected_for);
            tokio::time::sleep(Duration::from_secs(delay)).await;
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
        eprintln!("herdvia: connected {id}");
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
                                if let Err(err) = result { eprintln!("herdvia: stream from {id} ended: {err}"); }
                            }
                        }
                    });
                }
                _ = streams.join_next(), if !streams.is_empty() => {},
            }
        }
        streams.abort_all();
        while streams.join_next().await.is_some() {}
        eprintln!("herdvia: disconnected {id}");
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

    #[test]
    fn attempts_count_repeats_and_list_most_recent_first() {
        let start = Instant::now();
        let (a, b) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        let mut attempts = Attempts::default();
        attempts.record(a, start);
        attempts.record(b, start + Duration::from_secs(1));
        attempts.record(a, start + Duration::from_secs(2));
        assert_eq!(
            attempts.list(start + Duration::from_secs(5)),
            vec![
                (a, 2, Duration::from_secs(3)),
                (b, 1, Duration::from_secs(4))
            ]
        );
    }

    #[test]
    fn attempts_keep_at_most_16_and_drop_the_least_recently_seen() {
        let start = Instant::now();
        let ids: Vec<_> = (0..17).map(|_| SecretKey::generate().public()).collect();
        let mut attempts = Attempts::default();
        for (i, id) in ids.iter().enumerate().take(16) {
            attempts.record(*id, start + Duration::from_secs(i as u64));
        }
        // Seeing the oldest again makes ids[1] the least recently seen.
        attempts.record(ids[0], start + Duration::from_secs(20));
        attempts.record(ids[16], start + Duration::from_secs(21));
        let listed: HashSet<_> = attempts
            .list(start + Duration::from_secs(22))
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(listed.len(), 16);
        assert!(listed.contains(&ids[0]) && listed.contains(&ids[16]));
        assert!(!listed.contains(&ids[1]));
    }

    #[test]
    fn attempts_expire_ten_minutes_after_last_seen() {
        let start = Instant::now();
        let (a, b) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        let mut attempts = Attempts::default();
        attempts.record(a, start);
        attempts.record(b, start + Duration::from_secs(60));
        let listed = attempts.list(start + Duration::from_secs(599));
        assert_eq!(listed.len(), 2);
        let listed = attempts.list(start + ATTEMPT_TTL);
        assert_eq!(listed, vec![(b, 1, Duration::from_secs(540))]);
        // An expired entry starts over instead of resuming its old count.
        attempts.record(a, start + ATTEMPT_TTL + Duration::from_secs(1));
        assert!(
            attempts
                .list(start + ATTEMPT_TTL + Duration::from_secs(1))
                .contains(&(a, 1, Duration::ZERO))
        );
    }

    #[test]
    fn attempts_forget_peers_that_gained_inbound_permission() {
        let start = Instant::now();
        let (a, b) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        let mut attempts = Attempts::default();
        attempts.record(a, start);
        attempts.record(b, start);
        let peers = [
            Peer {
                name: "a".into(),
                id: a,
                permission: Permission::Out,
            },
            Peer {
                name: "b".into(),
                id: b,
                permission: Permission::Both,
            },
        ];
        attempts.forget_permitted(&peers);
        assert_eq!(attempts.list(start), vec![(a, 1, Duration::ZERO)]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connected_peers_cannot_open_unidirectional_streams_or_send_datagrams() {
        // As in the audit: this side dials a peer that it allowed outbound only.
        let dir = tempfile::tempdir().unwrap();
        let remote_key = SecretKey::generate();
        let remote_id = remote_key.public();
        peers::allow(
            dir.path(),
            Peer {
                name: "r".into(),
                id: remote_id,
                permission: Permission::Out,
            },
        )
        .unwrap();
        let network = Network::bind(dir.path().to_owned()).await.unwrap();
        let remote = Endpoint::builder(presets::Minimal)
            .secret_key(remote_key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let port = remote
            .bound_sockets()
            .into_iter()
            .find(|a| a.is_ipv4())
            .unwrap()
            .port();
        let addr = iroh::EndpointAddr::new(remote_id).with_ip_addr(([127, 0, 0, 1], port).into());
        let accepted = tokio::spawn({
            let remote = remote.clone();
            async move { remote.accept().await.unwrap().await.unwrap() }
        });
        let local = tokio::time::timeout(
            Duration::from_secs(10),
            network.endpoint.connect(addr, ALPN),
        )
        .await
        .unwrap()
        .unwrap();
        let conn = tokio::time::timeout(Duration::from_secs(10), accepted)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), conn.open_uni())
                .await
                .is_err(),
            "a unidirectional stream was granted"
        );
        assert_eq!(conn.max_datagram_size(), None);
        tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
            .await
            .unwrap()
            .unwrap();
        local.close(0u32.into(), b"done");
        remote.close().await;
        network.endpoint.close().await;
    }

    #[test]
    fn retry_delay_backs_off_until_a_connection_stays_up() {
        let mut delay = 0;
        let mut seen = Vec::new();
        for _ in 0..7 {
            delay = retry_delay(delay, Duration::ZERO);
            seen.push(delay);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30]);
        // Rejected right after the handshake: still a failure.
        assert_eq!(retry_delay(4, Duration::from_millis(9_999)), 8);
        assert_eq!(retry_delay(30, Duration::from_secs(10)), 1);
    }
}
