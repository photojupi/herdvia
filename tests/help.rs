//! `herdvia --help` 와 `herdvia <command> --help`: 데이터 폴더나 에이전트에 손대지 않고 도움말만 낸다.

#![cfg(unix)]

mod common;

use common::*;
use std::fs;
use std::time::{Duration, Instant};

const COMMANDS: [&str; 13] = [
    "agent", "listen", "connect", "register", "peers", "id", "allow", "revoke", "status",
    "pending", "accept", "doctor", "via",
];

#[test]
fn help_prints_to_stdout_and_creates_nothing() {
    let n = Node::new("help");
    let missing = n.root.join("none");
    let mut asks: Vec<Vec<&str>> = vec![vec!["--help"], vec!["-h"], vec!["help"]];
    for cmd in COMMANDS {
        asks.push(vec![cmd, "--help"]);
        asks.push(vec![cmd, "-h"]);
    }
    for dir in [&n.herdvia_dir, &missing] {
        for args in &asks {
            let mut c = n.cmd(args);
            c.env("HERDVIA_DIR", dir);
            let o = try_run(c, Duration::from_secs(10))
                .unwrap_or_else(|| panic!("{args:?} 가 10초 안에 끝나지 않음"));
            assert_eq!(o.code(), Some(0), "{args:?} stderr: {}", o.err());
            assert!(o.stderr.is_empty(), "{args:?} stderr: {}", o.err());
            let want = match args.as_slice() {
                [_] => "herdvia carries Herdr sessions".to_owned(),
                [cmd, _] => format!("usage: herdvia {cmd}"),
                _ => unreachable!(),
            };
            assert!(o.out().starts_with(&want), "{args:?} stdout: {}", o.out());
        }
    }
    assert!(!missing.exists(), "도움말이 데이터 폴더를 만듦");
    assert_eq!(
        fs::read_dir(&n.herdvia_dir).unwrap().count(),
        0,
        "도움말이 데이터 폴더에 파일을 만듦"
    );
}

#[test]
fn listen_help_prints_help_instead_of_listening() {
    let mut n = Node::new("help-listen");
    n.start_agent();
    // --help 와 -h 는 올바른 피어 이름이기도 하다. 예전에는 그 이름으로 5분간 기다렸다.
    for args in [&["listen", "--help"][..], &["listen", "-h"]] {
        let started = Instant::now();
        let o = n.herdvia(args);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{args:?} 가 늦게 끝남: {:?}",
            started.elapsed()
        );
        assert_eq!(o.code(), Some(0), "{args:?} stderr: {}", o.err());
        assert!(
            o.out().starts_with("usage: herdvia listen [<name>]\n"),
            "{args:?} stdout: {}",
            o.out()
        );
        assert!(!o.out().contains("Waiting for a connection"), "{}", o.out());
    }
    assert!(!n.peers_file().exists(), "listen --help 가 피어를 저장함");
}
