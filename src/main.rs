use herdvia::{
    control, ipc, network,
    peers::{self, Peer, Permission},
};
use iroh::EndpointId;
use std::io::{self, Write};

enum Command {
    Agent,
    #[cfg(windows)]
    AgentLog,
    AgentInstall,
    AgentUninstall,
    Via {
        peer: String,
        line: String,
    },
    Id,
    Allow(Peer),
    Revoke(String),
    Status,
    Pending,
    Accept {
        name: String,
        fingerprint: String,
        permission: Permission,
    },
    Version,
    Doctor,
}

fn parse(args: Vec<String>) -> io::Result<Command> {
    match args.as_slice() {
        [flag] if flag == "--version" => Ok(Command::Version),
        [cmd] if cmd == "agent" => Ok(Command::Agent),
        #[cfg(windows)]
        [cmd, flag] if cmd == "agent" && flag == "--log" => Ok(Command::AgentLog),
        [cmd, action] if cmd == "agent" && action == "install" => Ok(Command::AgentInstall),
        [cmd, action] if cmd == "agent" && action == "uninstall" => Ok(Command::AgentUninstall),
        [cmd] if cmd == "id" => Ok(Command::Id),
        [cmd] if cmd == "status" => Ok(Command::Status),
        [cmd] if cmd == "pending" => Ok(Command::Pending),
        [cmd] if cmd == "doctor" => Ok(Command::Doctor),
        [cmd, name] if cmd == "revoke" && peers::valid_name(name) => {
            Ok(Command::Revoke(name.clone()))
        }
        [cmd, name, id, flag] if cmd == "allow" && peers::valid_name(name) => {
            let permission = match flag.as_str() {
                "--inbound" => Permission::In,
                "--outbound" => Permission::Out,
                "--both" => Permission::Both,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "allow requires --inbound, --outbound, or --both",
                    ));
                }
            };
            Ok(Command::Allow(Peer {
                name: name.clone(),
                id: id.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid EndpointId")
                })?,
                permission,
            }))
        }
        [cmd, name, fingerprint, flag] if cmd == "accept" && peers::valid_name(name) => {
            let permission = match flag.as_str() {
                "--inbound" => Permission::In,
                "--both" => Permission::Both,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "accept requires --inbound or --both",
                    ));
                }
            };
            Ok(Command::Accept {
                name: name.clone(),
                fingerprint: peers::parse_fingerprint(fingerprint).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid fingerprint")
                })?,
                permission,
            })
        }
        [cmd, peer, line] if cmd == "via" => Ok(Command::Via {
            peer: peer.clone(),
            line: line.clone(),
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            if cfg!(windows) {
                "usage: herdvia agent [install|uninstall|--log] | via <peer> <line> | id | allow <name> <EndpointId> (--inbound|--outbound|--both) | revoke <name> | status | pending | accept <name> <fingerprint> (--inbound|--both) | doctor | --version"
            } else {
                "usage: herdvia agent [install|uninstall] | via <peer> <line> | id | allow <name> <EndpointId> (--inbound|--outbound|--both) | revoke <name> | status | pending | accept <name> <fingerprint> (--inbound|--both) | doctor | --version"
            },
        )),
    }
}

#[tokio::main]
async fn main() {
    let args: io::Result<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arguments must be UTF-8"))
        })
        .collect();
    let result = match ipc::reject_root().and(args).and_then(parse) {
        #[cfg(unix)]
        Ok(Command::Agent) => herdvia::agent::run(herdvia::launchd::log_path())
            .await
            .map(|()| 0),
        #[cfg(windows)]
        Ok(Command::Agent) => herdvia::agent::run(None).await.map(|()| 0),
        #[cfg(windows)]
        Ok(Command::AgentLog) => match herdvia::launchd::redirect_log() {
            Ok(path) => herdvia::agent::run(Some(path)).await.map(|()| 0),
            Err(err) => Err(err),
        },
        Ok(Command::AgentInstall) => herdvia::launchd::install().await.map(|()| 0),
        #[cfg(unix)]
        Ok(Command::AgentUninstall) => herdvia::launchd::uninstall().map(|()| 0),
        #[cfg(windows)]
        Ok(Command::AgentUninstall) => herdvia::launchd::uninstall().await.map(|()| 0),
        Ok(Command::Via { peer, line }) => herdvia::via::run(&peer, &line).await,
        Ok(Command::Doctor) => herdvia::doctor::run().await,
        Ok(Command::Version) => {
            println!("herdvia {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Ok(command) => manage(command).await.map(|()| 0),
        Err(err) => Err(err),
    };
    let code = match result {
        Ok(code) => code,
        Err(err) => {
            // Not eprintln!: it panics when standard error cannot be written (a full disk).
            let _ = writeln!(io::stderr(), "herdvia: {err}");
            255
        }
    };
    // Tokio's stdin may have a blocked reader thread. EXIT must not wait for it.
    std::process::exit(code);
}

async fn manage(command: Command) -> io::Result<()> {
    let dir = ipc::data_dir()?;
    match command {
        Command::Id => {
            let id = peers::load_key(&dir)?.public();
            println!("{id}\n{}", peers::fingerprint(id));
        }
        Command::Allow(peer) => {
            peers::allow(&dir, peer.clone())?;
            control::reload_if_running(&dir).await?;
            println!("{} {} {}", peer.name, peer.id, peer.permission.as_str());
        }
        Command::Revoke(name) => {
            let (peer, others) = peers::revoke(&dir, &name)?;
            control::reload_if_running(&dir).await?;
            println!("revoked {name} {}", peer.id);
            if !others.is_empty() {
                eprintln!(
                    "herdvia: warning: {} is still allowed as peer {}; revoke each of those names to remove it",
                    peer.id,
                    others.join(", ")
                );
            }
        }
        Command::Status => {
            let status = control::request(&dir, "status").await?;
            print!("{status}");
            if cfg!(target_os = "macos") && on_relay(&status) {
                eprintln!(
                    "herdvia: note: a peer is connected through a relay. If it stays that way and the peer is on the same local network as this Mac, allow herdvia under System Settings > Privacy & Security > Local Network."
                );
            }
        }
        Command::Pending => print!("{}", control::request(&dir, "pending").await?),
        Command::Accept {
            name,
            fingerprint,
            permission,
        } => {
            let id = pending_match(&control::request(&dir, "pending").await?, &fingerprint)?;
            peers::accept(
                &dir,
                Peer {
                    name: name.clone(),
                    id,
                    permission,
                },
            )?;
            control::reload_if_running(&dir).await?;
            println!("{name} {id} {}", permission.as_str());
        }
        _ => unreachable!("agent, via and version are handled separately"),
    }
    Ok(())
}

/// Whether a `status` response shows a peer connected through a relay.
fn on_relay(status: &str) -> bool {
    status.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace().skip(2);
        fields.next() == Some("connected") && fields.next() == Some("relay")
    })
}

/// Finds the one entry of a `pending` response whose EndpointId has this fingerprint.
fn pending_match(pending: &str, fingerprint: &str) -> io::Result<EndpointId> {
    let mut found = Vec::new();
    let mut entries = 0;
    for line in pending.lines().skip(1) {
        let id: EndpointId = line
            .split_whitespace()
            .nth(1)
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid pending response")
            })?;
        entries += 1;
        if peers::fingerprint(id) == fingerprint {
            found.push(id);
        }
    }
    // A full list may have pushed out the real caller, or a second key with the same fingerprint.
    if entries >= network::MAX_ATTEMPTS {
        return Err(io::Error::other(
            "the list of connection attempts is full (someone may be flooding this PC with connection attempts); use herdvia allow <name> <EndpointId> --inbound with the full EndpointId",
        ));
    }
    match found.as_slice() {
        [id] => Ok(*id),
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no connection attempt with fingerprint {fingerprint}; check that the other PC allowed this PC with --outbound and its agent is running, or use herdvia allow <name> <EndpointId> --inbound with the full EndpointId"
            ),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "several connection attempts have fingerprint {fingerprint}; use herdvia allow with the full EndpointId"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_log_is_windows_only_and_requires_exact_arguments() {
        let result = parse(vec!["agent".into(), "--log".into()]);
        #[cfg(windows)]
        assert!(matches!(result.unwrap(), Command::AgentLog));
        #[cfg(not(windows))]
        assert!(result.is_err());
        for args in [
            vec!["agent", "--log", "extra"],
            vec!["agent", "install", "--log"],
            vec!["agent", "uninstall", "--log"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }

    #[test]
    fn accepts_agent_subcommands_and_via_with_exact_argument_counts() {
        assert!(matches!(
            parse(vec!["agent".into()]).unwrap(),
            Command::Agent
        ));
        assert!(matches!(
            parse(vec!["agent".into(), "install".into()]).unwrap(),
            Command::AgentInstall
        ));
        assert!(matches!(
            parse(vec!["agent".into(), "uninstall".into()]).unwrap(),
            Command::AgentUninstall
        ));
        match parse(vec!["via".into(), "peer".into(), "one entire line".into()]).unwrap() {
            Command::Via { peer, line } => {
                assert_eq!(peer, "peer");
                assert_eq!(line, "one entire line");
            }
            _ => panic!("expected via"),
        }
        for args in [
            vec![],
            vec!["id", "extra"],
            vec!["agent", "unknown"],
            vec!["agent", "install", "extra"],
            vec!["agent", "uninstall", "extra"],
            vec!["via", "peer"],
            vec!["via", "p", "l", "extra"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }

    #[test]
    fn parses_management_commands_and_requires_explicit_direction() {
        assert!(matches!(parse(vec!["id".into()]).unwrap(), Command::Id));
        assert!(matches!(
            parse(vec!["status".into()]).unwrap(),
            Command::Status
        ));
        assert!(matches!(
            parse(vec!["revoke".into(), "work".into()]).unwrap(),
            Command::Revoke(_)
        ));
        let id = iroh::SecretKey::generate().public().to_string();
        for (flag, permission) in [
            ("--inbound", Permission::In),
            ("--outbound", Permission::Out),
            ("--both", Permission::Both),
        ] {
            match parse(vec!["allow".into(), "work".into(), id.clone(), flag.into()]).unwrap() {
                Command::Allow(peer) => {
                    assert_eq!(peer.name, "work");
                    assert_eq!(peer.permission, permission);
                }
                _ => panic!("expected allow"),
            }
        }
        assert!(parse(vec!["allow".into(), "work".into(), id.clone()]).is_err());
        assert!(parse(vec!["allow".into(), "bad/name".into(), id, "--both".into()]).is_err());
        assert!(
            parse(vec![
                "allow".into(),
                "work".into(),
                "bad-id".into(),
                "--both".into()
            ])
            .is_err()
        );
    }

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn version_takes_no_other_arguments() {
        assert!(matches!(
            parse(args(&["--version"])).unwrap(),
            Command::Version
        ));
        for a in [
            &["--version", "extra"][..],
            &["version"],
            &["-V"],
            &["id", "--version"],
        ] {
            assert!(parse(args(a)).is_err(), "{a:?}");
        }
    }

    #[test]
    fn doctor_takes_no_other_arguments() {
        assert!(matches!(parse(args(&["doctor"])).unwrap(), Command::Doctor));
        for a in [
            &["doctor", "extra"][..],
            &["doctor", "--version"],
            &["Doctor"],
        ] {
            assert!(parse(args(a)).is_err(), "{a:?}");
        }
    }

    #[test]
    fn parses_pending_and_accept_with_inbound_directions_only() {
        assert!(matches!(
            parse(args(&["pending"])).unwrap(),
            Command::Pending
        ));
        assert!(parse(args(&["pending", "extra"])).is_err());
        for (typed, flag, expected) in [
            ("0123-4567-89ab-cdef", "--inbound", Permission::In),
            ("0123456789ABCDEF", "--both", Permission::Both),
        ] {
            match parse(args(&["accept", "work", typed, flag])).unwrap() {
                Command::Accept {
                    name,
                    fingerprint,
                    permission,
                } => {
                    assert_eq!(name, "work");
                    assert_eq!(fingerprint, "0123-4567-89ab-cdef");
                    assert_eq!(permission, expected);
                }
                _ => panic!("expected accept"),
            }
        }
        for a in [
            &["accept", "work", "0123-4567-89ab-cdef", "--outbound"][..],
            &["accept", "work", "0123-4567-89ab-cdef", "--sideways"],
            &["accept", "work", "0123-4567-89ab-cdef"],
            &["accept", "work", "0123-4567-89ab-cde", "--both"],
            &["accept", "bad/name", "0123-4567-89ab-cdef", "--both"],
            &["accept", "work", "0123-4567-89ab-cdef", "--both", "extra"],
            &["accept"],
        ] {
            assert!(parse(args(a)).is_err(), "{a:?}");
        }
    }

    #[test]
    fn relay_note_needs_a_peer_connected_through_a_relay() {
        let header = "name permission connection path rtt\n";
        assert!(!on_relay(header));
        assert!(!on_relay(&format!(
            "{header}a out connected direct 4.12ms\nb in disconnected - -\n"
        )));
        assert!(on_relay(&format!(
            "{header}a out connected direct 4.12ms\nb both connected relay 238.82ms\n"
        )));
        // A peer may be named "relay" or "connected"; only the state columns count.
        assert!(!on_relay(&format!(
            "{header}relay out connected direct 4.12ms\nconnected in disconnected - -\n"
        )));
    }

    #[test]
    fn accept_needs_exactly_one_pending_match() {
        let a = iroh::SecretKey::generate().public();
        let b = iroh::SecretKey::generate().public();
        let row = |id: &str| format!("x {id} 1 0s\n");
        let header = "fingerprint id attempts last-seen\n";
        let pending = format!("{header}{}{}", row(&a.to_string()), row(&b.to_string()));
        assert_eq!(pending_match(&pending, &peers::fingerprint(a)).unwrap(), a);
        let err = pending_match(header, &peers::fingerprint(a)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        // Two keys sharing a fingerprint cannot be generated here; a repeated row stands in.
        let pending = format!("{pending}{}", row(&a.to_string()));
        let err = pending_match(&pending, &peers::fingerprint(a)).unwrap_err();
        assert!(err.to_string().contains("herdvia allow"), "{err}");
        assert!(pending_match(&format!("{header}x bad 1 0s\n"), "0000-0000-0000-0000").is_err());
    }

    #[test]
    fn accept_refuses_a_full_pending_list_even_with_one_match() {
        let header = "fingerprint id attempts last-seen\n";
        let ids: Vec<_> = (0..network::MAX_ATTEMPTS)
            .map(|_| iroh::SecretKey::generate().public())
            .collect();
        let rows = |ids: &[EndpointId]| -> String {
            ids.iter().map(|id| format!("x {id} 1 0s\n")).collect()
        };
        let below = format!("{header}{}", rows(&ids[1..]));
        let fp = peers::fingerprint(ids[1]);
        assert_eq!(pending_match(&below, &fp).unwrap(), ids[1]);
        let err = pending_match(&below, "0000-0000-0000-0000").unwrap_err();
        assert!(err.to_string().contains("--inbound"), "{err}");
        let full = format!("{header}{}", rows(&ids));
        for fingerprint in [fp.as_str(), "0000-0000-0000-0000"] {
            let err = pending_match(&full, fingerprint).unwrap_err();
            assert!(err.to_string().contains("is full"), "{err}");
            assert!(err.to_string().contains("herdvia allow"), "{err}");
        }
    }
}
