use crate::{frame, ipc};
use iroh::{EndpointId, SecretKey};
use std::{
    collections::HashSet,
    fs::File,
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
    match ipc::private_file(&dir.join("peers")) {
        Ok(file) => read(file),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

/// The peers in an open peers file.
pub(crate) fn read(mut file: File) -> io::Result<Vec<Peer>> {
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

fn update<T>(dir: &Path, edit: impl FnOnce(&mut Vec<Peer>) -> io::Result<T>) -> io::Result<T> {
    load_key(dir)?;
    let lock = ipc::private_file(&dir.join("secret.key"))?;
    ipc::lock_file(&lock)?;
    let mut peers = load(dir)?;
    let result = edit(&mut peers)?;
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
    Ok(result)
}

/// Names other than `name` that are registered with `id`.
fn other_names(peers: &[Peer], id: EndpointId, name: &str) -> Vec<String> {
    peers
        .iter()
        .filter(|p| p.id == id && p.name != name)
        .map(|p| p.name.clone())
        .collect()
}

/// One EndpointId under two names would survive a revoke of either name.
fn refuse_another_name(peers: &[Peer], peer: &Peer) -> io::Result<()> {
    let others = other_names(peers, peer.id, &peer.name);
    if others.is_empty() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "EndpointId {} is already registered as peer {}; revoke it first or use that name",
            peer.id,
            others.join(", ")
        ),
    ))
}

/// Returns the entry it replaced when the name was registered with another EndpointId.
pub fn allow(dir: &Path, peer: Peer) -> io::Result<Option<Peer>> {
    if !valid_name(&peer.name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        refuse_another_name(peers, &peer)?;
        let replaced = peers
            .iter()
            .find(|p| p.name == peer.name && p.id != peer.id)
            .cloned();
        peers.retain(|p| p.name != peer.name);
        peers.push(peer);
        Ok(replaced)
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
        refuse_another_name(peers, &peer)?;
        peers.retain(|p| p.name != peer.name);
        peers.push(peer);
        Ok(())
    })
}

/// Removes the named peer and returns it with the other names still registered with its EndpointId.
/// An unknown name is an error and leaves the file untouched.
pub fn revoke(dir: &Path, name: &str) -> io::Result<(Peer, Vec<String>)> {
    if !valid_name(name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        let index = peers
            .iter()
            .position(|peer| peer.name == name)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no peer named {name}"))
            })?;
        let peer = peers.remove(index);
        let others = other_names(peers, peer.id, name);
        Ok((peer, others))
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

pub fn check_connect_target(
    dir: &Path,
    name: &str,
    known_target: Option<EndpointId>,
) -> io::Result<()> {
    if !valid_name(name) {
        return Err(frame::invalid("invalid peer name"));
    }
    let peers = load(dir)?;
    if let Some(target) = known_target {
        refuse_another_name(
            &peers,
            &Peer {
                name: name.to_owned(),
                id: target,
                permission: Permission::Out,
            },
        )?;
        if peers.iter().any(|p| p.name == name && p.id != target) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "peer {name} is already registered with a different EndpointId; revoke it first or choose another name"
                ),
            ));
        }
    } else if peers
        .iter()
        .any(|p| p.name == name && p.permission.outbound())
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "peer {name} is already registered; revoke it first or choose another name; to re-run only the Herdr registration, run herdvia register {name}"
            ),
        ));
    }
    Ok(())
}

pub fn resolve_inbound_peer(
    peers: &[Peer],
    explicit_name: Option<&str>,
    caller_name: &str,
    id: EndpointId,
) -> io::Result<Peer> {
    if !valid_name(caller_name) {
        return Err(frame::invalid("invalid peer name"));
    }
    if let Some(exp) = explicit_name
        && !valid_name(exp)
    {
        return Err(frame::invalid("invalid peer name"));
    }

    let existing_for_id: Vec<_> = peers.iter().filter(|p| p.id == id).collect();
    if !existing_for_id.is_empty() {
        if let Some(exp) = explicit_name
            && !existing_for_id.iter().any(|p| p.name == exp)
        {
            let names: Vec<_> = existing_for_id.iter().map(|p| p.name.as_str()).collect();
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "EndpointId {id} is already registered as peer {}; revoke it first or use that name",
                    names.join(", ")
                ),
            ));
        }
        let chosen = explicit_name
            .and_then(|exp| existing_for_id.iter().copied().find(|p| p.name == exp))
            .unwrap_or(existing_for_id[0]);
        let permission = if chosen.permission.outbound() {
            Permission::Both
        } else {
            Permission::In
        };
        return Ok(Peer {
            name: chosen.name.clone(),
            id,
            permission,
        });
    }

    if let Some(exp) = explicit_name {
        if peers.iter().any(|p| p.name == exp) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "peer {exp} is already registered with a different EndpointId; revoke it first or use another name"
                ),
            ));
        }
        return Ok(Peer {
            name: exp.to_owned(),
            id,
            permission: Permission::In,
        });
    }

    // herdvia revoke --help (or -h) prints help, so a peer saved under either name could not be
    // revoked; such a caller name gets a suffix like a taken one.
    let help_flag = matches!(caller_name, "--help" | "-h");
    if !help_flag && !peers.iter().any(|p| p.name == caller_name) {
        return Ok(Peer {
            name: caller_name.to_owned(),
            id,
            permission: Permission::In,
        });
    }

    let id_hex = id.to_string();
    for suffix_len in [4usize, 8, 12, 16, 20, 24] {
        let max_base = 32usize.saturating_sub(1 + suffix_len);
        let base = caller_name[..caller_name.len().min(max_base)].trim_end_matches('-');
        let candidate = if base.is_empty() {
            id_hex[..suffix_len].to_owned()
        } else {
            format!("{base}-{}", &id_hex[..suffix_len])
        };
        if valid_name(&candidate) && !peers.iter().any(|p| p.name == candidate) {
            return Ok(Peer {
                name: candidate,
                id,
                permission: Permission::In,
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to choose an unused peer name",
    ))
}

pub fn grant_inbound(
    dir: &Path,
    explicit_name: Option<&str>,
    caller_name: &str,
    id: EndpointId,
) -> io::Result<Peer> {
    if !valid_name(caller_name) {
        return Err(frame::invalid("invalid peer name"));
    }
    if let Some(exp) = explicit_name
        && !valid_name(exp)
    {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        let peer = resolve_inbound_peer(peers, explicit_name, caller_name, id)?;
        peers.retain(|p| p.name != peer.name);
        peers.push(peer.clone());
        Ok(peer)
    })
}

pub fn grant_outbound(dir: &Path, name: &str, id: EndpointId) -> io::Result<Peer> {
    if !valid_name(name) {
        return Err(frame::invalid("invalid peer name"));
    }
    update(dir, |peers| {
        if peers.iter().any(|p| p.name == name && p.id != id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "peer {name} is already registered with a different EndpointId; revoke it first or choose another name"
                ),
            ));
        }
        let probe = Peer {
            name: name.to_owned(),
            id,
            permission: Permission::Out,
        };
        refuse_another_name(peers, &probe)?;
        let permission = if peers
            .iter()
            .any(|p| p.name == name && p.id == id && p.permission.inbound())
        {
            Permission::Both
        } else {
            Permission::Out
        };
        let peer = Peer {
            name: name.to_owned(),
            id,
            permission,
        };
        peers.retain(|p| p.name != name);
        peers.push(peer.clone());
        Ok(peer)
    })
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
        assert_eq!(allow(dir.path(), peer.clone()).unwrap(), None);
        assert_eq!(load(dir.path()).unwrap(), vec![peer.clone()]);
        // A permission change under the same key replaces no other key.
        peer.permission = Permission::Both;
        assert_eq!(allow(dir.path(), peer.clone()).unwrap(), None);
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
    fn revoke_of_an_unknown_name_fails_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let err = revoke(dir.path(), "work").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "no peer named work");
        assert!(!dir.path().join("peers").exists());
        // Unsorted on purpose: a rewrite would sort it.
        let (a, b) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        let text = format!("b {b} in\na {a} out\n");
        fs::write(dir.path().join("peers"), &text).unwrap();
        fs::set_permissions(dir.path().join("peers"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            revoke(dir.path(), "work").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs::read_to_string(dir.path().join("peers")).unwrap(), text);
    }

    #[cfg(unix)]
    #[test]
    fn one_endpoint_cannot_be_registered_under_two_names() {
        let dir = tempfile::tempdir().unwrap();
        let peer = Peer {
            name: "work".into(),
            id: SecretKey::generate().public(),
            permission: Permission::Out,
        };
        allow(dir.path(), peer.clone()).unwrap();
        let other = Peer {
            name: "home".into(),
            permission: Permission::Both,
            ..peer.clone()
        };
        for result in [
            allow(dir.path(), other.clone()).map(drop),
            accept(dir.path(), other.clone()),
        ] {
            let err = result.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
            assert!(err.to_string().contains("as peer work"), "{err}");
        }
        assert_eq!(load(dir.path()).unwrap(), vec![peer.clone()]);
        // The same name may still change its permission or, through allow, its EndpointId.
        let changed = Peer {
            permission: Permission::In,
            ..peer.clone()
        };
        accept(dir.path(), changed.clone()).unwrap();
        assert_eq!(load(dir.path()).unwrap(), vec![changed.clone()]);
        let replaced = Peer {
            id: SecretKey::generate().public(),
            ..peer
        };
        assert_eq!(allow(dir.path(), replaced.clone()).unwrap(), Some(changed));
        assert_eq!(load(dir.path()).unwrap(), vec![replaced]);
    }

    #[cfg(unix)]
    #[test]
    fn revoke_names_what_still_allows_the_same_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let (k, other) = (
            SecretKey::generate().public(),
            SecretKey::generate().public(),
        );
        // Written before duplicates were refused: such a file still loads.
        fs::write(
            dir.path().join("peers"),
            format!("a {k} both\nb {k} in\nc {other} out\nd {k} out\n"),
        )
        .unwrap();
        fs::set_permissions(dir.path().join("peers"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load(dir.path()).unwrap().len(), 4);
        let (peer, others) = revoke(dir.path(), "a").unwrap();
        assert_eq!((peer.name.as_str(), peer.id), ("a", k));
        assert_eq!(others, ["b", "d"]);
        let (_, others) = revoke(dir.path(), "c").unwrap();
        assert!(others.is_empty());
        let names: Vec<_> = load(dir.path())
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["b", "d"]);
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

    #[test]
    fn grant_inbound_disambiguates_colliding_caller_names_and_stays_within_32_chars() {
        let dir = tempfile::tempdir().unwrap();
        let id_1 = SecretKey::from_bytes(&[1u8; 32]).public();
        let id_2 = SecretKey::from_bytes(&[2u8; 32]).public();
        let id_3 = SecretKey::from_bytes(&[3u8; 32]).public();

        let p1 = grant_inbound(dir.path(), None, "macbook", id_1).unwrap();
        assert_eq!(
            p1,
            Peer {
                name: "macbook".into(),
                id: id_1,
                permission: Permission::In
            }
        );

        let p2 = grant_inbound(dir.path(), None, "macbook", id_2).unwrap();
        assert_eq!(p2.name, format!("macbook-{}", &id_2.to_string()[..4]));
        assert_eq!(p2.id, id_2);
        assert_eq!(p2.permission, Permission::In);

        let long_name = "m".repeat(32);
        allow(
            dir.path(),
            Peer {
                name: long_name.clone(),
                id: SecretKey::from_bytes(&[4u8; 32]).public(),
                permission: Permission::In,
            },
        )
        .unwrap();
        let p3 = grant_inbound(dir.path(), None, &long_name, id_3).unwrap();
        assert!(valid_name(&p3.name));
        assert!(p3.name.len() <= 32);
        assert_eq!(
            p3.name,
            format!("{}-{}", "m".repeat(27), &id_3.to_string()[..4])
        );
    }

    #[test]
    fn grant_inbound_does_not_save_a_help_flag_sent_as_the_caller_name() {
        let dir = tempfile::tempdir().unwrap();
        for (seed, caller) in [(5u8, "--help"), (6, "-h")] {
            let id = SecretKey::from_bytes(&[seed; 32]).public();
            let saved = grant_inbound(dir.path(), None, caller, id).unwrap();
            assert_eq!(saved.name, format!("{caller}-{}", &id.to_string()[..4]));
        }
    }

    #[test]
    fn grant_inbound_and_outbound_upgrade_existing_reverse_permission_to_both() {
        let dir_b = tempfile::tempdir().unwrap();
        let id_a = SecretKey::from_bytes(&[10u8; 32]).public();
        let id_c = SecretKey::from_bytes(&[11u8; 32]).public();
        allow(
            dir_b.path(),
            Peer {
                name: "laptop".into(),
                id: id_a,
                permission: Permission::Out,
            },
        )
        .unwrap();

        let err = grant_inbound(dir_b.path(), Some("wrong"), "other-host", id_a).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("laptop"), "{err}");

        let err = grant_inbound(dir_b.path(), Some("laptop"), "other-host", id_c).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        let upgraded = grant_inbound(dir_b.path(), None, "other-host", id_a).unwrap();
        assert_eq!(
            upgraded,
            Peer {
                name: "laptop".into(),
                id: id_a,
                permission: Permission::Both
            }
        );

        let again = grant_inbound(dir_b.path(), Some("laptop"), "other-host", id_a).unwrap();
        assert_eq!(again.permission, Permission::Both);

        let dir_a = tempfile::tempdir().unwrap();
        let id_b = SecretKey::from_bytes(&[12u8; 32]).public();
        allow(
            dir_a.path(),
            Peer {
                name: "home".into(),
                id: id_b,
                permission: Permission::In,
            },
        )
        .unwrap();
        check_connect_target(dir_a.path(), "home", None).unwrap();
        let out_upgraded = grant_outbound(dir_a.path(), "home", id_b).unwrap();
        assert_eq!(
            out_upgraded,
            Peer {
                name: "home".into(),
                id: id_b,
                permission: Permission::Both
            }
        );
    }

    #[test]
    fn check_connect_target_fails_fast_on_existing_outbound_or_conflicting_id() {
        let dir = tempfile::tempdir().unwrap();
        let id_1 = SecretKey::from_bytes(&[21u8; 32]).public();
        let id_2 = SecretKey::from_bytes(&[22u8; 32]).public();

        allow(
            dir.path(),
            Peer {
                name: "home".into(),
                id: id_1,
                permission: Permission::Out,
            },
        )
        .unwrap();

        let err = check_connect_target(dir.path(), "home", None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            err.to_string(),
            "peer home is already registered; revoke it first or choose another name; to re-run only the Herdr registration, run herdvia register home"
        );
        check_connect_target(dir.path(), "home", Some(id_1)).unwrap();
        assert_eq!(
            check_connect_target(dir.path(), "home", Some(id_2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );

        let err = check_connect_target(dir.path(), "office", Some(id_1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("home"), "{err}");

        let err = grant_outbound(dir.path(), "office", id_1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("home"), "{err}");
    }
}
