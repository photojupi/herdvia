//! M3 고유 검증: 피어 키 허용 목록, 방향별 권한, id/allow/revoke/status, 시작 거부, 받는 쪽 부재.
//!
//! 공유 쌍(A: b=out, B: a=in)은 피어 목록을 바꾸지 않는 검증에만 쓴다.
//! 피어 목록을 바꾸거나 에이전트를 죽이는 검증은 새 쌍을 띄운다.

#![cfg(unix)]

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

fn chmod(p: &std::path::Path, mode: u32) {
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

/// 피어 목록에서 그 피어를 지우거나 권한을 잃게 만든 뒤 공통으로 볼 것:
/// 실행 중이던 원격 자식이 곧 정리되고, via 는 255 로 끝나고, 이후 via 는 거부되고 실행되지 않는다.
fn assert_running_child_cleaned_and_later_refused(
    p: &Pair,
    v: Via,
    child: &str,
    grandchild: &str,
    what: &str,
) {
    let t = Instant::now();
    let out = v.wait_end_keep_stdin(Duration::from_secs(15));
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "[{what}] 권한을 잃은 뒤 via 가 늦게 끝남: {:?}",
        t.elapsed()
    );
    assert_eq!(out.code(), Some(255), "[{what}] stderr: {}", out.err());
    assert!(
        out.stdout.is_empty(),
        "[{what}] stdout 에 원격 외 바이트: {:?}",
        out.stdout
    );
    wait_gone(&[child, grandchild], Duration::from_secs(5), what);

    // 이후 via 는 거부되고 B 에서 실행되지 않는다.
    p.b.clear_state();
    let out = p.a.run_via("b", &session_line("cat"), b"must not run");
    assert_eq!(
        out.code(),
        Some(255),
        "[{what}] 이후 via 가 거부되지 않음. stderr: {}",
        out.err()
    );
    assert!(
        !out.err().trim().is_empty(),
        "[{what}] 이후 via 의 stderr 에 사유가 없음"
    );
    assert!(
        out.stdout.is_empty(),
        "[{what}] 이후 via stdout: {:?}",
        out.stdout
    );
    assert_eq!(
        p.b.invoked(),
        None,
        "[{what}] 권한을 잃은 피어의 줄이 B 에서 실행됨"
    );
}

fn start_sleep(p: &Pair) -> (Via, String, String) {
    let mut v = p.a.spawn_via("b", &session_line("sleep"));
    assert_eq!(v.read_stdout_exact(6, Duration::from_secs(10)), b"ready\n");
    let child = p.b.read_pid("child.pid");
    let grandchild = p.b.read_pid("grandchild.pid");
    assert!(
        process_alive(&child) && process_alive(&grandchild),
        "원격 프로세스가 처음부터 없음"
    );
    (v, child, grandchild)
}

// ---------------------------------------------------------------------------
// 접속 허용 목록과 방향
// ---------------------------------------------------------------------------

#[test]
fn unregistered_endpoint_is_refused_by_receiver() {
    // 세 번째 에이전트 C 가 B 를 outbound 로 등록해도 B 는 C 를 모르므로 실행하지 않는다.
    let p = shared();
    let mut c = Node::new("C");
    let (_c_id, _) = c.id();
    c.allow("b", &p.b_id, "--outbound");
    c.start_agent();

    let t = Instant::now();
    let out = c.run_via("b", &session_line("cat"), b"from C");
    assert_eq!(
        out.code(),
        Some(255),
        "C 의 via 가 거부되지 않음. stderr: {}",
        out.err()
    );
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "C 의 via 가 너무 오래 걸림: {:?}",
        t.elapsed()
    );
    assert!(!out.err().trim().is_empty(), "stderr 에 사유가 없음");
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert_eq!(
        p.b.invoked(),
        None,
        "등록되지 않은 피어의 줄이 B 에서 실행됨"
    );
    assert_eq!(c.invoked(), None, "C 자기 자신에서 실행됨");

    // B 는 여전히 A 를 받는다.
    let out = p.a.run_via("b", &session_line("cat"), b"A ok");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"A ok");
}

#[test]
fn inbound_only_peer_cannot_be_called_back() {
    // B 는 A 를 inbound 로만 받았다. B 에서 A 로 걸면(B→A) 거부된다. A 에서 실행되지 않는다.
    let p = shared();
    let t = Instant::now();
    let out = p.b.run_via("a", &session_line("cat"), b"reverse");
    assert_eq!(
        out.code(),
        Some(255),
        "B→A 가 거부되지 않음. stderr: {}",
        out.err()
    );
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "로컬에서 바로 거부해야 함: {:?}",
        t.elapsed()
    );
    assert!(!out.err().trim().is_empty());
    assert!(out.stdout.is_empty());
    assert_eq!(p.a.invoked(), None, "B→A 줄이 A 에서 실행됨");
    assert_eq!(p.b.invoked(), None, "B→A 줄이 B 자기 자신에서 실행됨");
}

#[test]
fn receiver_refuses_peer_it_granted_only_outbound() {
    // B 는 A 를 both 로 받아 A 로 걸 수 있다고 믿지만, A 는 B 를 outbound 로만 등록했다.
    // A 는 B 의 연결·스트림을 받아 실행하면 안 된다 (A 가 B 로 건 연결 위에 B 가 스트림을 열어도 마찬가지).
    let p = Pair::build("--outbound", "--both", false);
    let t = Instant::now();
    let out = p.b.run_via("a", &session_line("cat"), b"reverse");
    assert_eq!(
        out.code(),
        Some(255),
        "A 가 inbound 없이 받음. stderr: {}",
        out.err()
    );
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "너무 오래 걸림: {:?}",
        t.elapsed()
    );
    assert!(!out.err().trim().is_empty());
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert_eq!(
        p.a.invoked(),
        None,
        "inbound 권한 없는 B 의 줄이 A 에서 실행됨"
    );

    // A→B 방향은 그대로 된다.
    let out = p.a.run_via("b", &session_line("cat"), b"forward");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"forward");
}

#[test]
fn via_to_unknown_peer_name_is_255() {
    let p = shared();
    for name in ["nosuch", "B", "a", "b2", "", "b "] {
        let t = Instant::now();
        let out = p.a.run_via(name, &session_line("cat"), b"x");
        assert_eq!(
            out.code(),
            Some(255),
            "[{name:?}] 거부되어야 함. stderr: {}",
            out.err()
        );
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "[{name:?}] 바로 거부해야 함: {:?}",
            t.elapsed()
        );
        assert!(
            !out.err().trim().is_empty(),
            "[{name:?}] stderr 에 사유가 없음"
        );
        assert!(out.stdout.is_empty(), "[{name:?}] stdout: {:?}", out.stdout);
    }
    assert_eq!(p.b.invoked(), None, "없는 피어 이름인데 B 에서 실행됨");
    assert_eq!(p.a.invoked(), None, "없는 피어 이름인데 A 에서 실행됨");
}

// ---------------------------------------------------------------------------
// revoke 와 권한 변경의 즉시 반영
// ---------------------------------------------------------------------------

#[test]
fn revoke_on_receiver_kills_running_child_and_refuses_later() {
    let p = Pair::new();
    let (v, child, grandchild) = start_sleep(&p);
    p.b.revoke("a");
    assert!(
        !has_peer_name(&p.b.peers_text(), "a"),
        "B 의 peers 에 a 가 남음"
    );
    assert_running_child_cleaned_and_later_refused(&p, v, &child, &grandchild, "B 에서 revoke a");
}

#[test]
fn revoke_on_caller_kills_running_child_and_refuses_later() {
    let p = Pair::new();
    let (v, child, grandchild) = start_sleep(&p);
    p.a.revoke("b");
    assert!(
        !has_peer_name(&p.a.peers_text(), "b"),
        "A 의 peers 에 b 가 남음"
    );
    assert_running_child_cleaned_and_later_refused(&p, v, &child, &grandchild, "A 에서 revoke b");
    // A 에서는 이제 모르는 이름이므로 바로 거부된다.
    let t = Instant::now();
    let out = p.a.run_via("b", DEFAULT_LINE, b"");
    assert_eq!(out.code(), Some(255));
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "지운 피어는 바로 거부해야 함: {:?}",
        t.elapsed()
    );
}

#[test]
fn downgrade_inbound_to_outbound_takes_effect_immediately() {
    // B 가 A 의 권한을 in 에서 out 으로 바꾸면 A 는 더 이상 B 에서 실행할 수 없다.
    let p = Pair::new();
    let (v, child, grandchild) = start_sleep(&p);
    p.b.allow("a", &p.a_id, "--outbound");
    let peers = p.b.peers_text();
    assert!(
        has_peer_line(&peers, "a", &p.a_id, "out"),
        "B 의 peers 가 바뀌지 않음:\n{peers}"
    );
    assert_eq!(
        peers.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "중복 줄:\n{peers}"
    );
    assert_running_child_cleaned_and_later_refused(
        &p,
        v,
        &child,
        &grandchild,
        "B 에서 a 를 in→out",
    );

    // A 쪽도 b 를 out 에서 in 으로 바꾸면 A 에서 b 로 거는 것이 바로 거부된다.
    p.a.allow("b", &p.b_id, "--inbound");
    let t = Instant::now();
    let out = p.a.run_via("b", DEFAULT_LINE, b"");
    assert_eq!(out.code(), Some(255), "stderr: {}", out.err());
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "outbound 가 없는 피어는 바로 거부해야 함: {:?}",
        t.elapsed()
    );
}

#[test]
fn upgrade_after_start_is_picked_up_without_restart() {
    // 에이전트가 뜬 뒤 allow 한 피어도 재시작 없이 쓰인다 (allow 의 즉시 반영).
    let mut a = Node::new("A");
    let mut b = Node::new("B");
    let (a_id, _) = a.id();
    let (b_id, _) = b.id();
    a.start_agent();
    b.start_agent();
    // 아직 아무도 등록하지 않았다.
    let out = a.run_via("b", DEFAULT_LINE, b"");
    assert_eq!(out.code(), Some(255));
    b.allow("a", &a_id, "--inbound");
    a.allow("b", &b_id, "--outbound");
    wait_connected(&a, "b", Some(&b), Duration::from_secs(45));
    let out = a.run_via("b", &session_line("cat"), b"late allow");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"late allow");
}

// ---------------------------------------------------------------------------
// 키·피어 파일 권한이 넓으면 시작 거부
// ---------------------------------------------------------------------------

#[test]
fn wide_secret_key_permission_refuses_start() {
    let mut n = Node::new("A");
    let (id, _) = n.id();
    for mode in [0o644, 0o640, 0o604] {
        chmod(&n.key_file(), mode);
        let o = n.herdvia(&["id"]);
        assert_ne!(
            o.code(),
            Some(0),
            "[{mode:o}] id 가 넓은 키 권한을 받아들임"
        );
        assert!(!o.out().contains(&id), "[{mode:o}] 거부하면서 id 를 출력함");
        assert!(!o.err().trim().is_empty(), "[{mode:o}] id 거부 사유가 없음");
        expect_agent_refused(&n, &format!("secret.key {mode:o}"));
    }
    // 대조군: 0600 으로 돌리면 같은 키로 시작한다.
    chmod(&n.key_file(), 0o600);
    assert_eq!(n.id().0, id, "권한을 고친 뒤 id 가 바뀜");
    n.start_agent();
}

#[test]
fn wide_peers_permission_refuses_start() {
    let mut n = Node::new("A");
    let _ = n.id();
    let other = Node::new("X");
    let (other_id, _) = other.id();
    n.allow("x", &other_id, "--both");
    assert_eq!(mode_of(&n.peers_file()), 0o600, "allow 가 만든 peers 권한");
    for mode in [0o644, 0o640] {
        chmod(&n.peers_file(), mode);
        expect_agent_refused(&n, &format!("peers {mode:o}"));
    }
    chmod(&n.peers_file(), 0o600);
    n.start_agent();
}

// ---------------------------------------------------------------------------
// id 출력 형식
// ---------------------------------------------------------------------------

#[test]
fn id_prints_endpoint_id_and_fingerprint_stably() {
    let mut n = Node::new("A");
    n.herdvia_dir = n.root.join("fresh");
    assert!(!n.herdvia_dir.exists());

    let o1 = n.herdvia(&["id"]);
    assert_eq!(o1.code(), Some(0), "stderr: {}", o1.err());
    let s = o1.out();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 2, "id 출력은 두 줄이어야 함: {s:?}");
    let id = lines[0];
    assert_eq!(id.len(), 64, "EndpointId 는 hex 64자: {id:?}");
    assert!(
        id.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "EndpointId 는 소문자 hex: {id:?}"
    );
    assert_eq!(
        lines[1],
        fingerprint_of(id),
        "둘째 줄은 앞 16자를 4-4-4-4 로 묶은 지문"
    );

    assert_eq!(mode_of(&n.key_file()), 0o600, "id 가 만든 secret.key 권한");
    assert_eq!(
        mode_of(&n.herdvia_dir),
        0o700,
        "id 가 만든 데이터 폴더 권한"
    );

    let o2 = n.herdvia(&["id"]);
    assert_eq!(o2.code(), Some(0));
    assert_eq!(o2.stdout, o1.stdout, "다시 실행하면 같은 값이어야 함");

    // 에이전트가 떠 있어도 같은 값이고, 다른 노드와는 다르다.
    n.start_agent();
    assert_eq!(
        n.herdvia(&["id"]).stdout,
        o1.stdout,
        "에이전트가 뜬 뒤 id 가 바뀜"
    );
    let other = Node::new("X");
    assert_ne!(
        other.id().0,
        id,
        "서로 다른 데이터 폴더인데 EndpointId 가 같음"
    );
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_shows_name_connection_and_path() {
    let p = shared();
    for (node, peer, perm_word) in [(&p.a, "b", "out"), (&p.b, "a", "in")] {
        let o = node.status();
        assert_eq!(
            o.code(),
            Some(0),
            "[{}] status 실패. stderr: {}",
            node.label,
            o.err()
        );
        let s = o.out();
        let line = status_line(&s, peer)
            .unwrap_or_else(|| panic!("[{}] status 에 피어 {peer} 줄이 없음:\n{s}", node.label));
        assert!(
            says_connected(line),
            "[{}] status 가 연결됨을 보이지 않음: {line:?}",
            node.label
        );
        let lower = line.to_lowercase();
        assert!(
            PATH_WORDS.iter().any(|w| lower.contains(w)),
            "[{}] status 에 직접/중계 구분이 없음: {line:?}",
            node.label
        );
        assert!(
            lower.contains(perm_word),
            "[{}] status 에 권한({perm_word})이 없음: {line:?}",
            node.label
        );
        assert!(
            line.chars().any(|c| c.is_ascii_digit()),
            "[{}] status 에 왕복 시간이 없음: {line:?}",
            node.label
        );
    }
}

#[test]
fn status_without_agent_is_255() {
    let n = Node::new("A");
    let t = Instant::now();
    let o = n.status();
    assert_eq!(o.code(), Some(255), "stderr: {}", o.err());
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "에이전트가 없으면 기다리지 않아야 함"
    );
    assert!(!o.err().trim().is_empty(), "stderr 에 사유가 없음");
    assert_eq!(
        o.err().trim().lines().count(),
        1,
        "stderr 는 한 줄: {:?}",
        o.err()
    );
    assert!(o.stdout.is_empty(), "stdout: {:?}", o.out());
}

// ---------------------------------------------------------------------------
// allow / revoke 인자 검사와 파일 반영
// ---------------------------------------------------------------------------

#[test]
fn allow_rejects_bad_name_bad_id_and_missing_flag() {
    let n = Node::new("A");
    let _ = n.id();
    let ids: Vec<String> = (0..3).map(|i| Node::new(&format!("X{i}")).id().0).collect();
    let good = ids[0].as_str();

    let too_long = "x".repeat(33);
    for name in ["", "a b", "a/b", "한글", too_long.as_str(), "a;b", "a\nb"] {
        let o = n.herdvia(&["allow", name, good, "--both"]);
        assert_ne!(o.code(), Some(0), "잘못된 이름 {name:?} 을 받아들임");
        assert!(!o.err().trim().is_empty(), "[{name:?}] 거부 사유가 없음");
        assert!(
            !n.peers_text().contains(good),
            "잘못된 이름 {name:?} 이 파일에 들어감"
        );
    }

    let bad_ids = [
        "".to_string(),
        "xyz".to_string(),
        "g".repeat(64),
        good[..63].to_string(),
        format!("{good}0"),
        format!("{good} "),
        format!(" {good}"),
    ];
    for bad in &bad_ids {
        let o = n.herdvia(&["allow", "p1", bad, "--both"]);
        assert_ne!(o.code(), Some(0), "잘못된 EndpointId {bad:?} 를 받아들임");
        assert!(!o.err().trim().is_empty(), "[{bad:?}] 거부 사유가 없음");
        assert!(
            !has_peer_name(&n.peers_text(), "p1"),
            "잘못된 EndpointId {bad:?} 가 파일에 들어감"
        );
    }

    for args in [
        vec!["allow", "p2", good],
        vec!["allow", "p2", good, "--sideways"],
        vec!["allow", "p2", good, "inbound"],
        vec!["allow", "p2"],
        vec!["allow"],
    ] {
        let o = n.herdvia(&args);
        assert_ne!(o.code(), Some(0), "{args:?} 를 받아들임");
        assert!(!o.err().trim().is_empty(), "{args:?} 거부 사유가 없음");
        assert!(
            !has_peer_name(&n.peers_text(), "p2"),
            "{args:?} 가 파일에 들어감"
        );
    }

    // 경계값은 받아들인다.
    let max = "x".repeat(32);
    n.allow("A.b_9-z", &ids[1], "--both");
    n.allow(&max, &ids[2], "--inbound");
    let peers = n.peers_text();
    assert!(
        has_peer_line(&peers, "A.b_9-z", &ids[1], "both"),
        "peers:\n{peers}"
    );
    assert!(
        has_peer_line(&peers, &max, &ids[2], "in"),
        "peers:\n{peers}"
    );
}

#[test]
fn allow_and_revoke_update_file_without_agent() {
    let n = Node::new("A");
    let _ = n.id();
    let x = Node::new("X").id().0;
    let y = Node::new("Y").id().0;

    for (flag, perm) in [
        ("--inbound", "in"),
        ("--outbound", "out"),
        ("--both", "both"),
    ] {
        n.allow("x", &x, flag);
        let peers = n.peers_text();
        assert!(
            has_peer_line(&peers, "x", &x, perm),
            "[{flag}] peers:\n{peers}"
        );
        assert_eq!(
            peers
                .lines()
                .filter(|l| l.split_whitespace().next() == Some("x"))
                .count(),
            1,
            "[{flag}] 같은 이름 줄이 여러 개:\n{peers}"
        );
        assert_eq!(mode_of(&n.peers_file()), 0o600, "[{flag}] peers 권한");
    }
    n.allow("y", &y, "--inbound");
    n.revoke("x");
    let peers = n.peers_text();
    assert!(
        !has_peer_name(&peers, "x"),
        "revoke 뒤에도 x 가 남음:\n{peers}"
    );
    assert!(
        has_peer_line(&peers, "y", &y, "in"),
        "revoke 가 다른 피어까지 지움:\n{peers}"
    );
    assert_eq!(mode_of(&n.peers_file()), 0o600, "revoke 뒤 peers 권한");
}

// ---------------------------------------------------------------------------
// 받는 쪽 PC 가 꺼져 있을 때
// ---------------------------------------------------------------------------

#[test]
fn via_gives_255_within_about_5s_when_receiver_is_off() {
    let mut a = Node::new("A");
    let mut b = Node::new("B");
    let (a_id, _) = a.id();
    let (b_id, _) = b.id();
    a.allow("b", &b_id, "--outbound");
    b.allow("a", &a_id, "--inbound");
    a.start_agent();
    // B 는 띄우지 않는다.

    let t = Instant::now();
    let out = a.run_via_timeout("b", &session_line("cat"), b"", Duration::from_secs(30));
    let el = t.elapsed();
    assert_eq!(out.code(), Some(255), "stderr: {}", out.err());
    assert!(
        el < Duration::from_secs(8),
        "받는 쪽이 없을 때 약 5초 안에 끝나야 함: {el:?}"
    );
    assert!(!out.err().trim().is_empty(), "stderr 에 사유가 없음");
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);

    // B 가 나중에 켜지면 A 는 다시 걸어 연결한다 (1초~30초 간격 재시도).
    b.start_agent();
    wait_connected(&a, "b", Some(&b), Duration::from_secs(45));
    let out = a.run_via("b", &session_line("cat"), b"back");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"back");
}
