use crate::{frame, ipc};
use iroh::{EndpointId, SecretKey};
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    In,
    Out,
    Both,
}

impl Permission {
    pub fn parse(value: &str) -> io::Result<Self> {
        match value {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            "both" => Ok(Self::Both),
            _ => Err(frame::invalid("invalid peer permission")),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Both => "both",
        }
    }
    pub fn inbound(self) -> bool {
        self != Self::Out
    }
    pub fn outbound(self) -> bool {
        self != Self::In
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub id: EndpointId,
    pub permission: Permission,
}

pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn load_key(dir: &Path) -> io::Result<SecretKey> {
    ipc::prepare_dir(dir)?;
    let path = dir.join("secret.key");
    let mut file = match ipc::private_file(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            tmp.write_all(&key.to_bytes())?;
            tmp.as_file().sync_all()?;
            // Publish the complete key atomically, including when two commands start together.
            match tmp.persist_noclobber(&path) {
                Ok(_) => {}
                Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.error),
            }
            ipc::private_file(&path)?
        }
        Err(err) => return Err(err),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| frame::invalid("secret.key must contain exactly 32 bytes"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

pub fn load(dir: &Path) -> io::Result<Vec<Peer>> {
    let mut file = match ipc::private_file(&dir.join("peers")) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut peers = Vec::new();
    let mut names = HashSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 3 || !valid_name(fields[0]) || !names.insert(fields[0]) {
            return Err(frame::invalid("invalid or duplicate peer name in peers"));
        }
        peers.push(Peer {
            name: fields[0].into(),
            id: fields[1]
                .parse()
                .map_err(|_| frame::invalid("invalid EndpointId in peers"))?,
            permission: Permission::parse(fields[2])?,
        });
    }
    Ok(peers)
}

fn update(dir: &Path, edit: impl FnOnce(&mut Vec<Peer>) -> io::Result<()>) -> io::Result<()> {
    load_key(dir)?;
    let lock = ipc::private_file(&dir.join("secret.key"))?;
    ipc::lock_file(&lock)?;
    let mut peers = load(dir)?;
    edit(&mut peers)?;
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    for peer in peers {
        writeln!(
            tmp,
            "{} {} {}",
            peer.name,
            peer.id,
            peer.permission.as_str()
        )?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(dir.join("peers")).map_err(|err| err.error)?;
    Ok(())
}

pub fn allow(dir: &Path, peer: Peer) -> io::Result<()> {
    if !valid_name(&peer.name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        peers.retain(|p| p.name != peer.name);
        peers.push(peer);
        Ok(())
    })
}

/// Like `allow`, but refuses a name that is registered with another EndpointId.
/// The check runs under the same lock as the write.
pub fn accept(dir: &Path, peer: Peer) -> io::Result<()> {
    if !valid_name(&peer.name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        if peers.iter().any(|p| p.name == peer.name && p.id != peer.id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "peer {} is already registered with a different EndpointId; revoke it first or use another name",
                    peer.name
                ),
            ));
        }
        peers.retain(|p| p.name != peer.name);
        peers.push(peer);
        Ok(())
    })
}

pub fn revoke(dir: &Path, name: &str) -> io::Result<()> {
    if !valid_name(name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        peers.retain(|peer| peer.name != name);
        Ok(())
    })
}

pub fn fingerprint(id: EndpointId) -> String {
    let id = id.to_string();
    format!("{}-{}-{}-{}", &id[..4], &id[4..8], &id[8..12], &id[12..16])
}

/// Normalizes a fingerprint typed by hand: 16 hex digits in any case, with or without the dashes.
pub fn parse_fingerprint(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let hex = if bytes.len() == 19 && [4, 9, 14].iter().all(|&i| bytes[i] == b'-') {
        input.replace('-', "")
    } else {
        input.to_owned()
    };
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}",
        &hex[..4],
        &hex[4..8],
        &hex[8..12],
        &hex[12..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn names_use_only_allowed_ascii_with_length_limits() {
        for name in ["a", "A.b_9-z", &"x".repeat(32)] {
            assert!(valid_name(name));
        }
        for name in ["", "a b", "a/b", "한글", &"x".repeat(33)] {
            assert!(!valid_name(name));
        }
    }

    #[cfg(unix)]
    #[test]
    fn key_survives_reload_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let key = load_key(dir.path()).unwrap();
        assert_eq!(load_key(dir.path()).unwrap().to_bytes(), key.to_bytes());
        assert_eq!(
            fs::metadata(dir.path().join("secret.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let id = key.public().to_string();
        assert_eq!(
            fingerprint(key.public()),
            format!("{}-{}-{}-{}", &id[..4], &id[4..8], &id[8..12], &id[12..16])
        );
    }

    #[test]
    fn typed_fingerprints_ignore_case_and_optional_dashes() {
        let id = SecretKey::generate().public();
        let fp = fingerprint(id);
        for typed in [
            fp.clone(),
            fp.to_uppercase(),
            fp.replace('-', ""),
            fp.replace('-', "").to_uppercase(),
        ] {
            assert_eq!(parse_fingerprint(&typed).as_deref(), Some(fp.as_str()));
        }
        for typed in [
            "",
            "0123-4567-89ab-cde",
            "0123-4567-89ab-cdef0",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123-456789ab-cdef",
            "01234-567-89ab-cdef",
            "0123-4567-89ab-cdeg",
            "0123 4567 89ab cdef",
            "+123-4567-89ab-cdef",
            "한글한글한글한글",
        ] {
            assert_eq!(parse_fingerprint(typed), None, "{typed:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn allow_updates_and_revoke_persists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
        let mut peer = Peer {
            name: "work".into(),
            id: SecretKey::generate().public(),
            permission: Permission::In,
        };
        allow(dir.path(), peer.clone()).unwrap();
        assert_eq!(load(dir.path()).unwrap(), vec![peer.clone()]);
        peer.permission = Permission::Both;
        allow(dir.path(), peer.clone()).unwrap();
        assert_eq!(load(dir.path()).unwrap(), vec![peer]);
        assert_eq!(
            fs::metadata(dir.path().join("peers"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        revoke(dir.path(), "work").unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn accept_refuses_a_name_with_another_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut peer = Peer {
            name: "work".into(),
            id: SecretKey::generate().public(),
            permission: Permission::Out,
        };
        allow(dir.path(), peer.clone()).unwrap();
        let other = Peer {
            id: SecretKey::generate().public(),
            permission: Permission::In,
            ..peer.clone()
        };
        let err = accept(dir.path(), other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(load(dir.path()).unwrap(), vec![peer.clone()]);
        peer.permission = Permission::In;
        accept(dir.path(), peer.clone()).unwrap();
        assert_eq!(load(dir.path()).unwrap(), vec![peer]);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_wide_permissions_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("secret.key"), [1; 32]).unwrap();
        fs::set_permissions(
            dir.path().join("secret.key"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(load_key(dir.path()).is_err());
        fs::write(dir.path().join("peers"), "").unwrap();
        fs::set_permissions(dir.path().join("peers"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(dir.path()).is_err());
        fs::set_permissions(dir.path().join("peers"), fs::Permissions::from_mode(0o600)).unwrap();
        for row in ["x invalid both", "x", "x bad unknown"] {
            fs::write(dir.path().join("peers"), row).unwrap();
            assert!(load(dir.path()).is_err());
        }
    }
}
