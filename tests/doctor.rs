//! `herdvia doctor`: 읽기 전용 점검. 데이터 폴더를 만들지 않고, 에이전트가 없으면 1로 끝나며,
//! 떠 있는 에이전트에게서 버전과 실행 파일 경로를 받아 온다.

#![cfg(unix)]

mod common;

use common::*;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::{Duration, Instant};

/// 모든 줄이 `ok`/`warn`/`fail` 머리로 시작하는지 보고 줄들을 돌려준다.
fn lines(o: &Out) -> Vec<String> {
    assert!(o.err().is_empty(), "stderr: {}", o.err());
    let out = o.out();
    for l in out.lines() {
        assert!(
            ["ok   ", "warn ", "fail "].iter().any(|p| l.starts_with(p)),
            "줄 머리가 ok/warn/fail 이 아님: {l:?}"
        );
    }
    out.lines().map(str::to_owned).collect()
}

fn find<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix:?} 로 시작하는 줄이 없음:\n{}", lines.join("\n")))
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn creates_nothing_for_a_missing_data_directory() {
    let n = Node::new("X");
    let missing = n.root.join("none");
    let home_before = listing(&n.home);
    let mut c = n.cmd(&["doctor"]);
    c.env("HERDVIA_DIR", &missing);
    let o = try_run(c, Duration::from_secs(30)).expect("doctor 가 30초 안에 끝나지 않음");
    assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
    let l = lines(&o);
    assert!(find(&l, "warn data directory: ").contains("herdvia id"));
    find(&l, "warn secret.key: missing");
    find(&l, "ok   peers: none registered");
    find(&l, "fail agent: not running");
    assert!(!missing.exists(), "doctor 가 데이터 폴더를 만듦");
    assert_eq!(
        listing(&n.home),
        home_before,
        "doctor 가 HOME 에 무언가를 만듦"
    );
}

#[test]
fn fails_on_a_missing_data_directory_below_a_directory_others_can_write() {
    let n = Node::new("X");
    let shared = n.root.join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let mut c = n.cmd(&["doctor"]);
    c.env("HERDVIA_DIR", shared.join("hl"));
    let o = try_run(c, Duration::from_secs(30)).expect("doctor 가 30초 안에 끝나지 않음");
    assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
    let l = lines(&o);
    let dir = find(&l, "fail data directory: ");
    assert!(
        dir.contains(&format!(
            "{} is writable by other users",
            fs::canonicalize(&shared).unwrap().display()
        )),
        "{dir}"
    );
    // 원인은 폴더 줄에 있으므로 키, 피어, 에이전트 줄은 없다.
    for skipped in ["secret.key:", "peers:", "agent:"] {
        assert!(
            !l.iter().any(|l| l[5..].starts_with(skipped)),
            "{skipped} 줄이 있음:\n{}",
            o.out()
        );
    }
    assert!(
        fs::read_dir(&shared).unwrap().next().is_none(),
        "doctor 가 무언가를 만듦"
    );
}

#[test]
fn fails_on_dot_dot_after_a_missing_directory_and_creates_nothing() {
    let n = Node::new("X");
    let shared = n.root.join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    // 없는 missing 뒤의 .. 는 검사하지 못한 shared 로 돌아간다.
    let dir = n.root.join("missing/../shared/new/data");
    let mut c = n.cmd(&["doctor"]);
    c.env("HERDVIA_DIR", &dir);
    let o = try_run(c, Duration::from_secs(30)).expect("doctor 가 30초 안에 끝나지 않음");
    assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
    let l = lines(&o);
    assert_eq!(
        find(&l, "fail data directory: "),
        format!(
            "fail data directory: {} (from HERDVIA_DIR; automatic start always uses the default directory); the path to the data directory {} has \"..\" after {}, which does not exist, so the directories that \"..\" leads back to cannot be checked; write the path without \"..\"",
            dir.display(),
            dir.display(),
            fs::canonicalize(&n.root).unwrap().join("missing").display()
        )
    );
    for skipped in ["secret.key:", "peers:", "agent:"] {
        assert!(
            !l.iter().any(|l| l[5..].starts_with(skipped)),
            "{skipped} 줄이 있음:\n{}",
            o.out()
        );
    }
    // herdvia id 도 거부하고 아무것도 만들지 않는다.
    let mut c = n.cmd(&["id"]);
    c.env("HERDVIA_DIR", &dir);
    let o = try_run(c, Duration::from_secs(30)).expect("id 가 30초 안에 끝나지 않음");
    assert_eq!(o.code(), Some(255), "stdout:\n{}", o.out());
    assert!(!n.root.join("missing").exists(), "missing 이 만들어짐");
    assert!(
        fs::read_dir(&shared).unwrap().next().is_none(),
        "shared 아래에 무언가가 만들어짐"
    );
}

#[test]
fn fails_without_an_agent_and_reads_the_existing_identity() {
    let n = Node::new("X");
    let (id, fp) = n.id();
    n.allow("p", &id, "--both");
    let before = listing(&n.herdvia_dir);
    let o = n.herdvia(&["doctor"]);
    assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
    let l = lines(&o);
    assert_eq!(
        find(&l, "ok   secret.key: "),
        format!("ok   secret.key: present, fingerprint {fp}")
    );
    assert!(!o.out().contains(&id), "EndpointId 전체가 출력됨");
    find(&l, "ok   peers: 1 registered (in 0, out 0, both 1)");
    assert!(find(&l, "fail agent: not running").contains("herdvia agent"));
    assert_eq!(listing(&n.herdvia_dir), before);
    assert!(!l.iter().any(|l| l.starts_with("ok   peer ")));
}

#[test]
fn an_unreadable_key_points_to_a_new_identity_not_a_backup() {
    let n = Node::new("X");
    n.id();
    fs::write(n.key_file(), [1; 31]).unwrap();
    let o = n.herdvia(&["doctor"]);
    assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
    assert_eq!(
        find(&lines(&o), "fail secret.key: "),
        "fail secret.key: secret.key must contain exactly 32 bytes; fix its permissions or ownership if that is the cause; otherwise remove it, run herdvia id to create a new identity, and pair again with every peer (restoring a copied or backed-up key is not recommended)"
    );
    assert_eq!(
        fs::read(n.key_file()).unwrap(),
        [1; 31],
        "doctor 가 키를 바꿈"
    );
}

#[test]
fn fails_on_a_malformed_peer_list() {
    let n = Node::new("X");
    n.id();
    // 권한이 넓으면 그것부터 알린다.
    fs::write(n.peers_file(), "bad line\n").unwrap();
    fs::set_permissions(n.peers_file(), fs::Permissions::from_mode(0o644)).unwrap();
    let o = n.herdvia(&["doctor"]);
    assert_eq!(o.code(), Some(1));
    assert!(
        find(&lines(&o), "fail peers: ").contains("chmod 600"),
        "{}",
        o.out()
    );
    fs::set_permissions(n.peers_file(), fs::Permissions::from_mode(0o600)).unwrap();
    let o = n.herdvia(&["doctor"]);
    assert_eq!(o.code(), Some(1));
    assert!(
        find(&lines(&o), "fail peers: ").contains("invalid or duplicate peer name in peers"),
        "{}",
        o.out()
    );
    assert_eq!(fs::read_to_string(n.peers_file()).unwrap(), "bad line\n");
    assert_eq!(mode_of(&n.peers_file()), 0o600);
}

#[test]
fn reports_the_running_agents_version_and_executable() {
    let mut n = Node::new("X");
    n.id();
    n.start_agent();
    let o = n.herdvia(&["doctor"]);
    let l = lines(&o);
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        find(&l, "ok   agent: "),
        format!("ok   agent: running herdvia {version} from {HERDVIA}"),
        "출력:\n{}",
        o.out()
    );
    // 릴레이 연결 여부는 네트워크에 달렸다. 줄이 있는지만 본다.
    assert!(l.iter().any(|l| l.contains(" relay: ")), "{l:?}");
    find(&l, "warn peer connections: no peers are registered");
    find(&l, "ok   pending: no rejected connection attempts");
    assert!(!l.iter().any(|l| l.starts_with("fail ")), "{l:?}");
    assert_eq!(o.code(), Some(0));
}

#[test]
fn reports_each_connected_peer() {
    let p = shared();
    let o = p.a.herdvia(&["doctor"]);
    let l = lines(&o);
    assert!(
        find(&l, "ok   peer b (out): connected ").contains("ms"),
        "{l:?}"
    );
    let o = p.b.herdvia(&["doctor"]);
    let l = lines(&o);
    find(&l, "ok   peer a (in): connected ");
}

// ---------------------------------------------------------------------------
// 응답하지 않는 에이전트. doctor 는 요청마다 3초(src/doctor.rs 의 AGENT_TIMEOUT)만 기다린다.
// ---------------------------------------------------------------------------

/// src/doctor.rs 의 AGENT_TIMEOUT 과 같은 값.
const AGENT_TIMEOUT: Duration = Duration::from_secs(3);

// src/frame.rs 의 태그.
const STDOUT: u8 = 3;
const EXIT: u8 = 5;

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![tag];
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// 가짜 에이전트가 제어 요청 하나에 하는 일.
#[derive(Clone, Copy)]
enum Act {
    /// 제대로 답한다.
    Answer,
    /// 답하지 않고 연결을 붙잡고 있는다.
    Hang,
    /// 응답 프레임 머리 3바이트만 보내고 멈춘다.
    Partial,
    /// 연결을 끊는다(doctor 를 모르는 구버전 에이전트처럼).
    Drop,
    /// 이 글을 답한다.
    Reply(&'static str),
}

/// 노드의 소켓에서 듣는 가짜 에이전트. `act` 가 요청마다 할 일을 고른다.
/// 답하지 않은 연결은 테스트 프로세스가 끝날 때까지 붙잡는다.
fn fake_agent(n: &Node, act: fn(&str) -> Act) {
    let listener = UnixListener::bind(n.sock()).unwrap();
    thread::spawn(move || {
        let mut held: Vec<UnixStream> = Vec::new();
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut head = [0; 5];
            s.read_exact(&mut head).unwrap();
            let mut payload = vec![0; u32::from_be_bytes(head[1..].try_into().unwrap()) as usize];
            s.read_exact(&mut payload).unwrap();
            let request = String::from_utf8(payload).unwrap();
            match act(&request) {
                Act::Answer => {
                    let text = match request.as_str() {
                        "doctor" => format!(
                            "version {}\nexe {HERDVIA}\nrelay none\n",
                            env!("CARGO_PKG_VERSION")
                        ),
                        "status" => "name permission connection path rtt\n".into(),
                        _ => "fingerprint id attempts last-seen\n".into(),
                    };
                    let _ = s.write_all(&frame(STDOUT, text.as_bytes()));
                    let _ = s.write_all(&frame(EXIT, &0_i32.to_be_bytes()));
                }
                Act::Hang => held.push(s),
                Act::Partial => {
                    let _ = s.write_all(&frame(STDOUT, b"version")[..3]);
                    held.push(s);
                }
                Act::Drop => {}
                Act::Reply(text) => {
                    let _ = s.write_all(&frame(STDOUT, text.as_bytes()));
                    let _ = s.write_all(&frame(EXIT, &0_i32.to_be_bytes()));
                }
            }
        }
    });
}

/// 연결을 받기만 하고 요청을 읽지도 답하지도 않는 가짜 에이전트.
fn silent_agent(n: &Node) {
    let listener = UnixListener::bind(n.sock()).unwrap();
    thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s.unwrap());
        }
    });
}

fn timed_doctor(n: &Node) -> (Out, Duration) {
    let start = Instant::now();
    let o = try_run(n.cmd(&["doctor"]), Duration::from_secs(20))
        .expect("doctor 가 20초 안에 끝나지 않음");
    (o, start.elapsed())
}

/// 제한 시간을 한 번 넘겨 끝났는지. 시간을 두 번 쌓지 않는다.
fn assert_one_timeout(took: Duration, o: &Out) {
    assert!(
        took >= AGENT_TIMEOUT - Duration::from_millis(100) && took < 2 * AGENT_TIMEOUT,
        "{took:?} 걸림. 출력:\n{}",
        o.out()
    );
}

const NO_ANSWER: &str = "did not answer within 3 seconds";

#[test]
fn fails_within_the_timeout_when_the_agent_does_not_answer() {
    for partial in [false, true] {
        let n = Node::new("X");
        n.id();
        if partial {
            fake_agent(&n, |_| Act::Partial);
        } else {
            silent_agent(&n);
        }
        let (o, took) = timed_doctor(&n);
        assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
        let l = lines(&o);
        let agent = find(&l, "fail agent: ");
        assert!(
            agent.contains("may be running") && agent.contains(NO_ANSWER),
            "{agent}"
        );
        assert!(agent.contains("restart it: run herdvia agent"), "{agent}");
        assert!(
            !l.iter()
                .any(|l| l[5..].starts_with("peer ") || l[5..].starts_with("pending:")),
            "{l:?}"
        );
        assert_one_timeout(took, &o);
    }
}

#[test]
fn warns_within_the_timeout_when_status_or_pending_does_not_answer() {
    let n = Node::new("X");
    fake_agent(&n, |r| {
        if r == "status" {
            Act::Hang
        } else {
            Act::Answer
        }
    });
    let (o, took) = timed_doctor(&n);
    let l = lines(&o);
    find(&l, "ok   agent: running herdvia ");
    assert!(
        find(&l, "warn peer connections: ").contains(&format!("the agent {NO_ANSWER}")),
        "{l:?}"
    );
    // 멈춘 에이전트에게 pending 을 다시 묻지 않는다.
    assert!(!l.iter().any(|l| l[5..].starts_with("pending:")), "{l:?}");
    assert_one_timeout(took, &o);

    let n = Node::new("X");
    fake_agent(&n, |r| {
        if r == "pending" {
            Act::Partial
        } else {
            Act::Answer
        }
    });
    let (o, took) = timed_doctor(&n);
    let l = lines(&o);
    find(&l, "ok   agent: running herdvia ");
    find(&l, "warn peer connections: no peers are registered");
    assert!(
        find(&l, "warn pending: ").contains(&format!("the agent {NO_ANSWER}")),
        "{l:?}"
    );
    assert_one_timeout(took, &o);
}

#[test]
fn an_old_agent_is_told_apart_from_a_stuck_one() {
    // doctor 에서 끊고 status 에 답하면 구버전이다. 기다리지 않는다.
    // 구버전은 status-detail 도 모르므로 묻지 않는다(물으면 여기서 멈춘다).
    let n = Node::new("X");
    fake_agent(&n, |r| match r {
        "doctor" => Act::Drop,
        "status-detail" => Act::Hang,
        _ => Act::Answer,
    });
    let (o, took) = timed_doctor(&n);
    let l = lines(&o);
    find(&l, "warn agent: running, but older than this executable");
    find(&l, "ok   pending: no rejected connection attempts");
    assert!(
        !l.iter().any(|l| l[5..].starts_with("peer details:")),
        "{l:?}"
    );
    assert!(took < AGENT_TIMEOUT, "{took:?} 걸림");

    // doctor 에서 끊고 status 에서 멈추면 멈춘 에이전트다.
    let n = Node::new("X");
    fake_agent(&n, |r| if r == "doctor" { Act::Drop } else { Act::Hang });
    let (o, took) = timed_doctor(&n);
    assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
    let l = lines(&o);
    assert!(find(&l, "fail agent: ").contains(NO_ANSWER), "{l:?}");
    assert_one_timeout(took, &o);
}

#[test]
fn says_why_each_peer_is_not_connected() {
    let n = Node::new("X");
    fake_agent(&n, |r| match r {
        "status" => Act::Reply(
            "name permission connection path rtt\nwin1 out disconnected - -\nwin2 both disconnected - -\nlaptop in disconnected - -\n",
        ),
        "status-detail" => Act::Reply(
            "herdvia-status-detail 1\nwin1 out disconnected - - rejected 12 403 peer not allowed\nwin2 both disconnected - - unreachable 18 timed out\nlaptop in waiting - - - -\n",
        ),
        _ => Act::Answer,
    });
    let (o, took) = timed_doctor(&n);
    let l = lines(&o);
    assert_eq!(
        find(&l, "warn peer win1 "),
        "warn peer win1 (out): rejected by the peer (403 peer not allowed); on win1 run herdvia pending, then herdvia accept. Retrying in 12s."
    );
    assert_eq!(
        find(&l, "warn peer win2 "),
        "warn peer win2 (both): could not reach it (timed out); its agent may be off or offline. Retrying in 18s."
    );
    assert_eq!(
        find(&l, "warn peer laptop "),
        "warn peer laptop (in): waiting for laptop to connect (this PC only accepts it)."
    );
    find(&l, "ok   pending: no rejected connection attempts");
    assert!(took < AGENT_TIMEOUT, "{took:?} 걸림");

    // status-detail 을 모르는 에이전트(끊는다)에게는 지금 문구 그대로다.
    let n = Node::new("X");
    fake_agent(&n, |r| match r {
        "status" => Act::Reply("name permission connection path rtt\nwin1 out disconnected - -\n"),
        "status-detail" => Act::Drop,
        _ => Act::Answer,
    });
    let (o, took) = timed_doctor(&n);
    let l = lines(&o);
    assert_eq!(
        find(&l, "warn peer win1 "),
        "warn peer win1 (out): not connected; its agent is off, or it has not allowed this PC (on that PC: herdvia pending, then herdvia accept)"
    );
    find(&l, "ok   pending: no rejected connection attempts");
    assert!(took < AGENT_TIMEOUT, "{took:?} 걸림");

    // status-detail 에서 멈추면 한 번만 기다리고 pending 은 묻지 않는다.
    let n = Node::new("X");
    fake_agent(&n, |r| match r {
        "status" => Act::Reply("name permission connection path rtt\nwin1 out disconnected - -\n"),
        "status-detail" => Act::Hang,
        _ => Act::Answer,
    });
    let (o, took) = timed_doctor(&n);
    let l = lines(&o);
    find(&l, "warn peer win1 (out): not connected; ");
    assert!(
        find(&l, "warn peer details: ").contains(&format!("the agent {NO_ANSWER}")),
        "{l:?}"
    );
    assert!(!l.iter().any(|l| l[5..].starts_with("pending:")), "{l:?}");
    assert_one_timeout(took, &o);
}

// ---------------------------------------------------------------------------
// 데이터 폴더와 herdr-core 위치
// ---------------------------------------------------------------------------

#[test]
fn fails_on_a_data_directory_this_user_cannot_open() {
    let n = Node::new("X");
    n.id();
    // 000: 목록도 못 읽는다. 600: 목록은 읽지만 안의 파일에 닿지 못한다.
    for mode in [0o000, 0o600] {
        fs::set_permissions(&n.herdvia_dir, fs::Permissions::from_mode(mode)).unwrap();
        let o = try_run(n.cmd(&["doctor"]), Duration::from_secs(30));
        fs::set_permissions(&n.herdvia_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let o = o.expect("doctor 가 30초 안에 끝나지 않음");
        assert_eq!(o.code(), Some(1), "stdout:\n{}", o.out());
        let l = lines(&o);
        let dir = find(&l, "fail data directory: ");
        assert!(
            dir.contains("this user cannot open it") && dir.ends_with("run chmod 700 on it"),
            "{dir}"
        );
        // 원인은 폴더에 있으므로 뒤 항목은 쓰지 않는다.
        for skipped in [" secret.key:", " peers:", " agent:"] {
            assert!(!l.iter().any(|l| l.contains(skipped)), "{mode:o}: {l:?}");
        }
    }
}

#[test]
fn does_not_run_a_core_found_only_through_a_relative_or_empty_path_entry() {
    let n = Node::new("X");
    // 상대 경로 항목(bin)과 빈 항목은 현재 폴더 기준이다. 그 폴더의 herdr-core 를 찾지도 실행하지도 않는다.
    for (path, cwd) in [
        ("bin:/usr/bin:/bin", &n.root),
        ("./bin:/usr/bin:/bin", &n.root),
        (":/usr/bin:/bin", &n.bin),
    ] {
        let mut c = n.cmd(&["doctor"]);
        c.env("PATH", path).current_dir(cwd);
        let o = try_run(c, Duration::from_secs(30)).expect("doctor 가 30초 안에 끝나지 않음");
        let l = lines(&o);
        assert!(
            find(&l, "fail herdr-core: ").starts_with("fail herdr-core: not found ("),
            "[{path}] {l:?}"
        );
        assert!(
            !n.root.join("invoked").exists(),
            "[{path}] 현재 폴더 기준의 herdr-core 가 실행됨"
        );
    }
}
