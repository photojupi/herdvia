use crate::{frame, ipc};
use iroh::{EndpointId, SecretKey};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
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

fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != ipc::uid() || meta.mode() & 0o177 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be owned by this user with permissions no wider than 0600",
                path.display()
            ),
        ));
    }
    Ok(file)
}

pub fn load_key(dir: &Path) -> io::Result<SecretKey> {
    ipc::prepare_dir(dir)?;
    let path = dir.join("secret.key");
    let mut file = match private_file(&path) {
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
            private_file(&path)?
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
    let mut file = match private_file(&dir.join("peers")) {
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

fn update(dir: &Path, edit: impl FnOnce(&mut Vec<Peer>)) -> io::Result<()> {
    load_key(dir)?;
    let lock = private_file(&dir.join("secret.key"))?;
    // The key inode is stable. Use it to serialize peer edits without a separate lock file.
    // SAFETY: lock owns this descriptor until the entire update finishes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut peers = load(dir)?;
    edit(&mut peers);
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
    })
}

pub fn revoke(dir: &Path, name: &str) -> io::Result<()> {
    if !valid_name(name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| peers.retain(|peer| peer.name != name))
}

pub fn fingerprint(id: EndpointId) -> String {
    let id = id.to_string();
    format!("{}-{}-{}-{}", &id[..4], &id[4..8], &id[8..12], &id[12..16])
}

#[cfg(test)]
mod tests {
    use super::*;
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
