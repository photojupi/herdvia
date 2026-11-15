use crate::{frame, peers};
use data_encoding::BASE32_NOPAD;
use iroh::{EndpointId, SecretKey};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PAIR_ALPN: &[u8] = b"herdvia/pair/1";

const INVITE_PREFIX: &str = "hvi1-";
const INVITE_RAW_LEN: usize = 48;
const INVITE_STR_LEN: usize = 82;
const PAIR_VERSION: u8 = 1;
const MAX_PAIR_FRAME: usize = 256;

const TAG_HELLO: u8 = 1;
const TAG_CHALLENGE: u8 = 2;
const TAG_REVEAL: u8 = 3;
const TAG_APPROVED: u8 = 4;
const TAG_REJECTED: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Invite = 1,
    Lan = 2,
}

impl AuthKind {
    fn from_byte(b: u8) -> io::Result<Self> {
        match b {
            1 => Ok(Self::Invite),
            2 => Ok(Self::Lan),
            _ => Err(frame::invalid("invalid pairing auth kind")),
        }
    }

    fn context(self) -> &'static str {
        match self {
            Self::Invite => "herdvia-pair-invite-v1",
            Self::Lan => "herdvia-pair-lan-v1",
        }
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let seed = SecretKey::generate().to_bytes();
    let mut out = [0u8; N];
    if N <= 32 {
        out.copy_from_slice(&seed[..N]);
    } else {
        let mut h = blake3::Hasher::new_derive_key("herdvia-random-expand-v1");
        h.update(&seed);
        h.finalize_xof().fill(&mut out);
    }
    out
}

pub fn encode_invite(target: EndpointId, secret: &[u8; 16]) -> String {
    let mut raw = [0u8; INVITE_RAW_LEN];
    raw[..32].copy_from_slice(target.as_bytes());
    raw[32..].copy_from_slice(secret);
    format!(
        "{INVITE_PREFIX}{}",
        BASE32_NOPAD.encode(&raw).to_ascii_lowercase()
    )
}

pub fn decode_invite(input: &str) -> io::Result<(EndpointId, [u8; 16])> {
    let trimmed = input.trim();
    if trimmed.len() != INVITE_STR_LEN
        || !trimmed
            .get(..INVITE_PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(INVITE_PREFIX))
    {
        return Err(frame::invalid(
            "invalid invite token: expected an hvi1-... token",
        ));
    }
    let upper = trimmed[INVITE_PREFIX.len()..].to_ascii_uppercase();
    let decoded = BASE32_NOPAD
        .decode(upper.as_bytes())
        .map_err(|_| frame::invalid("invalid invite token: malformed base32 payload"))?;
    if decoded.len() != INVITE_RAW_LEN {
        return Err(frame::invalid("invalid invite token length"));
    }
    let id_bytes: [u8; 32] = decoded[..32].try_into().expect("slice is 32 bytes");
    let id = EndpointId::from_bytes(&id_bytes)
        .map_err(|_| frame::invalid("invalid invite token: invalid EndpointId"))?;
    let secret: [u8; 16] = decoded[32..48].try_into().expect("slice is 16 bytes");
    Ok((id, secret))
}

pub fn auth_proof(kind: AuthKind, token: &[u8; 16], tls_binding: &[u8; 32]) -> [u8; 32] {
    let key = blake3::derive_key(kind.context(), token);
    *blake3::keyed_hash(&key, tls_binding).as_bytes()
}

pub fn verify_auth_proof(
    kind: AuthKind,
    token: &[u8; 16],
    tls_binding: &[u8; 32],
    proof: &[u8; 32],
) -> bool {
    blake3::Hash::from_bytes(auth_proof(kind, token, tls_binding))
        == blake3::Hash::from_bytes(*proof)
}

pub fn compute_commit_a(
    tls_binding: &[u8; 32],
    id_a: EndpointId,
    id_b: EndpointId,
    nonce_a: &[u8; 32],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"herdvia-pair-commit-a-v1");
    h.update(tls_binding);
    h.update(id_a.as_bytes());
    h.update(id_b.as_bytes());
    h.update(nonce_a);
    *h.finalize().as_bytes()
}

pub fn verify_commit_a(
    tls_binding: &[u8; 32],
    id_a: EndpointId,
    id_b: EndpointId,
    nonce_a: &[u8; 32],
    expected: &[u8; 32],
) -> bool {
    blake3::Hash::from_bytes(compute_commit_a(tls_binding, id_a, id_b, nonce_a))
        == blake3::Hash::from_bytes(*expected)
}

pub fn derive_sas(
    tls_binding: &[u8; 32],
    id_a: EndpointId,
    id_b: EndpointId,
    nonce_a: &[u8; 32],
    nonce_b: &[u8; 32],
) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"herdvia-pair-sas-v1");
    h.update(tls_binding);
    h.update(id_a.as_bytes());
    h.update(id_b.as_bytes());
    h.update(nonce_a);
    h.update(nonce_b);
    let mut reader = h.finalize_xof();
    loop {
        let mut buf = [0u8; 4];
        reader.fill(&mut buf);
        let v = u32::from_le_bytes(buf);
        if v < 4_294_000_000 {
            let code = v % 1_000_000;
            return format!("{:03}-{:03}", code / 1000, code % 1000);
        }
    }
}

pub fn sanitize_hostname(raw: &str, fallback: EndpointId) -> String {
    let trimmed = raw.trim().trim_end_matches('.');
    let bytes = trimmed.as_bytes();
    let stripped = if bytes.len() >= 6 && bytes[bytes.len() - 6..].eq_ignore_ascii_case(b".local") {
        &trimmed[..bytes.len() - 6]
    } else {
        trimmed
    };

    let mut out = String::new();
    let mut need_dash = false;
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            if need_dash && !out.is_empty() && !out.ends_with(['-', '.', '_']) {
                if out.len() >= 32 {
                    break;
                }
                out.push('-');
            }
            need_dash = false;
            if out.len() >= 32 {
                break;
            }
            out.push(ch);
        } else if !out.is_empty() {
            need_dash = true;
        }
    }
    let cleaned = out.trim_matches(['-', '.', '_']);
    if cleaned.is_empty() || !peers::valid_name(cleaned) {
        format!("peer-{}", &fallback.to_string()[..8])
    } else {
        cleaned.to_owned()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairMsg {
    Hello {
        auth_kind: AuthKind,
        auth_proof: [u8; 32],
        commit_a: [u8; 32],
        caller_name: String,
    },
    Challenge {
        nonce_b: [u8; 32],
        target_name: String,
    },
    Reveal {
        nonce_a: [u8; 32],
    },
    Approved,
    Rejected {
        reason: String,
    },
}

pub async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut W, msg: &PairMsg) -> io::Result<()> {
    let mut payload = Vec::with_capacity(128);
    let tag = match msg {
        PairMsg::Hello {
            auth_kind,
            auth_proof,
            commit_a,
            caller_name,
        } => {
            if !peers::valid_name(caller_name) {
                return Err(frame::invalid("invalid caller name"));
            }
            payload.push(PAIR_VERSION);
            payload.push(*auth_kind as u8);
            payload.extend_from_slice(auth_proof);
            payload.extend_from_slice(commit_a);
            payload.extend_from_slice(caller_name.as_bytes());
            TAG_HELLO
        }
        PairMsg::Challenge {
            nonce_b,
            target_name,
        } => {
            if !peers::valid_name(target_name) {
                return Err(frame::invalid("invalid target name"));
            }
            payload.extend_from_slice(nonce_b);
            payload.extend_from_slice(target_name.as_bytes());
            TAG_CHALLENGE
        }
        PairMsg::Reveal { nonce_a } => {
            payload.extend_from_slice(nonce_a);
            TAG_REVEAL
        }
        PairMsg::Approved => TAG_APPROVED,
        PairMsg::Rejected { reason } => {
            let bytes = reason.as_bytes();
            let end = bytes.len().min(MAX_PAIR_FRAME);
            let valid_end = (0..=end)
                .rev()
                .find(|&i| reason.is_char_boundary(i))
                .unwrap_or(0);
            payload.extend_from_slice(&bytes[..valid_end]);
            TAG_REJECTED
        }
    };
    if payload.len() > MAX_PAIR_FRAME {
        return Err(frame::invalid("pairing frame exceeds maximum size"));
    }
    let mut header = [0u8; 3];
    header[0] = tag;
    header[1..3].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

pub async fn read_msg<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<PairMsg> {
    let mut header = [0u8; 3];
    reader.read_exact(&mut header).await?;
    let tag = header[0];
    let len = u16::from_be_bytes([header[1], header[2]]) as usize;
    if len > MAX_PAIR_FRAME
        || !matches!(
            tag,
            TAG_HELLO | TAG_CHALLENGE | TAG_REVEAL | TAG_APPROVED | TAG_REJECTED
        )
    {
        return Err(frame::invalid("invalid pairing frame header"));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    match tag {
        TAG_HELLO => {
            if payload.len() < 2 + 32 + 32 {
                return Err(frame::invalid("truncated PAIR_HELLO"));
            }
            if payload[0] != PAIR_VERSION {
                return Err(frame::invalid("unsupported pairing protocol version"));
            }
            let auth_kind = AuthKind::from_byte(payload[1])?;
            let auth_proof: [u8; 32] = payload[2..34].try_into().unwrap();
            let commit_a: [u8; 32] = payload[34..66].try_into().unwrap();
            let caller_name = std::str::from_utf8(&payload[66..])
                .map_err(|_| frame::invalid("invalid UTF-8 in caller_name"))?;
            if !peers::valid_name(caller_name) {
                return Err(frame::invalid("invalid caller_name in PAIR_HELLO"));
            }
            Ok(PairMsg::Hello {
                auth_kind,
                auth_proof,
                commit_a,
                caller_name: caller_name.to_owned(),
            })
        }
        TAG_CHALLENGE => {
            if payload.len() < 33 {
                return Err(frame::invalid("truncated PAIR_CHALLENGE"));
            }
            let nonce_b: [u8; 32] = payload[..32].try_into().unwrap();
            let target_name = std::str::from_utf8(&payload[32..])
                .map_err(|_| frame::invalid("invalid UTF-8 in target_name"))?;
            if !peers::valid_name(target_name) {
                return Err(frame::invalid("invalid target_name in PAIR_CHALLENGE"));
            }
            Ok(PairMsg::Challenge {
                nonce_b,
                target_name: target_name.to_owned(),
            })
        }
        TAG_REVEAL => {
            if payload.len() != 32 {
                return Err(frame::invalid("invalid PAIR_REVEAL length"));
            }
            let nonce_a: [u8; 32] = payload.try_into().unwrap();
            Ok(PairMsg::Reveal { nonce_a })
        }
        TAG_APPROVED => {
            if !payload.is_empty() {
                return Err(frame::invalid("unexpected payload in PAIR_APPROVED"));
            }
            Ok(PairMsg::Approved)
        }
        TAG_REJECTED => {
            let reason = std::str::from_utf8(&payload)
                .map_err(|_| frame::invalid("invalid UTF-8 in PAIR_REJECTED"))?
                .to_owned();
            Ok(PairMsg::Rejected { reason })
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peers;
    use iroh::SecretKey;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn invite_roundtrips_82_chars_and_accepts_trimmed_case_insensitive_input() {
        let id = SecretKey::generate().public();
        let secret: [u8; 16] = random_bytes();
        let encoded = encode_invite(id, &secret);
        assert_eq!(encoded.len(), 82);
        assert!(encoded.starts_with("hvi1-"));
        assert!(
            encoded[5..]
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '2'..='7'))
        );

        let noisy = format!("  {}\r\n", encoded.to_uppercase());
        let (decoded_id, decoded_secret) = decode_invite(&noisy).unwrap();
        assert_eq!(decoded_id, id);
        assert_eq!(decoded_secret, secret);

        assert!(decode_invite(&encoded.replacen("hvi1-", "hvi2-", 1)).is_err());
        assert!(decode_invite(&encoded[..81]).is_err());
        assert!(decode_invite(&format!("{encoded}a")).is_err());
        assert!(decode_invite(&format!("hvi1-1{}", &encoded[6..])).is_err());

        let mut flipped = encoded.into_bytes();
        flipped[10] = if flipped[10] == b'a' { b'b' } else { b'a' };
        let (tampered_id, tampered_secret) = decode_invite(&String::from_utf8(flipped).unwrap())
            .unwrap_or_else(|_| (SecretKey::generate().public(), [0u8; 16]));
        assert_ne!((tampered_id, tampered_secret), (id, secret));
    }

    #[test]
    fn auth_proof_binds_kind_token_and_tls_binding() {
        let token = [0x42u8; 16];
        let tls_binding = [0x99u8; 32];
        let proof = auth_proof(AuthKind::Invite, &token, &tls_binding);
        assert!(verify_auth_proof(
            AuthKind::Invite,
            &token,
            &tls_binding,
            &proof
        ));
        assert!(!verify_auth_proof(
            AuthKind::Lan,
            &token,
            &tls_binding,
            &proof
        ));

        let mut bad_token = token;
        bad_token[0] ^= 1;
        assert!(!verify_auth_proof(
            AuthKind::Invite,
            &bad_token,
            &tls_binding,
            &proof
        ));

        let mut bad_binding = tls_binding;
        bad_binding[31] ^= 1;
        assert!(!verify_auth_proof(
            AuthKind::Invite,
            &token,
            &bad_binding,
            &proof
        ));
    }

    #[test]
    fn commit_and_sas_are_deterministic_and_sensitive_to_every_input() {
        let id_a = SecretKey::from_bytes(&[1u8; 32]).public();
        let id_b = SecretKey::from_bytes(&[2u8; 32]).public();
        let tls = [3u8; 32];
        let nonce_a = [4u8; 32];
        let nonce_b = [5u8; 32];

        let commit = compute_commit_a(&tls, id_a, id_b, &nonce_a);
        assert!(verify_commit_a(&tls, id_a, id_b, &nonce_a, &commit));
        assert!(!verify_commit_a(&tls, id_b, id_a, &nonce_a, &commit));
        let mut tls2 = tls;
        tls2[0] ^= 1;
        assert!(!verify_commit_a(&tls2, id_a, id_b, &nonce_a, &commit));
        let mut na2 = nonce_a;
        na2[15] ^= 1;
        assert!(!verify_commit_a(&tls, id_a, id_b, &na2, &commit));

        let sas = derive_sas(&tls, id_a, id_b, &nonce_a, &nonce_b);
        assert_eq!(sas.len(), 7);
        assert_eq!(&sas[3..4], "-");
        assert!(sas[..3].chars().all(|c| c.is_ascii_digit()));
        assert!(sas[4..].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(sas, derive_sas(&tls, id_a, id_b, &nonce_a, &nonce_b));

        assert_ne!(sas, derive_sas(&tls, id_b, id_a, &nonce_a, &nonce_b));
        assert_ne!(sas, derive_sas(&tls2, id_a, id_b, &nonce_a, &nonce_b));
        assert_ne!(sas, derive_sas(&tls, id_a, id_b, &na2, &nonce_b));
        let mut nb2 = nonce_b;
        nb2[0] ^= 1;
        assert_ne!(sas, derive_sas(&tls, id_a, id_b, &nonce_a, &nb2));
    }

    #[test]
    fn sanitize_hostname_strips_local_suffix_non_ascii_and_bounds_length() {
        let fallback = SecretKey::from_bytes(&[7u8; 32]).public();
        let name = sanitize_hostname("지민의 MacBook Pro (2).local", fallback);
        assert_eq!(name, "MacBook-Pro-2");
        assert!(peers::valid_name(&name));

        let ko_only = sanitize_hostname("한글만.local", fallback);
        assert_eq!(ko_only, format!("peer-{}", &fallback.to_string()[..8]));
        assert!(peers::valid_name(&ko_only));

        let long = sanitize_hostname(&"a".repeat(60), fallback);
        assert_eq!(long.len(), 32);
        assert!(peers::valid_name(&long));
    }

    #[tokio::test]
    async fn pair_messages_roundtrip_and_reject_oversized_or_invalid_frames() {
        let msgs = vec![
            PairMsg::Hello {
                auth_kind: AuthKind::Invite,
                auth_proof: [1u8; 32],
                commit_a: [2u8; 32],
                caller_name: "macbook".into(),
            },
            PairMsg::Hello {
                auth_kind: AuthKind::Lan,
                auth_proof: [3u8; 32],
                commit_a: [4u8; 32],
                caller_name: "desk-pc".into(),
            },
            PairMsg::Challenge {
                nonce_b: [5u8; 32],
                target_name: "home-server".into(),
            },
            PairMsg::Reveal { nonce_a: [6u8; 32] },
            PairMsg::Approved,
            PairMsg::Rejected {
                reason: "busy".into(),
            },
        ];

        let (mut client, mut server) = tokio::io::duplex(4096);
        for msg in &msgs {
            write_msg(&mut client, msg).await.unwrap();
        }
        for expected in &msgs {
            let got = read_msg(&mut server).await.unwrap();
            assert_eq!(&got, expected);
        }

        let (mut w, mut r) = tokio::io::duplex(64);
        w.write_all(&[1u8, 0x01, 0x01]).await.unwrap();
        assert_eq!(
            read_msg(&mut r).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let (mut w, mut r) = tokio::io::duplex(64);
        w.write_all(&[99u8, 0x00, 0x00]).await.unwrap();
        assert_eq!(
            read_msg(&mut r).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let (mut w, mut r) = tokio::io::duplex(256);
        let mut payload = Vec::new();
        payload.push(1u8);
        payload.push(1u8);
        payload.extend_from_slice(&[0u8; 64]);
        payload.extend_from_slice(b"bad/name");
        w.write_all(&[1u8]).await.unwrap();
        w.write_all(&(payload.len() as u16).to_be_bytes())
            .await
            .unwrap();
        w.write_all(&payload).await.unwrap();
        assert_eq!(
            read_msg(&mut r).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn sanitize_hostname_handles_multibyte_utf8_without_panicking() {
        let fallback = SecretKey::from_bytes(&[9u8; 32]).public();
        assert_eq!(sanitize_hostname("컴퓨터A", fallback), "A");
        assert_eq!(sanitize_hostname("컴퓨터A.local", fallback), "A");
        assert_eq!(sanitize_hostname("太郎のMac", fallback), "Mac");
        assert_eq!(
            sanitize_hostname("주피의-노트북", fallback),
            format!("peer-{}", &fallback.to_string()[..8])
        );
        assert_eq!(
            sanitize_hostname("😀😀", fallback),
            format!("peer-{}", &fallback.to_string()[..8])
        );
    }
}
