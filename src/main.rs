use std::io;

enum Command {
    Agent,
    Via { peer: String, line: String },
}

fn parse(args: Vec<String>) -> io::Result<Command> {
    match args.as_slice() {
        [cmd] if cmd == "agent" => Ok(Command::Agent),
        [cmd, peer, line] if cmd == "via" => Ok(Command::Via {
            peer: peer.clone(),
            line: line.clone(),
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: hlink agent | hlink via <peer> <line>",
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
    let result = match args.and_then(parse) {
        Ok(Command::Agent) => hlink::agent::run().await.map(|()| 0),
        Ok(Command::Via { peer, line }) => hlink::via::run(&peer, &line).await,
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
            vec!["id"],
            vec!["agent", "install"],
            vec!["via", "peer"],
            vec!["via", "p", "l", "extra"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }
}
