use crate::{pair::codec::random_bytes, peers};
use iroh::EndpointId;
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::watch,
    time::{Instant, MissedTickBehavior},
};

pub const DEFAULT_PORT: u16 = 48721;
pub const MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 86);

const BEACON_MAGIC: &[u8; 5] = b"HDVB1";
const QUERY_MAGIC: &[u8; 5] = b"HDVD1";
const BEACON_FIXED_LEN: usize = 5 + 16 + 32 + 2 + 1;
const QUERY_LEN: usize = 5 + 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeaconInfo {
    pub listen_id: [u8; 16],
    pub endpoint_id: EndpointId,
    pub quic_port: u16,
    pub hostname: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub info: BeaconInfo,
    pub source_ip: IpAddr,
}

impl DiscoveredPeer {
    pub fn direct_addr(&self) -> Option<SocketAddr> {
        (self.info.quic_port != 0).then(|| SocketAddr::new(self.source_ip, self.info.quic_port))
    }
}

pub fn encode_beacon(info: &BeaconInfo) -> Vec<u8> {
    let host_bytes = info.hostname.as_bytes();
    let mut out = Vec::with_capacity(BEACON_FIXED_LEN + host_bytes.len());
    out.extend_from_slice(BEACON_MAGIC);
    out.extend_from_slice(&info.listen_id);
    out.extend_from_slice(info.endpoint_id.as_bytes());
    out.extend_from_slice(&info.quic_port.to_be_bytes());
    out.push(host_bytes.len() as u8);
    out.extend_from_slice(host_bytes);
    out
}

pub fn decode_beacon(buf: &[u8]) -> Option<BeaconInfo> {
    if buf.len() < BEACON_FIXED_LEN || &buf[..5] != BEACON_MAGIC {
        return None;
    }
    let listen_id: [u8; 16] = buf[5..21].try_into().ok()?;
    let id_bytes: [u8; 32] = buf[21..53].try_into().ok()?;
    let endpoint_id = EndpointId::from_bytes(&id_bytes).ok()?;
    let quic_port = u16::from_be_bytes([buf[53], buf[54]]);
    let host_len = buf[55] as usize;
    if buf.len() != BEACON_FIXED_LEN + host_len {
        return None;
    }
    let hostname = std::str::from_utf8(&buf[BEACON_FIXED_LEN..]).ok()?;
    if !peers::valid_name(hostname) {
        return None;
    }
    Some(BeaconInfo {
        listen_id,
        endpoint_id,
        quic_port,
        hostname: hostname.to_owned(),
    })
}

pub fn encode_query(nonce: &[u8; 16]) -> [u8; QUERY_LEN] {
    let mut out = [0u8; QUERY_LEN];
    out[..5].copy_from_slice(QUERY_MAGIC);
    out[5..].copy_from_slice(nonce);
    out
}

pub fn is_query(buf: &[u8]) -> bool {
    buf.len() == QUERY_LEN && &buf[..5] == QUERY_MAGIC
}

pub fn default_targets(port: u16) -> Vec<SocketAddr> {
    vec![
        SocketAddr::new(IpAddr::V4(MULTICAST_V4), port),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
    ]
}

#[cfg(unix)]
fn set_reuse_port(sock: &Socket) {
    use std::os::fd::AsRawFd;
    let one: libc::c_int = 1;
    // Best-effort SO_REUSEPORT so multiple local processes can share the discovery port.
    unsafe {
        let _ = libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            (&raw const one).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

pub fn bind_discovery_socket(port: u16) -> io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = sock.set_reuse_address(true);
    #[cfg(unix)]
    set_reuse_port(&sock);
    let _ = sock.set_broadcast(true);
    let _ = sock.set_multicast_loop_v4(true);
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port).into())?;
    let _ = sock.join_multicast_v4(&MULTICAST_V4, &Ipv4Addr::UNSPECIFIED);
    UdpSocket::from_std(sock.into())
}

fn bind_ephemeral_socket() -> io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = sock.set_broadcast(true);
    let _ = sock.set_multicast_loop_v4(true);
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).into())?;
    UdpSocket::from_std(sock.into())
}

pub async fn advertise_on(
    socket: UdpSocket,
    targets: Vec<SocketAddr>,
    info: BeaconInfo,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let beacon = encode_beacon(&info);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut recv_buf = [0u8; 256];

    loop {
        if *stop.borrow() {
            return Ok(());
        }
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            _ = tick.tick() => {
                for target in &targets {
                    let _ = socket.send_to(&beacon, *target).await;
                }
            }
            recvd = socket.recv_from(&mut recv_buf) => {
                if let Ok((n, peer_addr)) = recvd
                    && is_query(&recv_buf[..n])
                {
                    let _ = socket.send_to(&beacon, peer_addr).await;
                }
            }
        }
    }
}

pub async fn scan_on(
    listen_socket: Option<UdpSocket>,
    targets: Vec<SocketAddr>,
    self_id: EndpointId,
    duration: Duration,
) -> io::Result<Vec<DiscoveredPeer>> {
    let query_sock = bind_ephemeral_socket()?;
    let start = Instant::now();
    let deadline = start + duration;
    let send_offsets = [
        Duration::ZERO,
        Duration::from_millis(400),
        Duration::from_millis(900),
    ];
    let mut next_send = 0usize;
    let mut seen_ids = HashSet::new();
    let mut peers = Vec::new();
    let mut q_buf = [0u8; 256];
    let mut l_buf = [0u8; 256];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        while next_send < send_offsets.len() && now >= start + send_offsets[next_send] {
            let pkt = encode_query(&random_bytes());
            for target in &targets {
                let _ = query_sock.send_to(&pkt, *target).await;
            }
            next_send += 1;
        }

        let next_wake = if next_send < send_offsets.len() {
            (start + send_offsets[next_send]).min(deadline)
        } else {
            deadline
        };

        let mut record = |buf: &[u8], addr: SocketAddr| {
            if let Some(info) = decode_beacon(buf)
                && info.endpoint_id != self_id
                && seen_ids.insert(info.endpoint_id)
            {
                peers.push(DiscoveredPeer {
                    info,
                    source_ip: addr.ip(),
                });
            }
        };

        match &listen_socket {
            Some(lsock) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(next_wake) => {}
                    res = query_sock.recv_from(&mut q_buf) => {
                        if let Ok((n, addr)) = res {
                            record(&q_buf[..n], addr);
                        }
                    }
                    res = lsock.recv_from(&mut l_buf) => {
                        if let Ok((n, addr)) = res {
                            record(&l_buf[..n], addr);
                        }
                    }
                }
            }
            None => {
                tokio::select! {
                    _ = tokio::time::sleep_until(next_wake) => {}
                    res = query_sock.recv_from(&mut q_buf) => {
                        if let Ok((n, addr)) = res {
                            record(&q_buf[..n], addr);
                        }
                    }
                }
            }
        }
    }

    Ok(peers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn beacon_and_query_packets_roundtrip_and_ignore_garbage() {
        let id = SecretKey::from_bytes(&[9u8; 32]).public();
        let info = BeaconInfo {
            listen_id: [0xAB; 16],
            endpoint_id: id,
            quic_port: 54321,
            hostname: "home-mac".into(),
        };
        let pkt = encode_beacon(&info);
        assert_eq!(&pkt[..5], b"HDVB1");
        assert_eq!(decode_beacon(&pkt), Some(info.clone()));

        assert_eq!(decode_beacon(&pkt[..pkt.len() - 1]), None);
        let mut trailing = pkt.clone();
        trailing.push(0);
        assert_eq!(decode_beacon(&trailing), None);

        let mut wrong_magic = pkt.clone();
        wrong_magic[0] = b'X';
        assert_eq!(decode_beacon(&wrong_magic), None);

        let mut bad_host = Vec::from(&pkt[..5 + 16 + 32 + 2]);
        bad_host.push(8);
        bad_host.extend_from_slice(b"bad/name");
        assert_eq!(decode_beacon(&bad_host), None);

        let q = encode_query(&[7u8; 16]);
        assert_eq!(&q[..5], b"HDVD1");
        assert!(is_query(&q));
        assert!(!is_query(&q[..20]));
        assert!(!is_query(&pkt));
    }

    #[tokio::test]
    async fn scan_discovers_listener_over_loopback_and_filters_self_id() {
        let self_id = SecretKey::from_bytes(&[1u8; 32]).public();
        let id_b = SecretKey::from_bytes(&[2u8; 32]).public();

        let sock_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let sock_self = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_self = sock_self.local_addr().unwrap();

        let (stop_tx, stop_rx) = watch::channel(false);
        let info_b = BeaconInfo {
            listen_id: [0x11; 16],
            endpoint_id: id_b,
            quic_port: 12345,
            hostname: "desk-b".into(),
        };
        let info_self = BeaconInfo {
            listen_id: [0x22; 16],
            endpoint_id: self_id,
            quic_port: 23456,
            hostname: "self-node".into(),
        };

        let adv_b = tokio::spawn(advertise_on(
            sock_b,
            Vec::new(),
            info_b.clone(),
            stop_rx.clone(),
        ));
        let adv_self = tokio::spawn(advertise_on(sock_self, Vec::new(), info_self, stop_rx));

        let started = std::time::Instant::now();
        let found = scan_on(
            None,
            vec![addr_b, addr_self],
            self_id,
            Duration::from_millis(250),
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();

        let _ = stop_tx.send(true);
        let _ = adv_b.await;
        let _ = adv_self.await;

        assert!(elapsed < Duration::from_millis(350), "took {elapsed:?}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].info, info_b);
        assert_eq!(
            found[0].direct_addr(),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345))
        );
    }
}
