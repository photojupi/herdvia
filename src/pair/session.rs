use crate::{
    frame::{self, ERROR, EXIT, STDIN, STDOUT},
    log::{self, Kind},
    network::Network,
    pair::codec::{
        AuthKind, PAIR_ALPN, PairMsg, auth_proof, compute_commit_a, derive_sas, encode_invite,
        random_bytes, read_msg, sanitize_hostname, verify_auth_proof, verify_commit_a, write_msg,
    },
    peers::{self, Peer},
};
use data_encoding::HEXLOWER;
use iroh::{
    EndpointAddr, EndpointId,
    endpoint::{Connection, Side},
};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};

const LISTEN_TTL: Duration = Duration::from_secs(300);
const STEP_TIMEOUT: Duration = Duration::from_secs(15);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const CLIENT_APPROVAL_WAIT: Duration = Duration::from_secs(65);
const MAX_INFLIGHT_PAIR_CONNS: usize = 4;
const MAX_FAILED_PROOFS: usize = 5;

#[derive(Debug, Default)]
pub struct PairingGate {
    next_id: AtomicU64,
    active_listen: Mutex<Option<Arc<ListenSession>>>,
    outbound_targets: Mutex<HashMap<EndpointId, usize>>,
}

impl PairingGate {
    pub fn allow_handshake(&self, side: Side, remote_id: EndpointId) -> bool {
        match side {
            Side::Server => {
                let guard = self.active_listen.lock().expect("active_listen poisoned");
                let Some(session) = guard.as_ref() else {
                    return false;
                };
                !session.completed.load(Ordering::Acquire)
                    && Instant::now() < session.expires_at
                    && session.inflight.load(Ordering::Acquire) < MAX_INFLIGHT_PAIR_CONNS
            }
            Side::Client => self
                .outbound_targets
                .lock()
                .expect("outbound_targets poisoned")
                .get(&remote_id)
                .is_some_and(|&n| n > 0),
        }
    }

    fn current_listen(&self) -> Option<Arc<ListenSession>> {
        let guard = self.active_listen.lock().expect("active_listen poisoned");
        let session = guard.as_ref()?.clone();
        if session.completed.load(Ordering::Acquire) || Instant::now() >= session.expires_at {
            return None;
        }
        Some(session)
    }

    fn register_listen(
        self: &Arc<Self>,
        session: Arc<ListenSession>,
    ) -> io::Result<ListenRegistration> {
        let mut guard = self.active_listen.lock().expect("active_listen poisoned");
        if let Some(existing) = guard.as_ref()
            && !existing.completed.load(Ordering::Acquire)
            && Instant::now() < existing.expires_at
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a listen session is already active",
            ));
        }
        let id = session.id;
        *guard = Some(session.clone());
        Ok(ListenRegistration {
            gate: self.clone(),
            session,
            id,
        })
    }

    fn clear_listen(&self, id: u64) {
        let mut guard = self.active_listen.lock().expect("active_listen poisoned");
        if guard.as_ref().is_some_and(|s| s.id == id) {
            *guard = None;
        }
    }

    fn register_connect(self: &Arc<Self>, target: EndpointId) -> ConnectRegistration {
        *self
            .outbound_targets
            .lock()
            .expect("outbound_targets poisoned")
            .entry(target)
            .or_insert(0) += 1;
        ConnectRegistration {
            gate: self.clone(),
            target,
        }
    }
}

struct ListenRegistration {
    gate: Arc<PairingGate>,
    session: Arc<ListenSession>,
    id: u64,
}

impl Drop for ListenRegistration {
    fn drop(&mut self) {
        self.session.completed.store(true, Ordering::Release);
        self.gate.clear_listen(self.id);
        self.session.close_all_conns();
    }
}

struct ConnectRegistration {
    gate: Arc<PairingGate>,
    target: EndpointId,
}

impl Drop for ConnectRegistration {
    fn drop(&mut self) {
        let mut guard = self
            .gate
            .outbound_targets
            .lock()
            .expect("outbound_targets poisoned");
        if let Some(count) = guard.get_mut(&self.target) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                guard.remove(&self.target);
            }
        }
    }
}

struct ListenSession {
    id: u64,
    explicit_name: Option<String>,
    listen_id: [u8; 16],
    invite_secret: [u8; 16],
    hostname: String,
    expires_at: Instant,
    inflight: AtomicUsize,
    failed_attempts: AtomicUsize,
    completed: AtomicBool,
    prompt_lock: tokio::sync::Mutex<()>,
    conns: Mutex<HashMap<usize, Connection>>,
    event_tx: mpsc::Sender<ListenEvent>,
}

impl std::fmt::Debug for ListenSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenSession")
            .field("id", &self.id)
            .field("hostname", &self.hostname)
            .finish()
    }
}

impl ListenSession {
    fn close_all_conns(&self) {
        let mut conns = self.conns.lock().expect("pair conns poisoned");
        for conn in conns.drain().map(|(_, c)| c) {
            conn.close(0u32.into(), b"pairing session ended");
        }
    }
}

struct InflightGuard {
    session: Arc<ListenSession>,
    stable_id: usize,
}

impl InflightGuard {
    fn try_acquire(session: Arc<ListenSession>, conn: &Connection) -> Option<Self> {
        let prev = session.inflight.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_INFLIGHT_PAIR_CONNS || session.completed.load(Ordering::Acquire) {
            session.inflight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let stable_id = conn.stable_id();
        session
            .conns
            .lock()
            .expect("pair conns poisoned")
            .insert(stable_id, conn.clone());
        if session.completed.load(Ordering::Acquire) {
            session
                .conns
                .lock()
                .expect("pair conns poisoned")
                .remove(&stable_id);
            session.inflight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self { session, stable_id })
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.session
            .conns
            .lock()
            .expect("pair conns poisoned")
            .remove(&self.stable_id);
        self.session.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

enum ListenEvent {
    Prompt(PromptRequest),
    Abort(String),
}

struct PromptRequest {
    local_peer_name: String,
    caller_name: String,
    caller_id: EndpointId,
    sas: String,
    decision_tx: oneshot::Sender<PromptDecision>,
}

enum PromptDecision {
    Approve {
        saved: Peer,
        ack_tx: oneshot::Sender<()>,
    },
    Reject {
        ack_tx: oneshot::Sender<()>,
    },
}

pub fn local_hostname(fallback: EndpointId) -> String {
    #[cfg(unix)]
    let raw = {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        }
    };
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").unwrap_or_default();
    sanitize_hostname(&raw, fallback)
}

fn export_tls_binding(conn: &Connection) -> io::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"herdvia-pair-v1", &[])
        .map_err(|_| frame::invalid("TLS exporter failed"))?;
    Ok(out)
}

fn decode_hex_16(s: &str) -> io::Result<[u8; 16]> {
    if s.len() != 32 {
        return Err(frame::invalid("invalid pairing token hex length"));
    }
    let lower = s.to_ascii_lowercase();
    let vec = HEXLOWER
        .decode(lower.as_bytes())
        .map_err(|_| frame::invalid("invalid pairing token hex"))?;
    vec.try_into()
        .map_err(|_| frame::invalid("invalid pairing token length"))
}

async fn record_failed_attempt(session: &ListenSession) {
    let failed = session.failed_attempts.fetch_add(1, Ordering::AcqRel) + 1;
    if failed >= MAX_FAILED_PROOFS {
        session.completed.store(true, Ordering::Release);
        let _ = session
            .event_tx
            .send(ListenEvent::Abort(
                "too many invalid pairing attempts; run herdvia listen again".into(),
            ))
            .await;
    }
}

pub async fn serve_incoming(network: Arc<Network>, conn: Connection) {
    let Some(session) = network.pairing.current_listen() else {
        conn.close(403u32.into(), b"not listening");
        return;
    };
    let Some(_inflight) = InflightGuard::try_acquire(session.clone(), &conn) else {
        conn.close(429u32.into(), b"busy");
        return;
    };

    let Ok(tls_binding) = export_tls_binding(&conn) else {
        conn.close(400u32.into(), b"tls exporter failed");
        return;
    };
    let Ok(Ok((mut send, mut recv))) = tokio::time::timeout(STEP_TIMEOUT, conn.accept_bi()).await
    else {
        conn.close(400u32.into(), b"stream timeout");
        return;
    };

    let hello = match tokio::time::timeout(STEP_TIMEOUT, read_msg(&mut recv)).await {
        Ok(Ok(PairMsg::Hello {
            auth_kind,
            auth_proof,
            commit_a,
            caller_name,
        })) => (auth_kind, auth_proof, commit_a, caller_name),
        _ => {
            log::line(
                Kind::Unauthenticated,
                &format!("invalid pairing hello from {}", conn.remote_id()),
            );
            conn.close(400u32.into(), b"invalid hello");
            return;
        }
    };

    let (auth_kind, recv_proof, commit_a, caller_name) = hello;
    let expected_token = match auth_kind {
        AuthKind::Invite => &session.invite_secret,
        AuthKind::Lan => &session.listen_id,
    };
    if !verify_auth_proof(auth_kind, expected_token, &tls_binding, &recv_proof) {
        log::line(
            Kind::Unauthenticated,
            &format!("invalid pairing proof from {}", conn.remote_id()),
        );
        let _ = tokio::time::timeout(
            STEP_TIMEOUT,
            write_msg(
                &mut send,
                &PairMsg::Rejected {
                    reason: "invalid or expired pairing token".into(),
                },
            ),
        )
        .await;
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_millis(100), send.stopped()).await;
        record_failed_attempt(&session).await;
        conn.close(403u32.into(), b"invalid token");
        return;
    }

    let Ok(_prompt_guard) = session.prompt_lock.try_lock() else {
        let _ = tokio::time::timeout(
            STEP_TIMEOUT,
            write_msg(
                &mut send,
                &PairMsg::Rejected {
                    reason: "target PC is busy confirming another request".into(),
                },
            ),
        )
        .await;
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_millis(100), send.stopped()).await;
        conn.close(429u32.into(), b"busy");
        return;
    };

    if session.completed.load(Ordering::Acquire) || Instant::now() >= session.expires_at {
        conn.close(403u32.into(), b"expired");
        return;
    }

    let nonce_b: [u8; 32] = random_bytes();
    if !matches!(
        tokio::time::timeout(
            STEP_TIMEOUT,
            write_msg(
                &mut send,
                &PairMsg::Challenge {
                    nonce_b,
                    target_name: session.hostname.clone(),
                },
            ),
        )
        .await,
        Ok(Ok(()))
    ) {
        record_failed_attempt(&session).await;
        conn.close(400u32.into(), b"challenge write failed");
        return;
    }

    let nonce_a = match tokio::time::timeout(STEP_TIMEOUT, read_msg(&mut recv)).await {
        Ok(Ok(PairMsg::Reveal { nonce_a })) => nonce_a,
        _ => {
            log::line(
                Kind::Unauthenticated,
                &format!("missing pairing reveal from {}", conn.remote_id()),
            );
            record_failed_attempt(&session).await;
            conn.close(400u32.into(), b"invalid reveal");
            return;
        }
    };

    let caller_id = conn.remote_id();
    let self_id = network.endpoint.id();
    if !verify_commit_a(&tls_binding, caller_id, self_id, &nonce_a, &commit_a) {
        log::line(
            Kind::Unauthenticated,
            &format!("pairing commit mismatch from {caller_id}"),
        );
        record_failed_attempt(&session).await;
        conn.close(400u32.into(), b"commit mismatch");
        return;
    }

    let preview = match peers::load(network.dir()).and_then(|existing| {
        peers::resolve_inbound_peer(
            &existing,
            session.explicit_name.as_deref(),
            &caller_name,
            caller_id,
        )
    }) {
        Ok(peer) => peer,
        Err(err) => {
            let _ = tokio::time::timeout(
                STEP_TIMEOUT,
                write_msg(
                    &mut send,
                    &PairMsg::Rejected {
                        reason: "peer name conflict on target PC".into(),
                    },
                ),
            )
            .await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_millis(100), send.stopped()).await;
            let _ = session
                .event_tx
                .send(ListenEvent::Abort(err.to_string()))
                .await;
            conn.close(409u32.into(), b"name conflict");
            return;
        }
    };

    let sas = derive_sas(&tls_binding, caller_id, self_id, &nonce_a, &nonce_b);
    let (decision_tx, decision_rx) = oneshot::channel();
    if session
        .event_tx
        .send(ListenEvent::Prompt(PromptRequest {
            local_peer_name: preview.name,
            caller_name,
            caller_id,
            sas,
            decision_tx,
        }))
        .await
        .is_err()
    {
        conn.close(0u32.into(), b"cancelled");
        return;
    }

    match decision_rx.await {
        Ok(PromptDecision::Approve { saved, ack_tx }) => {
            let _ =
                tokio::time::timeout(STEP_TIMEOUT, write_msg(&mut send, &PairMsg::Approved)).await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_millis(200), send.stopped()).await;
            log::line(
                Kind::Other,
                &format!(
                    "pair approved: {} (fingerprint {})",
                    saved.name,
                    peers::fingerprint(caller_id)
                ),
            );
            let _ = ack_tx.send(());
        }
        Ok(PromptDecision::Reject { ack_tx }) => {
            let _ = tokio::time::timeout(
                STEP_TIMEOUT,
                write_msg(
                    &mut send,
                    &PairMsg::Rejected {
                        reason: "declined on target PC".into(),
                    },
                ),
            )
            .await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_millis(150), send.stopped()).await;
            log::line(
                Kind::Other,
                &format!("pair declined: {}", peers::fingerprint(caller_id)),
            );
            let _ = ack_tx.send(());
        }
        Err(_) => {
            conn.close(0u32.into(), b"cancelled");
        }
    }
}

async fn write_ipc_error<W: AsyncWrite + Unpin>(writer: &mut W, err: &io::Error) {
    let _ = frame::write(writer, ERROR, log::sanitize(&err.to_string()).as_bytes()).await;
}

pub async fn serve_listen<S>(stream: S, network: Arc<Network>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_listen_with_ttl(stream, network, LISTEN_TTL).await
}

async fn serve_listen_with_ttl<S>(stream: S, network: Arc<Network>, ttl: Duration) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let cfg_frame = match tokio::time::timeout(STEP_TIMEOUT, frame::read(&mut reader)).await {
        Ok(Ok(Some(f))) if f.tag == STDIN => f,
        Ok(Ok(None)) => return Ok(()),
        Ok(Ok(_)) => {
            let err = frame::invalid("expected STDIN config frame for listen");
            write_ipc_error(&mut writer, &err).await;
            return Err(err);
        }
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            let err = io::Error::new(io::ErrorKind::TimedOut, "timed out reading listen config");
            write_ipc_error(&mut writer, &err).await;
            return Err(err);
        }
    };

    let explicit_raw = std::str::from_utf8(&cfg_frame.payload)
        .map_err(|_| frame::invalid("listen config must be UTF-8"))?
        .trim();
    let explicit_name = if explicit_raw.is_empty() {
        None
    } else if peers::valid_name(explicit_raw) {
        Some(explicit_raw.to_owned())
    } else {
        let err = frame::invalid("invalid peer name");
        write_ipc_error(&mut writer, &err).await;
        return Err(err);
    };

    let self_id = network.endpoint.id();
    let listen_id: [u8; 16] = random_bytes();
    let invite_secret: [u8; 16] = random_bytes();
    let invite_token = encode_invite(self_id, &invite_secret);
    let hostname = local_hostname(self_id);
    let quic_port = network.quic_port();
    let (event_tx, mut event_rx) = mpsc::channel(4);

    let session_id = network.pairing.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let session = Arc::new(ListenSession {
        id: session_id,
        explicit_name,
        listen_id,
        invite_secret,
        hostname: hostname.clone(),
        expires_at: Instant::now() + ttl,
        inflight: AtomicUsize::new(0),
        failed_attempts: AtomicUsize::new(0),
        completed: AtomicBool::new(false),
        prompt_lock: tokio::sync::Mutex::new(()),
        conns: Mutex::new(HashMap::new()),
        event_tx,
    });

    let registration = match network.pairing.register_listen(session.clone()) {
        Ok(reg) => reg,
        Err(err) => {
            write_ipc_error(&mut writer, &err).await;
            return Err(err);
        }
    };

    let ready_line = format!(
        "READY {self_id} {quic_port} {} {invite_token} {hostname}\n",
        HEXLOWER.encode(&listen_id)
    );
    frame::write(&mut writer, STDOUT, ready_line.as_bytes()).await?;

    let deadline = tokio::time::Instant::from_std(session.expires_at);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                drop(registration);
                frame::write(&mut writer, STDOUT, b"TIMEOUT\n").await?;
                frame::write(&mut writer, EXIT, &1_i32.to_be_bytes()).await?;
                return Ok(());
            }
            ipc_in = frame::read(&mut reader) => {
                match ipc_in {
                    Ok(None) | Err(_) => return Ok(()),
                    Ok(Some(_)) => {}
                }
            }
            evt = event_rx.recv() => {
                let Some(evt) = evt else { return Ok(()); };
                match evt {
                    ListenEvent::Abort(reason) => {
                        drop(registration);
                        let err = io::Error::new(io::ErrorKind::PermissionDenied, reason);
                        write_ipc_error(&mut writer, &err).await;
                        return Err(err);
                    }
                    ListenEvent::Prompt(req) => {
                        let prompt_line = format!(
                            "PROMPT {} {} {} {}\n",
                            req.local_peer_name,
                            req.caller_id,
                            peers::fingerprint(req.caller_id),
                            req.sas
                        );
                        frame::write(&mut writer, STDOUT, prompt_line.as_bytes()).await?;

                        let approval_deadline = tokio::time::Instant::now() + APPROVAL_TIMEOUT;
                        let mut event_closed = false;
                        let approved = loop {
                            tokio::select! {
                                biased;
                                _ = tokio::time::sleep_until(deadline) => {
                                    session.completed.store(true, Ordering::Release);
                                    network.pairing.clear_listen(session.id);
                                    drop(registration);
                                    frame::write(&mut writer, STDOUT, b"TIMEOUT\n").await?;
                                    frame::write(&mut writer, EXIT, &1_i32.to_be_bytes()).await?;
                                    return Ok(());
                                }
                                evt = event_rx.recv(), if !event_closed => {
                                    match evt {
                                        Some(ListenEvent::Abort(reason)) => {
                                            session.completed.store(true, Ordering::Release);
                                            network.pairing.clear_listen(session.id);
                                            drop(registration);
                                            let err = io::Error::new(io::ErrorKind::PermissionDenied, reason);
                                            write_ipc_error(&mut writer, &err).await;
                                            return Err(err);
                                        }
                                        Some(ListenEvent::Prompt(_)) => {}
                                        None => event_closed = true,
                                    }
                                }
                                _ = tokio::time::sleep_until(approval_deadline) => {
                                    break false;
                                }
                                ipc_in = frame::read(&mut reader) => {
                                    match ipc_in {
                                        Ok(None) | Err(_) => {
                                            drop(registration);
                                            return Ok(());
                                        }
                                        Ok(Some(f)) => {
                                            break f.tag == STDIN
                                                && std::str::from_utf8(&f.payload).ok().map(str::trim)
                                                    == Some("approve");
                                        }
                                    }
                                }
                            }
                        };

                        let claimed = Instant::now() < session.expires_at
                            && session
                                .completed
                                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok();
                        session.completed.store(true, Ordering::Release);
                        network.pairing.clear_listen(session.id);

                        if !claimed {
                            while let Ok(evt) = event_rx.try_recv() {
                                if let ListenEvent::Abort(reason) = evt {
                                    drop(registration);
                                    let err =
                                        io::Error::new(io::ErrorKind::PermissionDenied, reason);
                                    write_ipc_error(&mut writer, &err).await;
                                    return Err(err);
                                }
                            }
                            drop(registration);
                            frame::write(&mut writer, STDOUT, b"TIMEOUT\n").await?;
                            frame::write(&mut writer, EXIT, &1_i32.to_be_bytes()).await?;
                            return Ok(());
                        }

                        if approved {
                            match peers::grant_inbound(
                                network.dir(),
                                session.explicit_name.as_deref(),
                                &req.caller_name,
                                req.caller_id,
                            )
                            .and_then(|saved| network.reload().map(|()| saved))
                            {
                                Ok(saved) => {
                                    let (ack_tx, ack_rx) = oneshot::channel();
                                    let _ = req.decision_tx.send(PromptDecision::Approve {
                                        saved: saved.clone(),
                                        ack_tx,
                                    });
                                    let _ = tokio::time::timeout(Duration::from_millis(300), ack_rx).await;
                                    drop(registration);
                                    let out = format!(
                                        "APPROVED {} {} {}\n",
                                        saved.name,
                                        req.caller_id,
                                        saved.permission.as_str()
                                    );
                                    frame::write(&mut writer, STDOUT, out.as_bytes()).await?;
                                    frame::write(&mut writer, EXIT, &0_i32.to_be_bytes()).await?;
                                    return Ok(());
                                }
                                Err(err) => {
                                    let (ack_tx, ack_rx) = oneshot::channel();
                                    let _ = req.decision_tx.send(PromptDecision::Reject { ack_tx });
                                    let _ = tokio::time::timeout(Duration::from_millis(200), ack_rx).await;
                                    drop(registration);
                                    write_ipc_error(&mut writer, &err).await;
                                    return Err(err);
                                }
                            }
                        } else {
                            let (ack_tx, ack_rx) = oneshot::channel();
                            let _ = req.decision_tx.send(PromptDecision::Reject { ack_tx });
                            let _ = tokio::time::timeout(Duration::from_millis(200), ack_rx).await;
                            drop(registration);
                            frame::write(&mut writer, STDOUT, b"REJECTED\n").await?;
                            frame::write(&mut writer, EXIT, &1_i32.to_be_bytes()).await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

pub async fn serve_connect<S>(stream: S, network: Arc<Network>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_connect_with_timeout(stream, network, APPROVAL_TIMEOUT).await
}

async fn serve_connect_with_timeout<S>(
    stream: S,
    network: Arc<Network>,
    confirm_timeout: Duration,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let result = serve_connect_inner(&mut reader, &mut writer, &network, confirm_timeout).await;
    if let Err(ref err) = result {
        write_ipc_error(&mut writer, err).await;
    }
    result
}

/// The target's answer to a pairing request: `Ok(None)` when it approved, `Ok(Some(reason))`
/// when it rejected.
fn target_decision(
    msg: Result<io::Result<PairMsg>, tokio::time::error::Elapsed>,
) -> io::Result<Option<String>> {
    match msg {
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for approval on the target PC",
        )),
        Ok(Err(err)) => Err(err),
        Ok(Ok(PairMsg::Approved)) => Ok(None),
        Ok(Ok(PairMsg::Rejected { reason })) => Ok(Some(reason)),
        Ok(Ok(_)) => Err(frame::invalid("unexpected pairing decision from target PC")),
    }
}

async fn serve_connect_inner<R, W>(
    reader: &mut R,
    writer: &mut W,
    network: &Arc<Network>,
    confirm_timeout: Duration,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let req_frame = tokio::time::timeout(STEP_TIMEOUT, frame::read(reader))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out reading connect request"))??
        .ok_or_else(|| frame::invalid("missing connect request"))?;
    if req_frame.tag != STDIN {
        return Err(frame::invalid("expected STDIN request for connect"));
    }
    let req_str = std::str::from_utf8(&req_frame.payload)
        .map_err(|_| frame::invalid("connect request must be UTF-8"))?;
    let parts: Vec<_> = req_str.split_whitespace().collect();
    if !(4..=5).contains(&parts.len()) {
        return Err(frame::invalid("invalid connect request arguments"));
    }

    let local_peer_name = parts[0];
    if !peers::valid_name(local_peer_name) {
        return Err(frame::invalid("invalid peer name"));
    }
    let target_id: EndpointId = parts[1]
        .parse()
        .map_err(|_| frame::invalid("invalid target EndpointId"))?;
    let self_id = network.endpoint.id();
    if target_id == self_id {
        return Err(frame::invalid("cannot pair an agent with itself"));
    }
    let auth_kind = match parts[2] {
        "invite" => AuthKind::Invite,
        "lan" => AuthKind::Lan,
        _ => return Err(frame::invalid("invalid connect auth mode")),
    };
    let token = decode_hex_16(parts[3])?;
    let direct_addr = match parts.get(4) {
        Some(raw) => Some(
            raw.parse::<SocketAddr>()
                .map_err(|_| frame::invalid("invalid direct socket address"))?,
        ),
        None => None,
    };

    peers::check_connect_target(network.dir(), local_peer_name, Some(target_id))?;

    let _connect_guard = network.pairing.register_connect(target_id);
    let mut addr = EndpointAddr::new(target_id);
    if let Some(sock_addr) = direct_addr {
        addr = addr.with_ip_addr(sock_addr);
    }

    let dial = async {
        let conn = tokio::time::timeout(STEP_TIMEOUT, network.endpoint.connect(addr, PAIR_ALPN))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out connecting to target PC (is 'herdvia listen' running there?)",
                )
            })?
            .map_err(|err| io::Error::other(format!("could not connect to target PC: {err}")))?;

        let tls_binding = export_tls_binding(&conn)?;
        let nonce_a: [u8; 32] = random_bytes();
        let commit_a = compute_commit_a(&tls_binding, self_id, target_id, &nonce_a);
        let proof = auth_proof(auth_kind, &token, &tls_binding);
        let caller_name = local_hostname(self_id);

        let (mut send, mut recv) = tokio::time::timeout(STEP_TIMEOUT, conn.open_bi())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "timed out opening pairing stream")
            })?
            .map_err(io::Error::other)?;

        tokio::time::timeout(
            STEP_TIMEOUT,
            write_msg(
                &mut send,
                &PairMsg::Hello {
                    auth_kind,
                    auth_proof: proof,
                    commit_a,
                    caller_name,
                },
            ),
        )
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "timed out sending pairing hello")
        })??;

        let challenge_msg = tokio::time::timeout(STEP_TIMEOUT, read_msg(&mut recv))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for target PC response",
                )
            })??;

        let (nonce_b, target_name) = match challenge_msg {
            PairMsg::Challenge {
                nonce_b,
                target_name,
            } => (nonce_b, target_name),
            PairMsg::Rejected { reason } => {
                conn.close(0u32.into(), b"rejected");
                let line = format!("REJECTED {}\n", log::sanitize(&reason));
                frame::write(writer, STDOUT, line.as_bytes()).await?;
                frame::write(writer, EXIT, &1_i32.to_be_bytes()).await?;
                return Ok(None);
            }
            _ => return Err(frame::invalid("unexpected pairing message from target PC")),
        };

        tokio::time::timeout(
            STEP_TIMEOUT,
            write_msg(&mut send, &PairMsg::Reveal { nonce_a }),
        )
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "timed out sending pairing reveal")
        })??;

        let sas = derive_sas(&tls_binding, self_id, target_id, &nonce_a, &nonce_b);
        let sas_line = format!(
            "SAS {target_name} {} {sas}\n",
            peers::fingerprint(target_id)
        );
        Ok(Some((conn, send, recv, sas_line)))
    };

    let dialed = tokio::select! {
        res = dial => res?,
        ipc_in = frame::read(reader) => {
            return match ipc_in {
                Ok(None) | Err(_) => Ok(()),
                Ok(Some(_)) => Err(frame::invalid("unexpected frame during connect")),
            };
        }
    };
    let Some((conn, _send, mut recv, sas_line)) = dialed else {
        return Ok(());
    };
    frame::write(writer, STDOUT, sas_line.as_bytes()).await?;

    // The LAN beacon that named this target is not authenticated, so the target's Approved alone
    // proves nothing: anyone on the LAN can send one. Nothing is saved until the person here has
    // also confirmed that both PCs show the same code. The two may arrive in either order.
    let decision = tokio::time::timeout(CLIENT_APPROVAL_WAIT, read_msg(&mut recv));
    tokio::pin!(decision);
    let mut target_approved = false;
    let rejected = 'wait: {
        let confirmation = {
            let read = frame::read(&mut *reader);
            tokio::pin!(read);
            let expiry = tokio::time::sleep(confirm_timeout);
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    msg = &mut decision, if !target_approved => match target_decision(msg)? {
                        None => target_approved = true,
                        Some(reason) => break 'wait Some(reason),
                    },
                    _ = &mut expiry => {
                        conn.close(0u32.into(), b"not confirmed");
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out waiting for the verification code to be confirmed on this PC",
                        ));
                    }
                    f = &mut read => break f,
                }
            }
        };
        match confirmation {
            Ok(Some(f))
                if f.tag == STDIN
                    && std::str::from_utf8(&f.payload).ok().map(str::trim) == Some("approve") => {}
            Ok(Some(f)) if f.tag != STDIN => {
                return Err(frame::invalid("unexpected frame during connect"));
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                conn.close(0u32.into(), b"cancelled");
                return Ok(());
            }
        }
        if target_approved {
            break 'wait None;
        }
        tokio::select! {
            msg = &mut decision => target_decision(msg)?,
            ipc_in = frame::read(reader) => {
                conn.close(0u32.into(), b"cancelled");
                return match ipc_in {
                    Ok(None) | Err(_) => Ok(()),
                    Ok(Some(_)) => Err(frame::invalid("unexpected frame during connect")),
                };
            }
        }
    };

    if let Some(reason) = rejected {
        conn.close(0u32.into(), b"rejected");
        let line = format!("REJECTED {}\n", log::sanitize(&reason));
        frame::write(writer, STDOUT, line.as_bytes()).await?;
        frame::write(writer, EXIT, &1_i32.to_be_bytes()).await?;
        return Ok(());
    }

    conn.close(0u32.into(), b"pair complete");
    if let Some(sock_addr) = direct_addr {
        network.set_addr_hint(target_id, sock_addr);
    }
    let saved = peers::grant_outbound(network.dir(), local_peer_name, target_id)?;
    network.reload()?;
    log::line(
        Kind::Other,
        &format!(
            "pair connected: {} (fingerprint {})",
            saved.name,
            peers::fingerprint(target_id)
        ),
    );
    let out = format!("APPROVED {target_id} {}\n", saved.permission.as_str());
    frame::write(writer, STDOUT, out.as_bytes()).await?;
    frame::write(writer, EXIT, &0_i32.to_be_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        frame::{self, ERROR, EXIT, STDIN, STDOUT},
        pair::codec::{AuthKind, PAIR_ALPN, PairMsg, compute_commit_a, write_msg},
        peers::{self, Permission},
    };
    use iroh::{Endpoint, EndpointAddr, SecretKey, endpoint::presets};
    use std::time::Duration;
    use tokio::task::JoinSet;

    fn spawn_accept_loop(net: Arc<Network>) -> tokio::task::AbortHandle {
        tokio::spawn(async move {
            let mut set = JoinSet::new();
            while let Some(incoming) = net.endpoint.accept().await {
                let net = net.clone();
                set.spawn(async move {
                    if let Ok(conn) = incoming.await {
                        if conn.alpn() == PAIR_ALPN {
                            serve_incoming(net, conn).await;
                        } else {
                            net.serve_connection(conn).await;
                        }
                    }
                });
            }
        })
        .abort_handle()
    }

    /// A target that sends Approved right after the Reveal, with no person approving on its side:
    /// what anyone on the LAN can do after sending a beacon with its own key.
    async fn spawn_eager_approver() -> (Endpoint, EndpointId, u16) {
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(vec![PAIR_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let port = ep
            .bound_sockets()
            .into_iter()
            .find(|a| a.is_ipv4())
            .unwrap()
            .port();
        let id = ep.id();
        let accept_ep = ep.clone();
        tokio::spawn(async move {
            let conn = accept_ep.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let _hello = crate::pair::codec::read_msg(&mut recv).await.unwrap();
            write_msg(
                &mut send,
                &PairMsg::Challenge {
                    nonce_b: [3u8; 32],
                    target_name: "attacker".into(),
                },
            )
            .await
            .unwrap();
            let _reveal = crate::pair::codec::read_msg(&mut recv).await.unwrap();
            write_msg(&mut send, &PairMsg::Approved).await.unwrap();
            let _ = send.finish();
            conn.closed().await;
        });
        (ep, id, port)
    }

    /// Starts a connect session to the eager approver and returns once the SAS line arrived.
    async fn connect_to_eager_approver(
        net_a: &Arc<Network>,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<io::Result<()>>,
        Endpoint,
        EndpointId,
    ) {
        let (target_ep, target_id, port) = spawn_eager_approver().await;
        let (mut cli_a, srv_a) = tokio::io::duplex(4096);
        let connect_task = tokio::spawn(serve_connect(srv_a, net_a.clone()));
        let req = format!(
            "home {target_id} lan {} 127.0.0.1:{port}",
            HEXLOWER.encode(&[5u8; 16])
        );
        frame::write(&mut cli_a, STDIN, req.as_bytes())
            .await
            .unwrap();
        let sas = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert_eq!(sas.tag, STDOUT);
        assert!(String::from_utf8(sas.payload).unwrap().starts_with("SAS "));
        (cli_a, connect_task, target_ep, target_id)
    }

    #[tokio::test]
    async fn eager_target_approval_saves_nothing_until_the_caller_confirms_the_code() {
        let dir_a = tempfile::tempdir().unwrap();
        let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
        let (mut cli_a, connect_task, target_ep, target_id) =
            connect_to_eager_approver(&net_a).await;

        // The target's Approved has arrived by now; without the caller's confirmation nothing
        // may be saved or reported as approved.
        let early = tokio::time::timeout(Duration::from_millis(700), frame::read(&mut cli_a)).await;
        assert!(
            early.is_err(),
            "agent answered before confirmation: {early:?}"
        );
        assert!(peers::load(dir_a.path()).unwrap().is_empty());

        frame::write(&mut cli_a, STDIN, b"approve").await.unwrap();
        let done = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert_eq!(done.tag, STDOUT);
        assert_eq!(
            String::from_utf8(done.payload).unwrap(),
            format!("APPROVED {target_id} out\n")
        );
        let exit = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert_eq!(exit.tag, EXIT);
        assert_eq!(i32::from_be_bytes(exit.payload.try_into().unwrap()), 0);
        connect_task.await.unwrap().unwrap();
        assert_eq!(
            peers::load(dir_a.path()).unwrap(),
            vec![peers::Peer {
                name: "home".into(),
                id: target_id,
                permission: Permission::Out
            }]
        );

        target_ep.close().await;
        net_a.endpoint.close().await;
    }

    #[tokio::test]
    async fn eager_target_approval_saves_nothing_when_the_caller_declines_or_disconnects() {
        for declined in [true, false] {
            let dir_a = tempfile::tempdir().unwrap();
            let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
            let (mut cli_a, connect_task, target_ep, _) = connect_to_eager_approver(&net_a).await;

            // Let the target's Approved arrive before the caller answers.
            tokio::time::sleep(Duration::from_millis(500)).await;
            if declined {
                let _ = frame::write(&mut cli_a, STDIN, b"reject").await;
            } else {
                drop(cli_a);
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), connect_task)
                .await
                .expect("connect session should end");
            assert!(
                peers::load(dir_a.path()).unwrap().is_empty(),
                "declined={declined}"
            );

            target_ep.close().await;
            net_a.endpoint.close().await;
        }
    }

    #[tokio::test]
    async fn eager_target_approval_saves_nothing_when_the_caller_never_answers() {
        let dir_a = tempfile::tempdir().unwrap();
        let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
        let (target_ep, target_id, port) = spawn_eager_approver().await;
        let (mut cli_a, srv_a) = tokio::io::duplex(4096);
        // A CLI that never sends a confirmation, as one older than the agent does.
        let connect_task = tokio::spawn(serve_connect_with_timeout(
            srv_a,
            net_a.clone(),
            Duration::from_millis(600),
        ));
        let req = format!(
            "home {target_id} lan {} 127.0.0.1:{port}",
            HEXLOWER.encode(&[5u8; 16])
        );
        frame::write(&mut cli_a, STDIN, req.as_bytes())
            .await
            .unwrap();
        let sas = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert!(String::from_utf8(sas.payload).unwrap().starts_with("SAS "));

        let outcome = tokio::time::timeout(Duration::from_secs(5), frame::read(&mut cli_a))
            .await
            .expect("connect session should time out")
            .unwrap()
            .unwrap();
        assert_eq!(outcome.tag, ERROR);
        assert!(
            String::from_utf8(outcome.payload)
                .unwrap()
                .contains("timed out waiting for the verification code to be confirmed"),
        );
        assert!(connect_task.await.unwrap().is_err());
        assert!(peers::load(dir_a.path()).unwrap().is_empty());

        target_ep.close().await;
        net_a.endpoint.close().await;
    }

    #[tokio::test]
    async fn pairing_over_lan_token_computes_matching_sas_grants_permissions_and_never_touches_pending()
     {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
        let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
        let id_a = net_a.endpoint.id();
        let id_b = net_b.endpoint.id();
        let accept_b = spawn_accept_loop(net_b.clone());

        let (mut cli_b, srv_b) = tokio::io::duplex(4096);
        let listen_task = tokio::spawn(serve_listen(srv_b, net_b.clone()));

        frame::write(&mut cli_b, STDIN, b"").await.unwrap();
        let ready = frame::read(&mut cli_b).await.unwrap().unwrap();
        assert_eq!(ready.tag, STDOUT);
        let ready_text = String::from_utf8(ready.payload).unwrap();
        let parts: Vec<_> = ready_text.split_whitespace().collect();
        assert_eq!(parts[0], "READY");
        let quic_port: u16 = parts[2].parse().unwrap();
        let listen_id_hex = parts[3];

        let (mut cli_a, srv_a) = tokio::io::duplex(4096);
        let connect_task = tokio::spawn(serve_connect(srv_a, net_a.clone()));
        let req = format!("home {id_b} lan {listen_id_hex} 127.0.0.1:{quic_port}");
        frame::write(&mut cli_a, STDIN, req.as_bytes())
            .await
            .unwrap();

        let sas_frame = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert_eq!(sas_frame.tag, STDOUT);
        let sas_line = String::from_utf8(sas_frame.payload).unwrap();
        let sas_a = sas_line.split_whitespace().nth(3).unwrap().to_owned();

        let prompt_frame = frame::read(&mut cli_b).await.unwrap().unwrap();
        assert_eq!(prompt_frame.tag, STDOUT);
        let prompt_line = String::from_utf8(prompt_frame.payload).unwrap();
        let sas_b = prompt_line.split_whitespace().nth(4).unwrap().to_owned();
        assert_eq!(sas_a, sas_b);

        frame::write(&mut cli_a, STDIN, b"approve").await.unwrap();
        frame::write(&mut cli_b, STDIN, b"approve").await.unwrap();

        let b_done = frame::read(&mut cli_b).await.unwrap().unwrap();
        assert_eq!(b_done.tag, STDOUT);
        assert!(
            String::from_utf8(b_done.payload)
                .unwrap()
                .starts_with("APPROVED ")
        );
        let b_exit = frame::read(&mut cli_b).await.unwrap().unwrap();
        assert_eq!(b_exit.tag, EXIT);
        assert_eq!(i32::from_be_bytes(b_exit.payload.try_into().unwrap()), 0);

        let a_done = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert_eq!(a_done.tag, STDOUT);
        assert!(
            String::from_utf8(a_done.payload)
                .unwrap()
                .starts_with("APPROVED ")
        );
        let a_exit = frame::read(&mut cli_a).await.unwrap().unwrap();
        assert_eq!(a_exit.tag, EXIT);
        assert_eq!(i32::from_be_bytes(a_exit.payload.try_into().unwrap()), 0);

        listen_task.await.unwrap().unwrap();
        connect_task.await.unwrap().unwrap();

        let peers_a = peers::load(dir_a.path()).unwrap();
        assert_eq!(
            peers_a,
            vec![peers::Peer {
                name: "home".into(),
                id: id_b,
                permission: Permission::Out
            }]
        );
        let peers_b = peers::load(dir_b.path()).unwrap();
        assert_eq!(peers_b.len(), 1);
        assert_eq!(peers_b[0].id, id_a);
        assert_eq!(peers_b[0].permission, Permission::In);
        assert_eq!(net_b.pending(), "fingerprint id attempts last-seen\n");

        accept_b.abort();
        net_a.endpoint.close().await;
        net_b.endpoint.close().await;
    }

    #[tokio::test]
    async fn five_wrong_proofs_abort_listen_session_and_unpaired_pair_alpn_is_rejected() {
        let dir_b = tempfile::tempdir().unwrap();
        let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
        let id_b = net_b.endpoint.id();
        let port_b = net_b.quic_port();
        let accept_b = spawn_accept_loop(net_b.clone());

        let attacker = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .bind()
            .await
            .unwrap();
        let addr_b = EndpointAddr::new(id_b).with_ip_addr(([127, 0, 0, 1], port_b).into());

        let uninvited = attacker.connect(addr_b.clone(), PAIR_ALPN).await;
        if let Ok(conn) = uninvited {
            assert!(
                tokio::time::timeout(Duration::from_secs(2), conn.closed())
                    .await
                    .is_ok()
            );
        }
        assert_eq!(net_b.pending(), "fingerprint id attempts last-seen\n");

        let (mut cli_b, srv_b) = tokio::io::duplex(4096);
        let listen_task = tokio::spawn(serve_listen(srv_b, net_b.clone()));
        frame::write(&mut cli_b, STDIN, b"").await.unwrap();
        let ready = frame::read(&mut cli_b).await.unwrap().unwrap();
        assert_eq!(ready.tag, STDOUT);

        for _ in 0..5 {
            let att_ep = Endpoint::builder(presets::Minimal)
                .secret_key(SecretKey::generate())
                .bind()
                .await
                .unwrap();
            let conn = att_ep.connect(addr_b.clone(), PAIR_ALPN).await.unwrap();
            let mut tls_binding = [0u8; 32];
            conn.export_keying_material(&mut tls_binding, b"herdvia-pair-v1", &[])
                .unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let commit_a = compute_commit_a(&tls_binding, att_ep.id(), id_b, &[7u8; 32]);
            write_msg(
                &mut send,
                &PairMsg::Hello {
                    auth_kind: AuthKind::Invite,
                    auth_proof: [0xFF; 32],
                    commit_a,
                    caller_name: "attacker".into(),
                },
            )
            .await
            .unwrap();
            let _ = crate::pair::codec::read_msg(&mut recv).await;
            att_ep.close().await;
        }

        let aborted = tokio::time::timeout(Duration::from_secs(3), frame::read(&mut cli_b))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(aborted.tag, ERROR);
        assert!(
            String::from_utf8(aborted.payload)
                .unwrap()
                .contains("too many invalid pairing attempts")
        );
        let _ = listen_task.await;
        assert_eq!(net_b.pending(), "fingerprint id attempts last-seen\n");

        attacker.close().await;
        accept_b.abort();
        net_b.endpoint.close().await;
    }

    #[tokio::test]
    async fn dropping_local_ipc_cancels_active_listen_immediately() {
        let dir_b = tempfile::tempdir().unwrap();
        let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
        let id_b = net_b.endpoint.id();
        let port_b = net_b.quic_port();
        let accept_b = spawn_accept_loop(net_b.clone());

        let (mut cli_b, srv_b) = tokio::io::duplex(4096);
        let listen_task = tokio::spawn(serve_listen(srv_b, net_b.clone()));
        frame::write(&mut cli_b, STDIN, b"").await.unwrap();
        let ready = frame::read(&mut cli_b).await.unwrap().unwrap();
        assert_eq!(ready.tag, STDOUT);

        drop(cli_b);
        let _ = tokio::time::timeout(Duration::from_secs(2), listen_task)
            .await
            .unwrap();

        let probe = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .bind()
            .await
            .unwrap();
        let addr_b = EndpointAddr::new(id_b).with_ip_addr(([127, 0, 0, 1], port_b).into());
        if let Ok(conn) = probe.connect(addr_b, PAIR_ALPN).await {
            assert!(
                tokio::time::timeout(Duration::from_secs(2), conn.closed())
                    .await
                    .is_ok()
            );
        }

        probe.close().await;
        accept_b.abort();
        net_b.endpoint.close().await;
    }

    #[tokio::test]
    async fn repeated_aborts_after_challenge_abort_listen_session() {
        let dir_b = tempfile::tempdir().unwrap();
        let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
        let id_b = net_b.endpoint.id();
        let port_b = net_b.quic_port();
        let accept_b = spawn_accept_loop(net_b.clone());
        let addr_b = EndpointAddr::new(id_b).with_ip_addr(([127, 0, 0, 1], port_b).into());

        let (mut cli_b, srv_b) = tokio::io::duplex(4096);
        let listen_task = tokio::spawn(serve_listen(srv_b, net_b.clone()));
        frame::write(&mut cli_b, STDIN, b"").await.unwrap();
        let ready = frame::read(&mut cli_b).await.unwrap().unwrap();
        let ready_text = String::from_utf8(ready.payload).unwrap();
        let listen_id = decode_hex_16(ready_text.split_whitespace().nth(3).unwrap()).unwrap();

        for _ in 0..5 {
            let att_ep = Endpoint::builder(presets::Minimal)
                .secret_key(SecretKey::generate())
                .bind()
                .await
                .unwrap();
            let conn = att_ep.connect(addr_b.clone(), PAIR_ALPN).await.unwrap();
            let tls_binding = export_tls_binding(&conn).unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let nonce_a = [9u8; 32];
            let commit_a = compute_commit_a(&tls_binding, att_ep.id(), id_b, &nonce_a);
            let proof = auth_proof(AuthKind::Lan, &listen_id, &tls_binding);
            write_msg(
                &mut send,
                &PairMsg::Hello {
                    auth_kind: AuthKind::Lan,
                    auth_proof: proof,
                    commit_a,
                    caller_name: "attacker".into(),
                },
            )
            .await
            .unwrap();
            let msg = crate::pair::codec::read_msg(&mut recv).await.unwrap();
            assert!(matches!(msg, PairMsg::Challenge { .. }));
            conn.close(0u32.into(), b"grind");
            att_ep.close().await;
        }

        let aborted = tokio::time::timeout(Duration::from_secs(3), frame::read(&mut cli_b))
            .await
            .expect("listen should abort after 5 post-challenge aborts")
            .unwrap()
            .unwrap();
        assert_eq!(aborted.tag, ERROR);
        assert!(
            String::from_utf8(aborted.payload)
                .unwrap()
                .contains("too many invalid pairing attempts")
        );
        let _ = listen_task.await;

        accept_b.abort();
        net_b.endpoint.close().await;
    }

    #[tokio::test]
    async fn approval_after_session_expiry_or_concurrent_abort_never_grants_inbound() {
        // 1. Concurrent abort (5 wrong proofs) while PROMPT is waiting on user input.
        {
            let dir_a = tempfile::tempdir().unwrap();
            let dir_b = tempfile::tempdir().unwrap();
            let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
            let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
            let id_b = net_b.endpoint.id();
            let port_b = net_b.quic_port();
            let accept_b = spawn_accept_loop(net_b.clone());
            let addr_b = EndpointAddr::new(id_b).with_ip_addr(([127, 0, 0, 1], port_b).into());

            let (mut cli_b, srv_b) = tokio::io::duplex(4096);
            let listen_task = tokio::spawn(serve_listen(srv_b, net_b.clone()));
            frame::write(&mut cli_b, STDIN, b"").await.unwrap();
            let ready = frame::read(&mut cli_b).await.unwrap().unwrap();
            let ready_text = String::from_utf8(ready.payload).unwrap();
            let listen_id_hex = ready_text.split_whitespace().nth(3).unwrap().to_owned();

            let (mut cli_a, srv_a) = tokio::io::duplex(4096);
            let connect_task = tokio::spawn(serve_connect(srv_a, net_a.clone()));
            let req = format!("home {id_b} lan {listen_id_hex} 127.0.0.1:{port_b}");
            frame::write(&mut cli_a, STDIN, req.as_bytes())
                .await
                .unwrap();

            let prompt_frame = frame::read(&mut cli_b).await.unwrap().unwrap();
            assert_eq!(prompt_frame.tag, STDOUT);
            assert!(
                String::from_utf8(prompt_frame.payload)
                    .unwrap()
                    .starts_with("PROMPT ")
            );

            // While PROMPT is waiting, 5 wrong-proof connections exhaust the session budget.
            for _ in 0..5 {
                let att_ep = Endpoint::builder(presets::Minimal)
                    .secret_key(SecretKey::generate())
                    .bind()
                    .await
                    .unwrap();
                let conn = att_ep.connect(addr_b.clone(), PAIR_ALPN).await.unwrap();
                let tls_binding = export_tls_binding(&conn).unwrap();
                let (mut send, mut recv) = conn.open_bi().await.unwrap();
                let commit_a = compute_commit_a(&tls_binding, att_ep.id(), id_b, &[7u8; 32]);
                write_msg(
                    &mut send,
                    &PairMsg::Hello {
                        auth_kind: AuthKind::Invite,
                        auth_proof: [0xEE; 32],
                        commit_a,
                        caller_name: "attacker".into(),
                    },
                )
                .await
                .unwrap();
                let _ = crate::pair::codec::read_msg(&mut recv).await;
                att_ep.close().await;
            }

            // Even if the user types "approve" right after, the aborted session must NOT save peers.
            let _ = frame::write(&mut cli_b, STDIN, b"approve").await;
            let outcome = tokio::time::timeout(Duration::from_secs(2), frame::read(&mut cli_b))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(outcome.tag, ERROR);
            assert!(peers::load(dir_b.path()).unwrap().is_empty());

            let _ = listen_task.await;
            drop(cli_a);
            let _ = connect_task.await;
            accept_b.abort();
            net_a.endpoint.close().await;
            net_b.endpoint.close().await;
        }

        // 2. Session TTL expiry while PROMPT is waiting on user input.
        {
            let dir_a = tempfile::tempdir().unwrap();
            let dir_b = tempfile::tempdir().unwrap();
            let net_a = Network::bind(dir_a.path().to_owned()).await.unwrap();
            let net_b = Network::bind(dir_b.path().to_owned()).await.unwrap();
            let id_b = net_b.endpoint.id();
            let port_b = net_b.quic_port();
            let accept_b = spawn_accept_loop(net_b.clone());

            let (mut cli_b, srv_b) = tokio::io::duplex(4096);
            let listen_task = tokio::spawn(serve_listen_with_ttl(
                srv_b,
                net_b.clone(),
                Duration::from_millis(600),
            ));
            frame::write(&mut cli_b, STDIN, b"").await.unwrap();
            let ready = frame::read(&mut cli_b).await.unwrap().unwrap();
            let ready_text = String::from_utf8(ready.payload).unwrap();
            let listen_id_hex = ready_text.split_whitespace().nth(3).unwrap().to_owned();

            let (mut cli_a, srv_a) = tokio::io::duplex(4096);
            let connect_task = tokio::spawn(serve_connect(srv_a, net_a.clone()));
            let req = format!("home {id_b} lan {listen_id_hex} 127.0.0.1:{port_b}");
            frame::write(&mut cli_a, STDIN, req.as_bytes())
                .await
                .unwrap();

            let prompt_frame = frame::read(&mut cli_b).await.unwrap().unwrap();
            assert_eq!(prompt_frame.tag, STDOUT);

            // Wait past the 600ms session TTL before typing "approve".
            tokio::time::sleep(Duration::from_millis(750)).await;
            let _ = frame::write(&mut cli_b, STDIN, b"approve").await;

            let outcome = tokio::time::timeout(Duration::from_secs(2), frame::read(&mut cli_b))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(outcome.tag, STDOUT);
            assert_eq!(String::from_utf8(outcome.payload).unwrap(), "TIMEOUT\n");
            assert!(peers::load(dir_b.path()).unwrap().is_empty());

            let _ = listen_task.await;
            drop(cli_a);
            let _ = connect_task.await;
            accept_b.abort();
            net_a.endpoint.close().await;
            net_b.endpoint.close().await;
        }
    }
}
