use herdvia::{
    control, ipc, network,
    peers::{self, Peer, Permission},
    status,
};
use iroh::EndpointId;
use std::io::{self, Write};

enum Command {
    Agent,
    #[cfg(windows)]
    AgentLog,
    AgentInstall,
    AgentUninstall,
    AgentReload,
    Via {
        peer: String,
        line: String,
    },
    Id,
    Allow(Peer),
    Revoke(String),
    Peers,
    Status,
    Pending,
    Accept {
        name: String,
        fingerprint: String,
        permission: Permission,
    },
    Listen(Option<String>),
    Connect {
        name: String,
        invite: Option<String>,
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
        [cmd, action] if cmd == "agent" && action == "reload" => Ok(Command::AgentReload),
        [cmd] if cmd == "id" => Ok(Command::Id),
        [cmd] if cmd == "listen" => Ok(Command::Listen(None)),
        [cmd, name] if cmd == "listen" && peers::valid_name(name) => {
            Ok(Command::Listen(Some(name.clone())))
        }
        [cmd, name] if cmd == "connect" && peers::valid_name(name) => Ok(Command::Connect {
            name: name.clone(),
            invite: None,
        }),
        [cmd, name, invite] if cmd == "connect" && peers::valid_name(name) => {
            Ok(Command::Connect {
                name: name.clone(),
                invite: Some(invite.clone()),
            })
        }
        [cmd] if cmd == "peers" => Ok(Command::Peers),
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
                "usage: herdvia agent [install|uninstall|reload|--log] | listen [<name>] | connect <name> [<invite>] | via <peer> <line> | id | allow <name> <EndpointId> (--inbound|--outbound|--both) | revoke <name> | peers | status | pending | accept <name> <fingerprint> (--inbound|--both) | doctor | --version"
            } else {
                "usage: herdvia agent [install|uninstall|reload] | listen [<name>] | connect <name> [<invite>] | via <peer> <line> | id | allow <name> <EndpointId> (--inbound|--outbound|--both) | revoke <name> | peers | status | pending | accept <name> <fingerprint> (--inbound|--both) | doctor | --version"
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
        Ok(Command::Listen(name)) => herdvia::pair::cli::run_listen(name).await,
        Ok(Command::Connect { name, invite }) => {
            herdvia::pair::cli::run_connect(name, invite).await
        }
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
        Command::AgentReload => {
            if !control::reload(&dir).await? {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "agent is not running; start it with 'herdvia agent install' (or 'herdvia agent' in a terminal)",
                ));
            }
            println!("reloaded: peers re-read; waiting connections retry now");
        }
        Command::Allow(peer) => {
            let replaced = peers::allow(&dir, peer.clone())?;
            let reloaded = control::reload_if_running(&dir).await;
            if reloaded.is_ok() {
                println!("{} {} {}", peer.name, peer.id, peer.permission.as_str());
            }
            if let Some(old) = replaced {
                eprintln!(
                    "herdvia: warning: {} now points to a different key (fingerprint {} -> {})",
                    peer.name,
                    peers::fingerprint(old.id),
                    peers::fingerprint(peer.id)
                );
            }
            reloaded.map_err(|err| not_confirmed("saved", &peer.name, &err, ""))?;
        }
        Command::Revoke(name) => {
            let (peer, others) = peers::revoke(&dir, &name)?;
            let reloaded = control::reload_if_running(&dir).await;
            if reloaded.is_ok() {
                println!("revoked {name} {}", peer.id);
            }
            if !others.is_empty() {
                eprintln!(
                    "herdvia: warning: {} is still allowed as peer {}; revoke each of those names to remove it",
                    peer.id,
                    others.join(", ")
                );
            }
            reloaded.map_err(|err| {
                let risk = format!(", so it may still admit {name} until it reloads");
                not_confirmed("revoked", &name, &err, &risk)
            })?;
            // What revoke leaves behind elsewhere.
            if peer.permission.outbound() {
                eprintln!(
                    "herdvia: note: Herdr may still list {name}; remove it with: hc machine rm {name}"
                );
            }
            eprintln!(
                "herdvia: note: on that PC, also revoke this PC (herdvia revoke <the name it uses for this PC>)"
            );
        }
        Command::Peers => print!("{}", peer_list(&peers::load(&dir)?)),
        Command::Status => {
            let mut old_agent = false;
            let (status, reasons) = match control::request(&dir, "status-detail").await {
                Ok(reply) => match status::parse(&reply) {
                    Some(rows) => (status::table(&rows), status_reasons(&rows)),
                    // A newer agent's format: restarting it would not help.
                    None => (control::request(&dir, "status").await?, String::new()),
                },
                Err(err) if rejects_status_detail(&err) => {
                    old_agent = true;
                    (control::request(&dir, "status").await?, String::new())
                }
                Err(err) => return Err(err),
            };
            print!("{status}{reasons}");
            if old_agent {
                eprintln!(
                    "herdvia: note: restart the agent (herdvia agent install) to see why peers are disconnected"
                );
            }
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
            control::reload_if_running(&dir)
                .await
                .map_err(|err| not_confirmed("saved", &name, &err, ""))?;
            println!("{name} {id} {}", permission.as_str());
        }
        _ => unreachable!("agent, via and version are handled separately"),
    }
    Ok(())
}

/// The error for a `peers` change that a running agent did not confirm reloading.
fn not_confirmed(done: &str, name: &str, cause: &io::Error, risk: &str) -> io::Error {
    io::Error::other(format!(
        "{done} {name} in the peers file, but the running agent did not confirm it ({cause}){risk}; run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)"
    ))
}

/// The saved peers as `herdvia peers` prints them; it says nothing about live connections.
fn peer_list(list: &[Peer]) -> String {
    if list.is_empty() {
        return "no peers registered\n".into();
    }
    let mut text = String::from("name permission fingerprint\n");
    for peer in list {
        text += &format!(
            "{} {} {}\n",
            peer.name,
            peer.permission.as_str(),
            peers::fingerprint(peer.id)
        );
    }
    text + "in = may open sessions on this PC; out = this PC may connect to it\n"
}

/// What `herdvia status` prints below the table: a line for each peer that is not connected.
fn status_reasons(rows: &[status::Row]) -> String {
    let lines: Vec<String> = rows.iter().filter_map(status::human).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut text = format!("\n{}\n", lines.join("\n"));
    if rows
        .iter()
        .any(|row| row.state != status::State::Connected && row.retry_secs.is_some())
    {
        text.push_str("Run herdvia agent reload to retry now.\n");
    }
    text
}

/// Whether the agent turned `status-detail` down because it predates it. This and later agents
/// answer ERROR `unknown control request`; v0.1.4 and earlier drop the connection without a reply
/// when the frame layer sees a CONTROL request it does not know.
fn rejects_status_detail(err: &io::Error) -> bool {
    use io::ErrorKind::*;
    // On Windows the old agent closes its end of the pipe: reading it is end of file (agent
    // closed without EXIT), and writing to it fails with a pipe error, checked by code because
    // std does not promise to map each of them to BrokenPipe.
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
        };
        if [ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED]
            .iter()
            .any(|&code| err.raw_os_error() == Some(code as i32))
        {
            return true;
        }
    }
    match err.kind() {
        ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof => true,
        InvalidData => err.to_string() == "agent closed without EXIT",
        Other => err.to_string() == "unknown control request",
        _ => false,
    }
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
        assert!(matches!(
            parse(vec!["agent".into(), "reload".into()]).unwrap(),
            Command::AgentReload
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
            vec!["agent", "reload", "extra"],
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
    fn peers_takes_no_arguments_and_lists_saved_peers() {
        assert!(matches!(parse(args(&["peers"])).unwrap(), Command::Peers));
        for a in [&["peers", "extra"][..], &["Peers"]] {
            assert!(parse(args(a)).is_err(), "{a:?}");
        }
        assert_eq!(peer_list(&[]), "no peers registered\n");
        let id = iroh::SecretKey::generate().public();
        let list = [
            Peer {
                name: "win1".into(),
                id,
                permission: Permission::Both,
            },
            Peer {
                name: "laptop".into(),
                id,
                permission: Permission::In,
            },
        ];
        let fp = peers::fingerprint(id);
        assert_eq!(
            peer_list(&list),
            format!(
                "name permission fingerprint\nwin1 both {fp}\nlaptop in {fp}\nin = may open sessions on this PC; out = this PC may connect to it\n"
            )
        );
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
    fn status_reasons_follow_the_table_only_for_peers_that_are_not_connected() {
        let rows = |detail: &str| status::parse(&format!("{}\n{detail}", status::HEADER)).unwrap();
        assert_eq!(
            status_reasons(&rows("home both connected direct 1.25ms - -\n")),
            ""
        );
        assert_eq!(
            status_reasons(&rows(
                "home both connected direct 1.25ms - -\nwin1 out disconnected - - closed - reset\nlaptop in waiting - - - -\n"
            )),
            "\nwin1: the connection closed (reset).\nlaptop: waiting for laptop to connect (this PC only accepts it).\n"
        );
        assert_eq!(
            status_reasons(&rows(
                "win1 out disconnected - - rejected 12 403 peer not allowed\nnew both disconnected - - - 1\n"
            )),
            "\nwin1: rejected by the peer (403 peer not allowed); on win1 run herdvia pending, then herdvia accept. Retrying in 12s.\nnew: not connected yet. Retrying in 1s.\nRun herdvia agent reload to retry now.\n"
        );
    }

    #[test]
    fn status_detail_counts_as_rejected_only_when_the_agent_does_not_know_it() {
        use io::ErrorKind::*;
        for err in [
            io::Error::other("unknown control request"),
            io::Error::new(InvalidData, "agent closed without EXIT"),
            io::Error::from(ConnectionReset),
            io::Error::from(ConnectionAborted),
            io::Error::from(BrokenPipe),
            io::Error::from(UnexpectedEof),
        ] {
            assert!(rejects_status_detail(&err), "{err:?}");
        }
        for err in [
            io::Error::new(
                TimedOut,
                "the agent did not answer the status-detail request within 5 seconds",
            ),
            io::Error::from(NotFound),
            io::Error::from(ConnectionRefused),
            io::Error::other("invalid peers file"),
            io::Error::new(InvalidData, "unexpected control response"),
        ] {
            assert!(!rejects_status_detail(&err), "{err:?}");
        }
        #[cfg(windows)]
        assert!(rejects_status_detail(&io::Error::from_raw_os_error(
            windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED as i32
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

    #[test]
    fn parses_listen_and_connect_commands() {
        match parse(args(&["listen"])).unwrap() {
            Command::Listen(None) => {}
            _ => panic!("expected listen without name"),
        }
        match parse(args(&["listen", "laptop"])).unwrap() {
            Command::Listen(Some(name)) => assert_eq!(name, "laptop"),
            _ => panic!("expected listen with name"),
        }
        for bad in [&["listen", "bad/name"][..], &["listen", "a", "b"]] {
            assert!(parse(args(bad)).is_err(), "{bad:?}");
        }

        match parse(args(&["connect", "home"])).unwrap() {
            Command::Connect { name, invite } => {
                assert_eq!(name, "home");
                assert_eq!(invite, None);
            }
            _ => panic!("expected connect without invite"),
        }
        match parse(args(&["connect", "home", "hvi1-abc"])).unwrap() {
            Command::Connect { name, invite } => {
                assert_eq!(name, "home");
                assert_eq!(invite.as_deref(), Some("hvi1-abc"));
            }
            _ => panic!("expected connect with invite"),
        }
        for bad in [
            &["connect"][..],
            &["connect", "bad/name"],
            &["connect", "a", "b", "c"],
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?}");
        }
    }
}
