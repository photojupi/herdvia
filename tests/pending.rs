//! 받는 쪽이 접속을 시도한 PC 목록(pending)을 보고 지문으로 수락(accept)하는 흐름.
//!
//! A 는 B 를 outbound 로 등록하고, B 는 A 를 모른다. B 가 거부한 A 의 시도가 B 의 pending 에 보이고,
//! B 가 지문으로 accept 하면 A 의 다음 재시도(최대 30초 간격)에 연결된다.

#![cfg(unix)]

mod common;

use common::*;
use std::thread;
use std::time::{Duration, Instant};

const HEADER: &str = "fingerprint id attempts last-seen\n";

/// `node` 의 pending 에 `id` 줄이 나타날 때까지 기다리고 그 줄의 칸들을 돌려준다.
fn wait_pending(node: &Node, id: &str, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let o = node.herdvia(&["pending"]);
        assert_eq!(
            o.code(),
            Some(0),
            "[{}] pending 실패: {}",
            node.label,
            o.err()
        );
        let out = o.out();
        assert!(out.starts_with(HEADER), "pending 머리 줄: {out:?}");
        if let Some(line) = out
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some(id))
        {
            return line.split_whitespace().map(str::to_owned).collect();
        }
        if Instant::now() > deadline {
            panic!(
                "[{}] {timeout:?} 안에 pending 에 {id} 가 보이지 않음. 마지막 출력:\n{out}\n로그:\n{}",
                node.label,
                node.agent_log()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn pending_out(node: &Node) -> String {
    let o = node.herdvia(&["pending"]);
    assert_eq!(
        o.code(),
        Some(0),
        "[{}] pending 실패: {}",
        node.label,
        o.err()
    );
    o.out()
}

/// A: b=outbound, B: 피어 없음(또는 `b_peers` 만) 으로 띄운다.
fn caller_and_stranger(b_peers: &[(&str, &str, &str)]) -> (Node, Node, String) {
    let mut a = Node::new("A");
    let mut b = Node::new("B");
    let (a_id, _) = a.id();
    let (b_id, _) = b.id();
    a.allow("b", &b_id, "--outbound");
    for (name, id, flag) in b_peers {
        b.allow(name, id, flag);
    }
    b.start_agent();
    a.start_agent();
    (a, b, a_id)
}

#[test]
fn accept_registers_the_pending_caller_and_it_connects() {
    let n = Node::new("X");
    let o = n.herdvia(&["pending"]);
    assert_eq!(o.code(), Some(255), "에이전트 없이 pending: {}", o.err());
    assert_eq!(o.err().trim().lines().count(), 1, "stderr: {:?}", o.err());

    let (a, b, a_id) = caller_and_stranger(&[]);
    let fields = wait_pending(&b, &a_id, Duration::from_secs(30));
    assert_eq!(fields.len(), 4, "pending 줄: {fields:?}");
    assert_eq!(fields[0], fingerprint_of(&a_id));
    assert!(
        fields[2].parse::<u64>().is_ok_and(|n| n >= 1),
        "시도 횟수: {fields:?}"
    );
    assert!(
        fields[3]
            .strip_suffix('s')
            .is_some_and(|s| s.parse::<u64>().is_ok()),
        "마지막으로 본 시각: {fields:?}"
    );
    assert_eq!(b.invoked(), None, "수락 전인데 A 의 줄이 B 에서 실행됨");

    // 다른 화면을 보고 손으로 치는 값이므로 대문자·대시 없음도 받는다.
    let typed = fingerprint_of(&a_id).replace('-', "").to_uppercase();
    let o = b.herdvia(&["accept", "a", &typed, "--inbound"]);
    assert_eq!(o.code(), Some(0), "accept 실패: {}", o.err());
    assert_eq!(o.out(), format!("a {a_id} in\n"));
    assert!(
        has_peer_line(&b.peers_text(), "a", &a_id, "in"),
        "peers:\n{}",
        b.peers_text()
    );
    assert_eq!(pending_out(&b), HEADER, "수락한 피어가 pending 에 남음");

    // A 는 거부당하는 동안 간격을 늘려 왔으므로 다음 재시도까지 최대 30초가 걸릴 수 있다.
    // A 의 status 는 B 가 막 거부한 연결도 잠깐 connected 로 보일 수 있으므로, 받은 쪽인 B 의 status 로 본다.
    wait_connected(&b, "a", None, Duration::from_secs(45));
    let out = a.run_via("b", &session_line("cat"), b"accepted");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"accepted");
    assert_eq!(pending_out(&b), HEADER, "연결 뒤 pending 에 다시 보임");
}

#[test]
fn accept_refuses_a_name_registered_with_another_endpoint() {
    let other = Node::new("X").id().0;
    let (_a, b, a_id) = caller_and_stranger(&[("a", &other, "--inbound")]);
    let fp = fingerprint_of(&a_id);
    wait_pending(&b, &a_id, Duration::from_secs(30));

    let o = b.herdvia(&["accept", "a", &fp, "--both"]);
    assert_eq!(o.code(), Some(255), "다른 ID 의 이름을 덮어씀: {}", o.out());
    assert!(
        o.err().contains("already registered"),
        "stderr: {}",
        o.err()
    );
    assert!(o.out().is_empty(), "stdout: {:?}", o.out());
    let peers = b.peers_text();
    assert!(has_peer_line(&peers, "a", &other, "in"), "peers:\n{peers}");
    assert!(!peers.contains(&a_id), "peers:\n{peers}");

    // 지문이 맞는 시도가 없으면 상대 쪽 설정을 확인하라고 알린다.
    let o = b.herdvia(&["accept", "c", "0000-0000-0000-0000", "--both"]);
    assert_eq!(o.code(), Some(255));
    assert!(o.err().contains("--outbound"), "stderr: {}", o.err());
    assert!(!has_peer_name(&b.peers_text(), "c"));

    // 다른 이름으로는 받는다.
    let o = b.herdvia(&["accept", "a2", &fp, "--both"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.out(), format!("a2 {a_id} both\n"));
    assert_eq!(pending_out(&b), HEADER);
}

#[test]
fn accept_refuses_an_endpoint_id_registered_under_another_name() {
    // B 는 A 를 outbound 로만 알고 있다. A 의 시도는 거부되어 pending 에 보인다.
    let mut a = Node::new("A");
    let mut b = Node::new("B");
    let (a_id, _) = a.id();
    let (b_id, _) = b.id();
    a.allow("b", &b_id, "--outbound");
    b.allow("z", &a_id, "--outbound");
    b.start_agent();
    a.start_agent();
    let fp = fingerprint_of(&a_id);
    wait_pending(&b, &a_id, Duration::from_secs(30));

    let before = b.peers_text();
    let o = b.herdvia(&["accept", "a", &fp, "--inbound"]);
    assert_eq!(
        o.code(),
        Some(255),
        "같은 EndpointId 를 다른 이름으로 받아들임"
    );
    assert!(
        o.err().contains("already registered as peer z;"),
        "stderr: {}",
        o.err()
    );
    assert!(o.out().is_empty(), "stdout: {:?}", o.out());
    assert_eq!(b.peers_text(), before);

    // 그 이름으로는 받는다 (권한 변경).
    let o = b.herdvia(&["accept", "z", &fp, "--both"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.out(), format!("z {a_id} both\n"));
    assert_eq!(pending_out(&b), HEADER);
}
