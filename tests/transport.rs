//! M2 루프백 테스트가 보던 성질을 M3 의 A→B 네트워크 경로로 옮긴 통합 테스트.
//!
//! A 에서 `hlink via b <줄>` 을 실행하면 A 의 에이전트가 iroh 로 B 의 에이전트에 스트림을 열고,
//! B 가 줄을 검사해 실행한다. B 의 PATH 에는 가짜 herdr-core 가 있고 HC_SESSION 으로 동작을 고른다.
//! 대부분의 검증은 한 쌍(`shared()`)을 잠가 가며 재사용한다. 에이전트를 죽이는 검증만 새 쌍을 띄운다.

#![cfg(unix)]

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// (1) 바이트 투명성: 표준 입력이 그대로 원격으로 가고 출력이 그대로 돌아온다
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_all_byte_values_default_line() {
    let p = shared();
    let input = all_bytes(4096); // 1 MiB, 0x00~0xFF 전부
    let out = p.a.run_via("b", DEFAULT_LINE, &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "기본 줄 cat 왕복");
    assert_eq!(
        p.b.invoked().as_deref(),
        Some("<default>\n"),
        "B 에서 한 번 실행되어야 함"
    );
    assert_eq!(p.a.invoked(), None, "거는 쪽 A 에서 실행되면 안 됨");
}

#[test]
fn roundtrip_all_byte_values_session_line() {
    let p = shared();
    let input = all_bytes(64);
    let out = p.a.run_via("b", &session_line("cat"), &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "세션 줄 cat 왕복");
    assert_eq!(p.b.invoked().as_deref(), Some("cat\n"));
    assert_eq!(p.a.invoked(), None);
}

#[test]
fn interactive_echo_without_closing_stdin() {
    // 묶어 보내지 않고 읽는 즉시 전달하는지: 표준 입력을 열어 둔 채 조각마다 메아리를 받는다.
    let p = shared();
    let mut v = p.a.spawn_via("b", &session_line("cat"));
    for i in 0..32u8 {
        let msg = [i, 0x00, 0xFF, b'\n', 0x80u8.wrapping_add(i)];
        v.write(&msg);
        let got = v.read_stdout_exact(msg.len(), Duration::from_secs(5));
        assert_eq!(got, msg, "조각 {i} 메아리 불일치");
    }
    thread::sleep(Duration::from_millis(300));
    assert!(v.running(), "표준 입력이 열려 있는데 via 가 먼저 끝남");
    let out = v.finish(Duration::from_secs(10));
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert!(
        out.stdout.is_empty(),
        "남은 출력이 있으면 안 됨: {:?}",
        out.stdout
    );
}

// ---------------------------------------------------------------------------
// (2) EOF 와 종료 코드
// ---------------------------------------------------------------------------

#[test]
fn closing_stdin_ends_remote_and_via_with_its_code() {
    let p = shared();
    let mut v = p.a.spawn_via("b", &session_line("catexit3"));
    v.write(b"abc");
    assert_eq!(v.read_stdout_exact(3, Duration::from_secs(5)), b"abc");
    thread::sleep(Duration::from_millis(200));
    assert!(v.running(), "표준 입력을 닫기 전에 via 가 끝남");
    let t = Instant::now();
    let out = v.finish(Duration::from_secs(10));
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "표준 입력을 닫은 뒤 via 가 끝나기까지 너무 오래 걸림: {:?}",
        t.elapsed()
    );
    assert_eq!(out.code(), Some(3), "stderr: {}", out.err());
    assert!(out.stdout.is_empty());
}

#[test]
fn nonzero_exit_code_is_forwarded() {
    let p = shared();
    let out = p.a.run_via("b", &session_line("exit7"), b"");
    assert_eq!(out.code(), Some(7), "stderr: {}", out.err());
    assert!(out.stdout.is_empty());
}

#[test]
fn signal_exit_is_forwarded_as_128_plus_n() {
    let p = shared();
    let out = p.a.run_via("b", &session_line("sigterm"), b"");
    assert_eq!(out.code(), Some(128 + 15), "stderr: {}", out.err());
}

// ---------------------------------------------------------------------------
// (3) 127: B 에 herdr-core 가 PATH 와 HOME 모두에 없을 때
// ---------------------------------------------------------------------------

#[test]
fn missing_herdr_core_gives_127_and_message() {
    for path in ["/usr/bin/herdr-core", "/bin/herdr-core"] {
        assert!(!Path::new(path).exists(), "전제 조건 위반: {path} 가 있음");
    }
    let p = shared();
    p.b.set_fake(Fake::Nowhere);
    for line in [DEFAULT_LINE.to_string(), session_line("cat")] {
        let out = p.a.run_via("b", &line, b"");
        assert_eq!(out.code(), Some(127), "stderr: {}", out.err());
        assert!(
            out.err().contains(NOT_INSTALLED_MSG),
            "stderr 에 미설치 문구가 없음: {:?}",
            out.err()
        );
        assert!(
            out.stdout.is_empty(),
            "stdout 에 무언가 섞임: {:?}",
            out.stdout
        );
    }
    // A 에는 가짜 herdr-core 가 있어도 A 에서 실행되면 안 된다.
    assert_eq!(p.a.invoked(), None, "B 가 없을 때 A 쪽에서 실행됨");
}

#[test]
fn herdr_core_found_via_home_fallback() {
    // B 의 PATH 에는 없고 $HOME/.local/bin 에만 있으면 그쪽으로 실행된다 (127 경우의 대조군).
    let p = shared();
    p.b.set_fake(Fake::InHome);
    let input = all_bytes(4);
    let out = p.a.run_via("b", DEFAULT_LINE, &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "HOME 폴백 cat 왕복");
    assert_eq!(p.b.invoked().as_deref(), Some("<default>\n"));
}

// ---------------------------------------------------------------------------
// (4) stdout 과 stderr 가 섞이지 않는다
// ---------------------------------------------------------------------------

#[test]
fn stdout_and_stderr_stay_separate() {
    let p = shared();
    let out = p.a.run_via("b", &session_line("stderr"), b"");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());

    let expected_out: String = (0..300).map(|i| format!("out {i}\n")).collect();
    let actual_out = String::from_utf8_lossy(&out.stdout);
    assert!(
        actual_out == expected_out,
        "stdout 이 원격 stdout 과 다름 (err 줄 섞임 여부: {})",
        actual_out.contains("err ")
    );

    let err = out.err();
    assert!(!err.contains("out "), "stderr 에 stdout 줄이 섞임");
    let err_lines: Vec<&str> = err.lines().filter(|l| l.starts_with("err ")).collect();
    let expected_err: Vec<String> = (0..300).map(|i| format!("err {i}")).collect();
    assert_eq!(err_lines, expected_err, "stderr 줄이 빠지거나 순서가 바뀜");
}

// ---------------------------------------------------------------------------
// (5) 허용되지 않은 줄은 255 와 stderr 사유로 거부되고 어느 쪽에서도 실행되지 않는다
// ---------------------------------------------------------------------------

#[test]
fn disallowed_lines_are_rejected_and_not_executed() {
    let p = shared();
    let marker_b = p.b.root.join("PWNED");
    let marker_a = p.a.root.join("PWNED");
    let m = marker_b.display().to_string();
    let ma = marker_a.display().to_string();
    let token = "REJECTTOKEN4729";

    let cases: Vec<(&str, String)> = vec![
        ("임의 명령", format!("echo {token}; touch {m}; touch {ma}")),
        ("임의 sh -c", format!("sh -c 'touch {m}; touch {ma}'")),
        ("빈 줄", String::new()),
        (
            "connect 대신 다른 하위 명령",
            DEFAULT_LINE.replace("connect'", "serve'"),
        ),
        (
            "세션에 세미콜론·공백",
            session_line(&format!("x;touch {m};")),
        ),
        ("세션에 명령 치환", session_line(&format!("$(touch {m})"))),
        ("세션에 백틱", session_line(&format!("`touch {m}`"))),
        ("세션에 슬래시", session_line("a/b")),
        (
            "세션에 작은따옴표",
            session_line(&format!("a';touch {m};'")),
        ),
        ("세션에 비 ASCII", session_line("세션")),
        ("빈 세션", session_line("")),
        ("세션 65자", session_line(&"a".repeat(65))),
        ("줄 끝에 명령 덧붙임", format!("{DEFAULT_LINE}; touch {m}")),
        ("줄 끝에 && 덧붙임", format!("{DEFAULT_LINE} && touch {m}")),
        ("줄 끝에 개행", format!("{DEFAULT_LINE}\n")),
        ("줄 끝에 공백", format!("{DEFAULT_LINE} ")),
        ("줄 앞에 공백", format!(" {DEFAULT_LINE}")),
        ("줄 앞에 명령", format!("touch {m}; {DEFAULT_LINE}")),
        (
            "세션 줄 끝에 덧붙임",
            format!("{}; touch {m}", session_line("cat")),
        ),
    ];

    for (name, line) in &cases {
        let out = p.a.run_via("b", line, b"");
        assert_eq!(
            out.code(),
            Some(255),
            "[{name}] 거부되어야 함. 줄: {line:?}, stderr: {}",
            out.err()
        );
        assert!(
            !out.err().trim().is_empty(),
            "[{name}] stderr 에 사유가 없음"
        );
        assert!(
            out.stdout.is_empty(),
            "[{name}] stdout 에 무언가 섞임: {:?}",
            out.stdout
        );
        thread::sleep(Duration::from_millis(50));
        assert!(
            !marker_b.exists(),
            "[{name}] 거부된 줄이 B 에서 실행됨 (표지 파일 생김)"
        );
        assert!(
            !marker_a.exists(),
            "[{name}] 거부된 줄이 A 에서 실행됨 (표지 파일 생김)"
        );
        assert_eq!(
            p.b.invoked(),
            None,
            "[{name}] 거부된 줄에서 B 의 herdr-core 가 실행됨"
        );
        assert_eq!(
            p.a.invoked(),
            None,
            "[{name}] 거부된 줄에서 A 의 herdr-core 가 실행됨"
        );
    }

    // 거부한 줄은 로그에 남긴다 (어느 쪽 에이전트든, 출력 또는 데이터 폴더 안 파일).
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let logs = format!("{}\n{}", p.a.agent_log(), p.b.agent_log());
        if logs.contains(token) {
            break;
        }
        if Instant::now() > deadline {
            panic!("거부한 줄이 에이전트 로그에 남지 않음. 로그:\n{logs}");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // 거부 뒤에도 연결과 두 에이전트는 정상 동작한다.
    let out = p.a.run_via("b", &session_line("cat"), b"still alive");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"still alive");
}

#[test]
fn session_name_boundaries_are_accepted() {
    // 허용 문자 집합과 최대 길이(64)는 받아들여져 B 에서 실제로 실행된다.
    // 가짜 herdr-core 는 모르는 모드에서 98 로 끝난다.
    let p = shared();
    let long = "a".repeat(64);
    for name in ["a.b_c-D9", "Z", long.as_str()] {
        let out = p.a.run_via("b", &session_line(name), b"");
        assert_eq!(
            out.code(),
            Some(98),
            "[{name}] 허용되어야 함. stderr: {}",
            out.err()
        );
        assert!(out.err().contains(&format!("unknown mode {name}")));
    }
    let invoked = p.b.invoked().unwrap_or_default();
    assert_eq!(invoked, format!("a.b_c-D9\nZ\n{long}\n"));
}

// ---------------------------------------------------------------------------
// (6) 로컬 에이전트가 없을 때 via 는 기다리지 않고 255
// ---------------------------------------------------------------------------

fn assert_quick_255(n: &Node, what: &str) {
    let t = Instant::now();
    let out = n.run_via("b", DEFAULT_LINE, b"");
    let el = t.elapsed();
    assert_eq!(out.code(), Some(255), "[{what}] stderr: {}", out.err());
    assert!(
        el < Duration::from_secs(2),
        "[{what}] via 가 기다림: {el:?}"
    );
    assert!(
        !out.err().trim().is_empty(),
        "[{what}] stderr 에 사유가 없음"
    );
    assert!(out.stdout.is_empty(), "[{what}] stdout 에 무언가 섞임");
    assert_eq!(n.invoked(), None);
}

#[test]
fn no_agent_means_immediate_255() {
    let mut n = Node::new("A");
    // 데이터 폴더는 있지만 소켓이 없음
    assert_quick_255(&n, "소켓 없음");
    // 데이터 폴더 자체가 없음
    n.hlink_dir = n.root.join("does-not-exist");
    assert_quick_255(&n, "폴더 없음");
}

#[test]
fn stale_socket_means_immediate_255() {
    let n = Node::new("A");
    // 소켓 파일은 남아 있지만 듣는 쪽이 없음 (연결 거부)
    drop(UnixListener::bind(n.sock()).unwrap());
    assert!(n.sock().exists());
    assert_quick_255(&n, "죽은 소켓");
}

#[test]
fn caller_agent_dying_midstream_gives_255_and_remote_child_is_cleaned() {
    // A 의 에이전트가 죽으면 via 는 EXIT 없이 끊긴 것이므로 곧바로 255.
    // B 는 연결이 끊긴 것을 알아채면 자식을 정리한다 (SIGKILL 이라 QUIC 유휴 시간 제한까지 걸릴 수 있다).
    let mut p = Pair::new();
    let mut v = p.a.spawn_via("b", &session_line("sleep"));
    assert_eq!(v.read_stdout_exact(6, Duration::from_secs(10)), b"ready\n");
    let child = p.b.read_pid("child.pid");
    let grandchild = p.b.read_pid("grandchild.pid");
    assert!(
        process_alive(&child) && process_alive(&grandchild),
        "원격 프로세스가 처음부터 없음"
    );

    p.a.kill_agent("KILL");
    let t = Instant::now();
    let out = v.wait_end_keep_stdin(Duration::from_secs(10));
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "A 에이전트가 죽은 뒤 via 가 늦게 끝남: {:?}",
        t.elapsed()
    );
    assert_eq!(out.code(), Some(255), "stderr: {}", out.err());
    assert!(!out.err().trim().is_empty());
    assert!(
        out.stdout.is_empty(),
        "stdout 에 원격 외 바이트가 섞임: {:?}",
        out.stdout
    );

    wait_gone(
        &[&child, &grandchild],
        Duration::from_secs(60),
        "A 에이전트 사망 뒤 B 의 자식 정리",
    );
}

#[test]
fn receiver_agent_dying_midstream_gives_255() {
    // B 의 에이전트가 죽으면 연결이 끊긴 것이므로 A 의 via 는 255 로 끝난다.
    let mut p = Pair::new();
    let mut v = p.a.spawn_via("b", &session_line("sleep"));
    assert_eq!(v.read_stdout_exact(6, Duration::from_secs(10)), b"ready\n");
    let _ = p.b.read_pid("child.pid");
    p.b.kill_agent("KILL");
    let out = v.wait_end_keep_stdin(Duration::from_secs(60));
    assert_eq!(out.code(), Some(255), "stderr: {}", out.err());
    assert!(!out.err().trim().is_empty());
    assert!(
        out.stdout.is_empty(),
        "stdout 에 원격 외 바이트가 섞임: {:?}",
        out.stdout
    );
    // A 의 에이전트는 계속 산다.
    assert!(
        p.a.agent_running(),
        "B 가 죽었는데 A 에이전트도 끝남. 로그:\n{}",
        p.a.agent_log()
    );
}

// ---------------------------------------------------------------------------
// (7) 수 MB 출력이 손실 없이 전달되고 그동안 입력 방향이 막히지 않는다
// ---------------------------------------------------------------------------

#[test]
fn multi_megabyte_output_while_input_flows() {
    let p = shared();
    let big_out = pattern(8 * 1024 * 1024, 1);
    fs::write(p.b.root.join("pattern.bin"), &big_out).unwrap();
    let big_in = pattern(2 * 1024 * 1024, 7);

    let mut v = p.a.spawn_via("b", &session_line("big"));
    // 표준 출력을 아직 읽지 않는다. 출력 방향이 꽉 막힌 상태에서도 입력은 원격에 닿아야 한다.
    let done = v.feed(big_in.clone());
    done.recv_timeout(Duration::from_secs(60))
        .expect("출력이 막힌 동안 via 표준 입력 쓰기가 막힘 (입력 방향이 출력 방향에 묶임)");

    let copy = p.b.root.join("stdin_copy");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let len = fs::metadata(&copy).map(|m| m.len()).unwrap_or(0);
        if len == big_in.len() as u64 {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "출력이 막힌 동안 입력이 원격에 다 닿지 않음: {len}/{} 바이트",
                big_in.len()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(v.running(), "출력을 다 보내기 전에 via 가 끝남");

    v.start_reading_stdout();
    let out = v.finish(Duration::from_secs(120));
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &big_out, "8 MiB 출력");
    assert_bytes_eq(&fs::read(&copy).unwrap(), &big_in, "2 MiB 입력");
}

#[test]
fn multi_megabyte_echo_full_duplex() {
    // cat 으로 수 MB 를 동시에 넣고 받는다. 한쪽이 다른 쪽을 기다리면 교착된다.
    let p = shared();
    let input = pattern(6 * 1024 * 1024, 3);
    let out =
        p.a.run_via_timeout("b", &session_line("cat"), &input, Duration::from_secs(120));
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "6 MiB 메아리");
}

// ---------------------------------------------------------------------------
// (8) via 여러 개 동시 실행 (한 연결 위 여러 스트림)
// ---------------------------------------------------------------------------

#[test]
fn many_concurrent_vias() {
    let p = shared();
    const N: usize = 8;
    let mut vias: Vec<Via> = (0..N)
        .map(|i| {
            let mode = if i % 2 == 0 { "cat" } else { "catexit3" };
            p.a.spawn_via("b", &session_line(mode))
        })
        .collect();

    // 모두 열린 상태에서 각자 왕복이 되는지 (서로 섞이지 않는지)
    for round in 0..3 {
        for (i, v) in vias.iter_mut().enumerate() {
            let msg = format!("via {i} round {round}\n");
            v.write(msg.as_bytes());
        }
        for (i, v) in vias.iter_mut().enumerate() {
            let msg = format!("via {i} round {round}\n");
            let got = v.read_stdout_exact(msg.len(), Duration::from_secs(10));
            assert_eq!(String::from_utf8_lossy(&got), msg);
        }
    }

    // 동시에 큰 데이터를 흘린다
    let payloads: Vec<Vec<u8>> = (0..N)
        .map(|i| pattern(512 * 1024, i as u32 + 100))
        .collect();
    let dones: Vec<Receiver<()>> = vias
        .iter_mut()
        .zip(&payloads)
        .map(|(v, pl)| v.feed(pl.clone()))
        .collect();
    let handles: Vec<_> = vias
        .into_iter()
        .map(|v| thread::spawn(move || v.finish(Duration::from_secs(120))))
        .collect();
    for d in dones {
        let _ = d.recv_timeout(Duration::from_secs(120));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let out = h.join().unwrap();
        let want = if i % 2 == 0 { 0 } else { 3 };
        assert_eq!(out.code(), Some(want), "via {i} stderr: {}", out.err());
        assert_bytes_eq(&out.stdout, &payloads[i], &format!("via {i}"));
    }
}

// ---------------------------------------------------------------------------
// (9) via 를 죽이면 B 의 원격 자식(프로세스 그룹)이 정리된다
// ---------------------------------------------------------------------------

#[test]
fn killing_via_cleans_up_remote_children() {
    let p = shared();
    for sig in ["KILL", "TERM"] {
        let _ = fs::remove_file(p.b.root.join("child.pid"));
        let _ = fs::remove_file(p.b.root.join("grandchild.pid"));

        let mut v = p.a.spawn_via("b", &session_line("sleep"));
        assert_eq!(v.read_stdout_exact(6, Duration::from_secs(10)), b"ready\n");
        let child = p.b.read_pid("child.pid");
        let grandchild = p.b.read_pid("grandchild.pid");
        assert!(process_alive(&child), "[{sig}] 원격 자식이 처음부터 없음");
        assert!(
            process_alive(&grandchild),
            "[{sig}] 원격 손자가 처음부터 없음"
        );

        let st = Command::new("kill")
            .args([&format!("-{sig}"), &v.child.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
        v.wait(Duration::from_secs(5));

        wait_gone(
            &[&child, &grandchild],
            Duration::from_secs(10),
            &format!("{sig} 로 via 종료"),
        );
    }

    // 연결과 에이전트는 계속 동작한다.
    let out = p.a.run_via("b", &session_line("cat"), b"after kill");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"after kill");
}

// ---------------------------------------------------------------------------
// (10) 폴더·소켓·키·피어 파일 권한
// ---------------------------------------------------------------------------

#[test]
fn permissions_when_agent_creates_dir() {
    let mut n = Node::new("A");
    n.hlink_dir = n.root.join("fresh");
    assert!(!n.hlink_dir.exists());
    n.start_agent();
    assert_eq!(mode_of(&n.hlink_dir), 0o700, "데이터 폴더 권한");
    let meta = fs::symlink_metadata(n.sock()).unwrap();
    assert!(meta.file_type().is_socket(), "agent.sock 이 소켓이 아님");
    assert_eq!(mode_of(&n.sock()), 0o600, "소켓 권한");
    assert_eq!(
        mode_of(&n.key_file()),
        0o600,
        "secret.key 권한 (에이전트가 키를 만든 경우)"
    );
}

#[test]
fn permissions_when_dir_is_too_open() {
    // 이미 있는 폴더가 0755 여도 결과는 0700 / 0600 이어야 한다. allow 가 만든 peers 도 0600.
    let mut n = Node::new("A");
    fs::set_permissions(&n.hlink_dir, fs::Permissions::from_mode(0o755)).unwrap();
    n.start_agent();
    let other = Node::new("X");
    let (other_id, _) = other.id();
    n.allow("x", &other_id, "--both");
    assert_eq!(mode_of(&n.hlink_dir), 0o700, "데이터 폴더 권한");
    assert_eq!(mode_of(&n.sock()), 0o600, "소켓 권한");
    assert_eq!(mode_of(&n.key_file()), 0o600, "secret.key 권한");
    assert_eq!(mode_of(&n.peers_file()), 0o600, "peers 권한");
}

// ---------------------------------------------------------------------------
// (11) via 의 표준 출력에는 원격 바이트 외에 아무것도 섞이지 않는다
// ---------------------------------------------------------------------------

#[test]
fn via_stdout_carries_only_remote_bytes() {
    let p = shared();

    // 원격이 아무것도 쓰지 않으면 stdout 은 정확히 비어 있다.
    let out = p.a.run_via("b", &session_line("cat"), b"");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert!(
        out.stdout.is_empty(),
        "빈 원격 출력인데 stdout 에 {:?}",
        out.stdout
    );

    let out = p.a.run_via("b", &session_line("exit7"), b"");
    assert_eq!(out.code(), Some(7));
    assert!(
        out.stdout.is_empty(),
        "exit7 인데 stdout 에 {:?}",
        out.stdout
    );

    // 원격 stderr 가 있어도 stdout 은 원격 stdout 그대로다.
    let out = p.a.run_via("b", &session_line("stderr"), b"");
    let expected: String = (0..300).map(|i| format!("out {i}\n")).collect();
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);

    // 줄 거부, 없는 피어 이름일 때도 stdout 은 비어 있다.
    let out = p.a.run_via("b", "echo hi", b"");
    assert_eq!(out.code(), Some(255));
    assert!(out.stdout.is_empty(), "거부인데 stdout 에 {:?}", out.stdout);
    let out = p.a.run_via("nosuch", DEFAULT_LINE, b"");
    assert_eq!(out.code(), Some(255));
    assert!(
        out.stdout.is_empty(),
        "없는 피어인데 stdout 에 {:?}",
        out.stdout
    );

    // 바이트 하나도 앞뒤에 붙지 않는다.
    let out = p.a.run_via("b", DEFAULT_LINE, b"\x00");
    assert_eq!(out.code(), Some(0));
    assert_eq!(out.stdout, b"\x00");
}
