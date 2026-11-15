//! M3 고유 검증: 피어 키 허용 목록, 방향별 권한, id/allow/revoke/status, 시작 거부, 받는 쪽 부재.
//!
//! 공유 쌍(A: b=out, B: a=in)은 피어 목록을 바꾸지 않는 검증에만 쓴다.
//! 피어 목록을 바꾸거나 에이전트를 죽이는 검증은 새 쌍을 띄운다.

#![cfg(unix)]

mod common;

use common::*;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
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

#[test]
fn a_data_directory_below_a_directory_others_can_write_is_refused() {
    // 다른 계정이 바꿀 수 있는 상위 폴더 아래에서는 아무것도 만들지 않고 거부한다.
    let mut n = Node::new("A");
    let shared = n.root.join("shared");
    fs::create_dir(&shared).unwrap();
    chmod(&shared, 0o777);
    n.herdvia_dir = shared.join("hl");
    let other = Node::new("X");
    let (other_id, _) = other.id();
    let refusal = format!(
        "{} is writable by other users",
        fs::canonicalize(&shared).unwrap().display()
    );
    for args in [
        &["id"][..],
        &["allow", "x", other_id.as_str(), "--both"],
        &["status"],
    ] {
        let o = n.herdvia(args);
        assert_eq!(o.code(), Some(255), "{args:?} stderr: {}", o.err());
        assert!(o.err().contains(&refusal), "{args:?} stderr: {}", o.err());
    }
    assert!(
        fs::read_dir(&shared).unwrap().next().is_none(),
        "거부한 명령이 무언가를 만듦"
    );
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
        // 결과 한 줄은 accept 와 같은 형식이다.
        let o = n.herdvia(&["allow", "x", &x, flag]);
        assert_eq!(o.code(), Some(0), "[{flag}] stderr: {}", o.err());
        assert_eq!(o.out(), format!("x {x} {perm}\n"), "[{flag}]");
        assert!(o.err().is_empty(), "[{flag}] stderr: {}", o.err());
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
    let o = n.herdvia(&["revoke", "x"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.out(), format!("revoked x {x}\n"));
    // x 는 both 였으므로 Herdr 정리 안내도 나온다.
    assert_eq!(o.err(), format!("{HERDR_NOTE_X}{OTHER_PC_NOTE}"));
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

#[test]
fn revoke_of_an_unknown_name_fails_and_changes_nothing() {
    let n = Node::new("A");
    let _ = n.id();
    let x = Node::new("X").id().0;
    let o = n.herdvia(&["revoke", "nobody"]);
    assert_eq!(o.code(), Some(255), "없는 이름 revoke 가 성공함");
    assert_eq!(o.err(), "herdvia: no peer named nobody\n");
    assert!(o.stdout.is_empty(), "stdout: {:?}", o.out());
    assert!(!n.peers_file().exists(), "실패한 revoke 가 peers 를 만듦");

    n.allow("x", &x, "--both");
    let before = n.peers_text();
    let o = n.herdvia(&["revoke", "X"]);
    assert_eq!(o.code(), Some(255), "이름은 대소문자를 구분해야 함");
    assert!(o.err().contains("no peer named X"), "stderr: {}", o.err());
    assert_eq!(n.peers_text(), before, "실패한 revoke 가 peers 를 바꿈");
}

#[test]
fn one_endpoint_id_cannot_take_a_second_name() {
    let n = Node::new("A");
    let _ = n.id();
    let x = Node::new("X").id().0;
    n.allow("x", &x, "--outbound");
    let before = n.peers_text();
    let o = n.herdvia(&["allow", "x2", &x, "--both"]);
    assert_eq!(
        o.code(),
        Some(255),
        "같은 EndpointId 를 다른 이름으로 받아들임"
    );
    assert_eq!(o.err().trim().lines().count(), 1, "stderr: {:?}", o.err());
    assert!(
        o.err().contains("already registered as peer x;"),
        "stderr: {}",
        o.err()
    );
    assert!(o.stdout.is_empty(), "stdout: {:?}", o.out());
    assert_eq!(n.peers_text(), before);
    // 같은 이름으로 권한을 바꾸는 것은 그대로 된다.
    n.allow("x", &x, "--both");
    assert!(has_peer_line(&n.peers_text(), "x", &x, "both"));
}

#[test]
fn revoke_warns_when_the_endpoint_id_is_still_allowed_under_another_name() {
    let n = Node::new("A");
    let _ = n.id();
    let x = Node::new("X").id().0;
    // 중복을 막기 전에 만들어진 peers 파일도 그대로 읽힌다.
    fs::write(n.peers_file(), format!("a {x} both\nb {x} in\n")).unwrap();
    chmod(&n.peers_file(), 0o600);
    let o = n.herdvia(&["revoke", "a"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.out(), format!("revoked a {x}\n"));
    let err = o.err();
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 3, "stderr: {err:?}");
    assert!(
        lines[0].starts_with("herdvia: warning: ") && lines[0].contains("still allowed as peer b;"),
        "stderr: {err}"
    );
    // 경고가 먼저, 정리 안내가 뒤에 온다.
    assert_eq!(
        format!("{}\n{}\n", lines[1], lines[2]),
        format!(
            "herdvia: note: Herdr may still list a; remove it with: hc machine rm a\n{OTHER_PC_NOTE}"
        )
    );
    let peers = n.peers_text();
    assert!(!has_peer_name(&peers, "a"), "peers:\n{peers}");
    assert!(has_peer_line(&peers, "b", &x, "in"), "peers:\n{peers}");
    // 마지막 이름을 지우면 경고가 없고, inbound 만인 피어는 Herdr 안내도 없다.
    let o = n.herdvia(&["revoke", "b"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.err(), OTHER_PC_NOTE);
}

const HERDR_NOTE_X: &str =
    "herdvia: note: Herdr may still list x; remove it with: hc machine rm x\n";
const OTHER_PC_NOTE: &str = "herdvia: note: on that PC, also revoke this PC (herdvia revoke <the name it uses for this PC>)\n";

// ---------------------------------------------------------------------------
// peers 목록과 allow 의 키 변경 경고
// ---------------------------------------------------------------------------

#[test]
fn peers_lists_saved_peers_without_an_agent_and_creates_nothing() {
    let n = Node::new("A");
    // 데이터 폴더가 없어도, 비어 있어도 만들지 않고 알린다.
    let missing = n.root.join("none");
    for dir in [&missing, &n.herdvia_dir] {
        let mut c = n.cmd(&["peers"]);
        c.env("HERDVIA_DIR", dir);
        let o = try_run(c, Duration::from_secs(10)).expect("peers 가 10초 안에 끝나지 않음");
        assert_eq!(o.code(), Some(0), "[{}] stderr: {}", dir.display(), o.err());
        assert_eq!(o.out(), "no peers registered\n");
        assert!(o.err().is_empty(), "stderr: {}", o.err());
    }
    assert!(!missing.exists(), "peers 가 데이터 폴더를 만듦");
    assert_eq!(
        fs::read_dir(&n.herdvia_dir).unwrap().count(),
        0,
        "peers 가 데이터 폴더에 파일을 만듦"
    );

    let (x, x_fp) = Node::new("X").id();
    let (y, y_fp) = Node::new("Y").id();
    n.allow("x", &x, "--both");
    n.allow("y", &y, "--inbound");
    let before = fs::read_dir(&n.herdvia_dir).unwrap().count();
    let o = n.herdvia(&["peers"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(
        o.out(),
        format!(
            "name permission fingerprint\nx both {x_fp}\ny in {y_fp}\nin = may open sessions on this PC; out = this PC may connect to it\n"
        )
    );
    assert!(o.err().is_empty(), "stderr: {}", o.err());
    assert!(!o.out().contains(&x), "EndpointId 전체가 출력됨");
    assert_eq!(fs::read_dir(&n.herdvia_dir).unwrap().count(), before);

    // 에이전트와 같은 안전 검사로 거부하고, 권한을 고치지 않는다.
    chmod(&n.peers_file(), 0o644);
    let o = n.herdvia(&["peers"]);
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert!(o.stdout.is_empty(), "stdout: {}", o.out());
    assert_eq!(o.err().lines().count(), 1, "stderr: {}", o.err());
    assert!(o.err().contains("0600"), "stderr: {}", o.err());
    assert_eq!(mode_of(&n.peers_file()), 0o644, "peers 가 권한을 고침");
}

#[test]
fn allow_warns_when_a_name_moves_to_another_key() {
    let n = Node::new("A");
    let (x, x_fp) = Node::new("X").id();
    let (y, y_fp) = Node::new("Y").id();
    n.allow("p", &x, "--both");
    // 같은 키의 권한만 바꾸면 경고가 없다.
    let o = n.herdvia(&["allow", "p", &x, "--outbound"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert!(o.err().is_empty(), "stderr: {}", o.err());
    let o = n.herdvia(&["allow", "p", &y, "--outbound"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.out(), format!("p {y} out\n"));
    assert_eq!(
        o.err(),
        format!(
            "herdvia: warning: p now points to a different key (fingerprint {x_fp} -> {y_fp})\n"
        )
    );
    assert!(has_peer_line(&n.peers_text(), "p", &y, "out"));

    // 에이전트가 reload 를 확인하지 않아도 경고는 오류보다 먼저 나온다.
    fake_agent(&n, reload_error);
    let o = n.herdvia(&["allow", "p", &x, "--both"]);
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert!(o.stdout.is_empty(), "stdout: {}", o.out());
    assert_eq!(
        o.err(),
        format!(
            "herdvia: warning: p now points to a different key (fingerprint {y_fp} -> {x_fp})\nherdvia: saved p in the peers file, but the running agent did not confirm it (invalid peers file){AFTER}"
        )
    );
    assert!(has_peer_line(&n.peers_text(), "p", &x, "both"));
}

// ---------------------------------------------------------------------------
// 실행 중인 에이전트가 reload 를 확인하지 않을 때
// ---------------------------------------------------------------------------

const STDOUT: u8 = 3;
const EXIT: u8 = 5;
const ERROR: u8 = 6;

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![tag];
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// 노드의 소켓에서 듣는 가짜 에이전트. `reply` 가 제어 요청마다 보낼 바이트를 고르고,
/// None 이면 답하지 않고 연결을 테스트 프로세스가 끝날 때까지 붙잡는다.
fn fake_agent(n: &Node, reply: impl Fn(&str) -> Option<Vec<u8>> + Send + 'static) {
    let listener = UnixListener::bind(n.sock()).unwrap();
    thread::spawn(move || {
        let mut held: Vec<UnixStream> = Vec::new();
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut head = [0; 5];
            s.read_exact(&mut head).unwrap();
            let mut payload = vec![0; u32::from_be_bytes(head[1..].try_into().unwrap()) as usize];
            s.read_exact(&mut payload).unwrap();
            match reply(std::str::from_utf8(&payload).unwrap()) {
                Some(bytes) => {
                    let _ = s.write_all(&bytes);
                }
                None => held.push(s),
            }
        }
    });
}

fn reload_error(_: &str) -> Option<Vec<u8>> {
    Some(frame(ERROR, b"invalid peers file"))
}

const AFTER: &str = "; run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)\n";

#[test]
fn revoke_fails_when_the_running_agent_never_answers_the_reload() {
    let n = Node::new("A");
    let x = Node::new("X").id().0;
    n.allow("x", &x, "--both");
    fake_agent(&n, |_| None);
    let t = Instant::now();
    let o = n.herdvia(&["revoke", "x"]);
    let took = t.elapsed();
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert!(
        took >= Duration::from_millis(4900) && took < Duration::from_secs(8),
        "{took:?} 걸림"
    );
    assert!(
        o.stdout.is_empty(),
        "확인되지 않았는데 성공 줄을 냄: {}",
        o.out()
    );
    assert_eq!(
        o.err(),
        format!(
            "herdvia: revoked x in the peers file, but the running agent did not confirm it (the agent did not answer the reload request within 5 seconds), so it may still admit x until it reloads{AFTER}"
        )
    );
    assert!(
        !has_peer_name(&n.peers_text(), "x"),
        "파일에서 지워지지 않음"
    );
}

#[test]
fn revoke_keeps_the_duplicate_key_warning_when_the_agent_answers_an_error() {
    let n = Node::new("A");
    let x = Node::new("X").id().0;
    fs::write(n.peers_file(), format!("a {x} both\nb {x} in\n")).unwrap();
    chmod(&n.peers_file(), 0o600);
    fake_agent(&n, reload_error);
    let o = n.herdvia(&["revoke", "a"]);
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert!(o.stdout.is_empty(), "stdout: {}", o.out());
    let err = o.err();
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 2, "stderr: {err}");
    assert!(
        lines[0].starts_with("herdvia: warning: ") && lines[0].contains("still allowed as peer b;"),
        "stderr: {err}"
    );
    assert_eq!(
        format!("{}\n", lines[1]),
        format!(
            "herdvia: revoked a in the peers file, but the running agent did not confirm it (invalid peers file), so it may still admit a until it reloads{AFTER}"
        )
    );
    let peers = n.peers_text();
    assert!(!has_peer_name(&peers, "a"), "peers:\n{peers}");
    assert!(has_peer_line(&peers, "b", &x, "in"), "peers:\n{peers}");
}

#[test]
fn allow_and_accept_fail_when_the_agent_answers_an_error() {
    let n = Node::new("A");
    let x = Node::new("X").id().0;
    let y = Node::new("Y").id().0;
    let attempt = format!(
        "fingerprint id attempts last-seen\n{} {y} 1 0s\n",
        fingerprint_of(&y)
    );
    fake_agent(&n, move |request| match request {
        "pending" => {
            let mut bytes = frame(STDOUT, attempt.as_bytes());
            bytes.extend(frame(EXIT, &0_i32.to_be_bytes()));
            Some(bytes)
        }
        _ => reload_error(request),
    });
    let o = n.herdvia(&["allow", "x", &x, "--outbound"]);
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert!(o.stdout.is_empty(), "stdout: {}", o.out());
    assert_eq!(
        o.err(),
        format!(
            "herdvia: saved x in the peers file, but the running agent did not confirm it (invalid peers file){AFTER}"
        )
    );
    assert!(has_peer_line(&n.peers_text(), "x", &x, "out"));

    let o = n.herdvia(&["accept", "y", &fingerprint_of(&y), "--inbound"]);
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert!(o.stdout.is_empty(), "stdout: {}", o.out());
    assert_eq!(
        o.err(),
        format!(
            "herdvia: saved y in the peers file, but the running agent did not confirm it (invalid peers file){AFTER}"
        )
    );
    assert!(has_peer_line(&n.peers_text(), "y", &y, "in"));
}

#[test]
fn agent_reload_reports_a_confirmed_reload() {
    let n = Node::new("A");
    fake_agent(&n, |request| {
        assert_eq!(request, "reload");
        Some(frame(EXIT, &0_i32.to_be_bytes()))
    });
    let o = n.herdvia(&["agent", "reload"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(
        o.out(),
        "reloaded: peers re-read; waiting connections retry now\n"
    );
    assert!(o.err().is_empty(), "stderr: {}", o.err());
}

#[test]
fn agent_reload_without_an_agent_is_255_and_creates_nothing() {
    let n = Node::new("A");
    let missing = n.root.join("none");
    for dir in [&n.herdvia_dir, &missing] {
        let mut c = n.cmd(&["agent", "reload"]);
        c.env("HERDVIA_DIR", dir);
        let o = try_run(c, Duration::from_secs(10)).expect("agent reload 가 10초 안에 끝나지 않음");
        assert_eq!(
            o.code(),
            Some(255),
            "[{}] stdout: {}",
            dir.display(),
            o.out()
        );
        assert!(o.stdout.is_empty(), "stdout: {}", o.out());
        assert_eq!(
            o.err(),
            "herdvia: agent is not running; start it with 'herdvia agent install' (or 'herdvia agent' in a terminal)\n"
        );
    }
    assert!(!missing.exists(), "agent reload 가 데이터 폴더를 만듦");
    assert_eq!(
        fs::read_dir(&n.herdvia_dir).unwrap().count(),
        0,
        "agent reload 가 데이터 폴더에 파일을 만듦"
    );
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
