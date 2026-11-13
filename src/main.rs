use hlink::{
    control, ipc,
    peers::{self, Peer, Permission},
};
use std::io;

enum Command {
    Agent,
    Via { peer: String, line: String },
    Id,
    Allow(Peer),
    Revoke(String),
    Status,
}

fn parse(args: Vec<String>) -> io::Result<Command> {
    match args.as_slice() {
        [cmd] if cmd == "agent" => Ok(Command::Agent),
        [cmd] if cmd == "id" => Ok(Command::Id),
        [cmd] if cmd == "status" => Ok(Command::Status),
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
        [cmd, peer, line] if cmd == "via" => Ok(Command::Via {
            peer: peer.clone(),
            line: line.clone(),
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: hlink agent | via <peer> <line> | id | allow <name> <EndpointId> (--inbound|--outbound|--both) | revoke <name> | status",
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
        Ok(Command::Agent) => hlink::agent::run().await.map(|()| 0),
        Ok(Command::Via { peer, line }) => hlink::via::run(&peer, &line).await,
        Ok(command) => manage(command).await.map(|()| 0),
        Err(err) => Err(err),
    };
    let code = match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("hlink: {err}");
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
            peers::allow(&dir, peer)?;
            control::reload_if_running(&dir).await?;
        }
        Command::Revoke(name) => {
            peers::revoke(&dir, &name)?;
            control::reload_if_running(&dir).await?;
        }
        Command::Status => print!("{}", control::request(&dir, "status").await?),
        _ => unreachable!("agent and via are handled separately"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_agent_and_via_with_exact_argument_counts() {
        assert!(matches!(
            parse(vec!["agent".into()]).unwrap(),
            Command::Agent
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
            vec!["agent", "install"],
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
}
