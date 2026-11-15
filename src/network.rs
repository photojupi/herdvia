use crate::{
    exec,
    frame::{self, *},
    log::{self, Kind},
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
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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

/// A peer for the log: the name of its first inbound entry and its fingerprint.
fn describe(peers: &[Peer], id: EndpointId) -> String {
    let name = peers
        .iter()
        .find(|p| p.id == id && p.permission.inbound())
        .map_or("?", |p| p.name.as_str());
    format!("{name} (fingerprint {})", peers::fingerprint(id))
}

fn changed_peers(old: &[Peer], new: &[Peer]) -> HashSet<EndpointId> {
    old.iter()
        .filter(|p| !new.contains(p))
        .chain(new.iter().filter(|p| !old.contains(p)))
        .map(|p| p.id)
        .collect()
}

/// Most entries `pending` lists. `accept` refuses a full list: pushing out entries is open to anyone.
pub const MAX_ATTEMPTS: usize = 16;
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
    pairing: Arc<crate::pair::session::PairingGate>,
}

impl EndpointHooks for Policy {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        if conn.alpn() == crate::pair::codec::PAIR_ALPN {
            if self.pairing.allow_handshake(conn.side(), conn.remote_id()) {
                return AfterHandshakeOutcome::Accept;
            }
            log::line(
                Kind::Unauthenticated,
                &format!("rejected pairing connection {}", conn.remote_id()),
            );
            return AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"not listening for pairing".to_vec(),
            };
        }
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
            log::line(
                Kind::Unauthenticated,
                &format!("rejected peer {}", conn.remote_id()),
            );
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
    addr_hints: Mutex<HashMap<EndpointId, SocketAddr>>,
    pub(crate) pairing: Arc<crate::pair::session::PairingGate>,
    reconnect: watch::Sender<u64>,
    changed: watch::Sender<u64>,
    stopping: AtomicBool,
    /// Stream tasks whose `SessionEnd` has not been dropped yet.
    streams: watch::Sender<usize>,
}

impl Network {
    pub async fn bind(dir: PathBuf) -> io::Result<Arc<Self>> {
        let key = peers::load_key(&dir)?;
        let (peers, policy) = watch::channel(peers::load(&dir)?);
        let attempts = Arc::new(Mutex::new(Attempts::default()));
        let pairing = Arc::new(crate::pair::session::PairingGate::default());
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec(), crate::pair::codec::PAIR_ALPN.to_vec()])
            .transport_config(transport_config())
            .hooks(Policy {
                peers: policy,
                attempts: attempts.clone(),
                pairing: pairing.clone(),
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
            addr_hints: Mutex::new(HashMap::new()),
            pairing,
            reconnect: watch::channel(0).0,
            changed: watch::channel(0).0,
            stopping: AtomicBool::new(false),
            streams: watch::channel(0).0,
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn quic_port(&self) -> u16 {
        let sockets = self.endpoint.bound_sockets();
        sockets
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| sockets.first())
            .map_or(0, SocketAddr::port)
    }

    pub fn set_addr_hint(&self, id: EndpointId, addr: SocketAddr) {
        self.addr_hints
            .lock()
            .expect("addr_hints poisoned")
            .insert(id, addr);
    }

    /// Called before the agent aborts its tasks, so that sessions cut short say why.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// Waits, for at most a second, until aborted stream tasks have written their end lines.
    /// Aborting serve_connection only aborts its stream tasks; they are dropped later.
    pub async fn streams_ended(&self) {
        let mut streams = self.streams.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(1), streams.wait_for(|n| *n == 0)).await;
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
        self.reconnect.send_modify(|n| *n = n.wrapping_add(1));
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
        let mut reconnect = self.reconnect.subscribe();
        let mut delay = 0;
        loop {
            let _ = *reconnect.borrow_and_update();
            let addr = match self
                .addr_hints
                .lock()
                .expect("addr_hints poisoned")
                .get(&id)
                .copied()
            {
                Some(sock_addr) => iroh::EndpointAddr::new(id).with_ip_addr(sock_addr),
                None => iroh::EndpointAddr::new(id),
            };
            let connected_for = match self.endpoint.connect(addr, ALPN).await {
                Ok(conn) => {
                    let connected = Instant::now();
                    self.clone().serve_connection(conn).await;
                    connected.elapsed()
                }
                Err(err) => {
                    log::line(Kind::Other, &format!("connection to {id} failed: {err}"));
                    Duration::ZERO
                }
            };
            delay = retry_delay(delay, connected_for);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                Ok(()) = reconnect.changed() => {
                    delay = 0;
                }
            }
        }
    }

    pub async fn serve_connection(self: Arc<Self>, conn: Connection) {
        if conn.alpn() != ALPN {
            conn.close(403u32.into(), b"invalid alpn");
            return;
        }
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
        log::line(Kind::Other, &format!("connected {id}"));
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
                        let who = {
                            let peers = network.peers.borrow();
                            if !permitted(&peers, id, true) { return; }
                            describe(&peers, id)
                        };
                        let mut end = SessionEnd::new(&network, &conn);
                        let started = |name: Option<&str>| {
                            let text = match name {
                                Some(name) => format!("{who}, herdr session {name}"),
                                None => who.clone(),
                            };
                            log::line(Kind::Other, &format!("session started: {text}"));
                            let _ = end.session.set(text);
                        };
                        let stopped = send.stopped();
                        let reason = tokio::select! {
                            biased;
                            err = conn.closed() => err.to_string(),
                            _ = stopped => "the peer stopped the stream".to_owned(),
                            result = exec::serve(tokio::io::join(recv, send), started) => match result {
                                Ok(code) => format!("exit code {code}"),
                                Err(err) if end.session.get().is_none() => {
                                    log::line(Kind::Other, &format!("stream from {id} ended: {err}"));
                                    return;
                                }
                                Err(err) => err.to_string(),
                            },
                        };
                        end.reason = Some(reason);
                    });
                }
                _ = streams.join_next(), if !streams.is_empty() => {},
            }
        }
        streams.abort_all();
        while streams.join_next().await.is_some() {}
        log::line(Kind::Other, &format!("disconnected {id}"));
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
                // iroh's errors can carry a close reason that the remote chose.
                frame::write(
                    &mut local,
                    ERROR,
                    log::sanitize(&err.to_string()).as_bytes(),
                )
                .await?;
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

/// Writes a session's end line when dropped: the stream task can be aborted before it sees
/// the connection close (serve_connection aborts its streams once it does) or on agent shutdown.
struct SessionEnd<'a> {
    network: &'a Network,
    conn: &'a Connection,
    session: OnceLock<String>,
    reason: Option<String>,
}
impl<'a> SessionEnd<'a> {
    fn new(network: &'a Network, conn: &'a Connection) -> Self {
        network.streams.send_modify(|n| *n += 1);
        Self {
            network,
            conn,
            session: OnceLock::new(),
            reason: None,
        }
    }
}
impl Drop for SessionEnd<'_> {
    fn drop(&mut self) {
        if let Some(text) = self.session.get() {
            let reason = self.reason.take().unwrap_or_else(|| {
                if self.network.stopping.load(Ordering::Relaxed) {
                    "the agent stopped".to_owned()
                } else {
                    self.conn
                        .close_reason()
                        .map_or_else(|| "connection closed".to_owned(), |err| err.to_string())
                }
            });
            log::line(Kind::Other, &format!("session ended: {text}: {reason}"));
        }
        self.network.streams.send_modify(|n| *n -= 1);
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
    fn describes_a_peer_by_its_first_inbound_name_and_fingerprint() {
        let id = SecretKey::generate().public();
        let peer = |name: &str, permission| Peer {
            name: name.into(),
            id,
            permission,
        };
        let peers = [
            peer("out", Permission::Out),
            peer("first", Permission::In),
            peer("second", Permission::Both),
        ];
        assert_eq!(
            describe(&peers, id),
            format!("first (fingerprint {})", peers::fingerprint(id))
        );
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
    async fn connected_peers_get_at_most_16_bidirectional_streams_and_nothing_else() {
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
        // At most 16 bidirectional streams at once: the 17th waits until streams end. The QUIC
        // stack announces new streams only once more than an eighth of the limit has ended, so
        // all 16 end here.
        let mut streams = Vec::new();
        for _ in 0..16 {
            streams.push(
                tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), conn.open_bi())
                .await
                .is_err(),
            "a 17th bidirectional stream was granted"
        );
        for (send, _) in &mut streams {
            send.finish().unwrap();
            let (mut local_send, mut local_recv) =
                tokio::time::timeout(Duration::from_secs(5), local.accept_bi())
                    .await
                    .unwrap()
                    .unwrap();
            local_recv.read_to_end(0).await.unwrap();
            local_send.finish().unwrap();
        }
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

    #[tokio::test]
    async fn reload_wakes_sleeping_outbound_worker_immediately() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let id_b = peers::load_key(dir_b.path()).unwrap().public();
        peers::grant_outbound(dir_a.path(), "home", id_b).unwrap();

        let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
        let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
        let id_a = net_a.endpoint.id();
        net_a.set_addr_hint(id_b, ([127, 0, 0, 1], net_b.quic_port()).into());

        let accept_b = tokio::spawn({
            let net_b = net_b.clone();
            async move {
                let mut set = JoinSet::new();
                while let Some(incoming) = net_b.endpoint.accept().await {
                    let net_b = net_b.clone();
                    set.spawn(async move {
                        if let Ok(conn) = incoming.await {
                            net_b.serve_connection(conn).await;
                        }
                    });
                }
            }
        });
        let worker_a = tokio::spawn(net_a.clone().outbound(id_b));

        // Wait until B has rejected 3 attempts (1s + 2s = ~3s), putting A into a 4s backoff sleep.
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if net_b.pending().contains(&format!("{id_a} 3 ")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("expected 3 rejected attempts on B");

        peers::grant_inbound(dir_b.path(), Some("caller"), "caller", id_a).unwrap();
        net_b.reload().unwrap();
        net_a.reload().unwrap();

        tokio::time::timeout(Duration::from_millis(800), async {
            loop {
                if net_a.status().contains("home out connected ") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("reload should wake sleeping outbound worker immediately without waiting 4s");

        worker_a.abort();
        accept_b.abort();
        net_a.endpoint.close().await;
        net_b.endpoint.close().await;
    }
}
