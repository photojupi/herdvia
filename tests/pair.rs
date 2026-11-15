#![cfg(unix)]

mod common;

use common::{Node, session_line, wait_connected};
use std::{
    fs,
    io::{BufReader, Write},
    os::unix::fs::PermissionsExt,
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

static LAN_LOCK: Mutex<()> = Mutex::new(());

struct Interactive {
    child: Child,
    stdin: Option<ChildStdin>,
    lines_rx: mpsc::Receiver<String>,
    err_rx: mpsc::Receiver<Vec<u8>>,
    collected_out: String,
}

impl Interactive {
    fn spawn(node: &Node, args: &[&str]) -> Self {
        let mut cmd = node.cmd(args);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn herdvia");
        let stdin = child.stdin.take();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let err_rx = common::spawn_reader(child.stderr.take().unwrap());
        let (tx, lines_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut buf = Vec::new();
                // Read until '\n' OR ':' (for prompts like `Allow? [y/N]: ` that have no trailing newline).
                let mut byte = [0u8; 1];
                loop {
                    match std::io::Read::read(&mut reader, &mut byte) {
                        Ok(0) => break,
                        Ok(_) => {
                            buf.push(byte[0]);
                            if byte[0] == b'\n' || byte[0] == b':' {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if buf.is_empty() {
                    break;
                }
                if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines_rx,
            err_rx,
            collected_out: String::new(),
        }
    }

    fn wait_for_substr(&mut self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.collected_out.contains(needle) {
                return self.collected_out.clone();
            }
            let remain = deadline.saturating_duration_since(Instant::now());
            match self
                .lines_rx
                .recv_timeout(remain.min(Duration::from_millis(200)))
            {
                Ok(chunk) => self.collected_out.push_str(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if self.collected_out.contains(needle) {
            return self.collected_out.clone();
        }
        panic!(
            "timed out waiting for {needle:?} in stdout.\ncollected stdout:\n{}\ncollected stderr:\n{}",
            self.collected_out,
            self.drain_stderr_now()
        );
    }

    fn send_line(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn drain_stderr_now(&self) -> String {
        let mut bytes = Vec::new();
        while let Ok(chunk) = self.err_rx.try_recv() {
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn finish(mut self, timeout: Duration) -> (i32, String, String) {
        drop(self.stdin.take());
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                break st;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "process did not exit within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                    self.collected_out,
                    self.drain_stderr_now()
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        while let Ok(chunk) = self.lines_rx.recv_timeout(Duration::from_secs(2)) {
            self.collected_out.push_str(&chunk);
        }
        let mut err_bytes = Vec::new();
        while let Ok(chunk) = self.err_rx.recv_timeout(Duration::from_secs(2)) {
            err_bytes.extend_from_slice(&chunk);
        }
        (
            status.code().unwrap_or(-1),
            self.collected_out,
            String::from_utf8_lossy(&err_bytes).into_owned(),
        )
    }
}

fn extract_sas(text: &str) -> String {
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once("Verification code:") {
            let code = rest.trim();
            if code.len() == 7 {
                return code.to_owned();
            }
        }
    }
    panic!("no Verification code found in:\n{text}");
}

fn extract_invite(text: &str) -> String {
    for word in text.split_whitespace() {
        if word.starts_with("hvi1-") {
            return word.to_owned();
        }
    }
    panic!("no hvi1- invite token found in:\n{text}");
}

fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn install_fake_hc(node: &Node) -> std::path::PathBuf {
    let record = node.root.join("hc_args");
    let hc_path = node.bin.join("hc");
    let quoted_probe = sh_single_quote(&session_line("exit7"));
    let script = format!(
        "#!/bin/sh\nset -u\nprintf '%s\\n' \"$@\" > {}\nif [ \"$1\" = machine ] && [ \"$2\" = add ] && [ \"$3\" = --via ]; then\n  q={}\n  rc=0\n  sh -c \"$4 $q\" </dev/null >/dev/null || rc=$?\n  [ \"$rc\" -eq 7 ] || {{ echo \"unexpected probe rc: $rc\" >&2; exit 42; }}\nfi\nexit 0\n",
        sh_single_quote(&record.to_string_lossy()),
        sh_single_quote(&quoted_probe),
    );
    fs::write(&hc_path, script).unwrap();
    fs::set_permissions(&hc_path, fs::Permissions::from_mode(0o755)).unwrap();
    record
}

#[test]
fn listen_and_connect_fail_without_running_agent() {
    let node = Node::new("offline");
    for args in [&["listen"][..], &["connect", "home"]] {
        let out = node.herdvia(args);
        assert_eq!(out.code(), Some(255), "{args:?} stderr: {}", out.err());
        assert!(out.err().starts_with("herdvia: "), "stderr: {}", out.err());
        assert!(
            out.err().contains("agent is not running"),
            "stderr: {}",
            out.err()
        );
    }
}

#[test]
fn connect_refuses_duplicate_peer_name_before_dialing() {
    let mut a = Node::new("dup-a");
    let other_id = Node::new("dup-b").id().0;
    a.allow("home", &other_id, "--outbound");
    a.start_agent();

    let started = Instant::now();
    let out = a.herdvia(&["connect", "home"]);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(out.code(), Some(255));
    assert!(out.err().contains("already registered"), "{}", out.err());
}

#[test]
fn lan_discovery_pairs_two_local_agents_registers_hc_and_passes_traffic() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("lan-a");
    let mut b = Node::new("lan-b");
    let (a_id, _) = a.id();
    let (b_id, _) = b.id();
    let hc_record = install_fake_hc(&a);

    b.start_agent();
    a.start_agent();

    let mut listen_proc = Interactive::spawn(&b, &["listen"]);
    listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));

    let started = Instant::now();
    let mut connect_proc = Interactive::spawn(&a, &["connect", "home"]);

    let a_out_so_far = connect_proc.wait_for_substr(
        "Waiting for approval on the other PC...",
        Duration::from_secs(10),
    );
    let b_out_so_far = listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(10));

    let sas_a = extract_sas(&a_out_so_far);
    let sas_b = extract_sas(&b_out_so_far);
    assert_eq!(sas_a, sas_b);

    listen_proc.send_line("y\n");

    let (b_code, _b_out, b_err) = listen_proc.finish(Duration::from_secs(10));
    let (a_code, _a_out, a_err) = connect_proc.finish(Duration::from_secs(10));
    assert_eq!(b_code, 0, "listen stderr: {b_err}");
    assert_eq!(a_code, 0, "connect stderr: {a_err}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "LAN pairing should finish quickly without 30s backoff: {:?}",
        started.elapsed()
    );

    assert_eq!(a.peers_text().trim(), format!("home {b_id} out"));
    let b_peers = b.peers_text();
    assert!(
        b_peers.contains(&format!(" {a_id} in")),
        "unexpected B peers: {b_peers}"
    );

    let hc_args: Vec<_> = fs::read_to_string(&hc_record)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(hc_args.len(), 6, "hc_args: {hc_args:?}");
    assert_eq!(&hc_args[0..3], &["machine", "add", "--via"]);
    assert!(hc_args[3].ends_with(" via home"), "via arg: {}", hc_args[3]);
    assert_eq!(&hc_args[4..6], &["--name", "home"]);

    let via_out = a.run_via("home", &session_line("cat"), b"paired over lan");
    assert_eq!(via_out.code(), Some(0), "via stderr: {}", via_out.err());
    assert_eq!(via_out.stdout, b"paired over lan");
}

#[test]
fn declining_or_eof_on_listen_leaves_peers_empty_and_exits_one() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("dec-a");
    let mut b = Node::new("dec-b");
    b.start_agent();
    a.start_agent();

    let mut listen_proc = Interactive::spawn(&b, &["listen"]);
    listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));

    let mut connect_proc = Interactive::spawn(&a, &["connect", "home"]);
    connect_proc.wait_for_substr(
        "Waiting for approval on the other PC...",
        Duration::from_secs(10),
    );
    listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(10));

    // Close stdin (EOF) without typing 'y'.
    listen_proc.close_stdin();

    let (b_code, _, _) = listen_proc.finish(Duration::from_secs(10));
    let (a_code, _, a_err) = connect_proc.finish(Duration::from_secs(10));
    assert_eq!(b_code, 1);
    assert_eq!(a_code, 1);
    assert!(
        a_err.contains("rejected") || a_err.contains("declined"),
        "{a_err}"
    );
    assert!(a.peers_text().trim().is_empty());
    assert!(b.peers_text().trim().is_empty());
}

#[test]
fn invite_token_pairs_two_agents_over_relay() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("inv-a");
    let mut b = Node::new("inv-b");
    let (a_id, _) = a.id();
    let (b_id, _) = b.id();
    install_fake_hc(&a);

    b.start_agent();
    a.start_agent();

    let mut listen_proc = Interactive::spawn(&b, &["listen", "controller"]);
    let listen_banner = listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));
    let invite = extract_invite(&listen_banner);

    let mut connect_proc = Interactive::spawn(&a, &["connect", "remote-box", &invite]);
    let a_out = connect_proc.wait_for_substr(
        "Waiting for approval on the other PC...",
        Duration::from_secs(30),
    );
    let b_out = listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(30));
    assert_eq!(extract_sas(&a_out), extract_sas(&b_out));

    listen_proc.send_line("yes\n");

    let (b_code, _, b_err) = listen_proc.finish(Duration::from_secs(15));
    let (a_code, _, a_err) = connect_proc.finish(Duration::from_secs(15));
    assert_eq!(b_code, 0, "listen stderr: {b_err}");
    assert_eq!(a_code, 0, "connect stderr: {a_err}");

    assert_eq!(a.peers_text().trim(), format!("remote-box {b_id} out"));
    assert_eq!(b.peers_text().trim(), format!("controller {a_id} in"));

    wait_connected(&a, "remote-box", Some(&b), Duration::from_secs(30));
    let via_out = a.run_via("remote-box", &session_line("cat"), b"paired over relay");
    assert_eq!(via_out.code(), Some(0), "via stderr: {}", via_out.err());
    assert_eq!(via_out.stdout, b"paired over relay");
}

#[test]
fn connect_with_eof_stdin_when_no_lan_peer_cancels_with_exit_one_and_docs_match_contract() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("eof-connect");
    a.start_agent();

    let proc = Interactive::spawn(&a, &["connect", "nobody"]);
    let (code, _, err) = proc.finish(Duration::from_secs(10));
    assert_eq!(code, 1, "connect EOF stderr: {err}");
    assert!(
        err.contains("pairing cancelled"),
        "expected cancellation message, got: {err}"
    );
    assert!(a.peers_text().trim().is_empty());

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in [
        "DESIGN.md",
        "README.md",
        "docs/README.ko.md",
        "docs/README.ja.md",
        "docs/README.zh-CN.md",
    ] {
        let text = fs::read_to_string(root.join(rel)).unwrap();
        assert!(
            !text.contains("`-2`, `-3`") && !text.contains("`-2`、`-3`"),
            "{rel} still documents sequential -2/-3 hostname suffixes instead of EndpointId hex prefix"
        );
        for chunk in text.split("```mermaid\n").skip(1) {
            if let Some((diagram, _)) = chunk.split_once("```")
                && diagram.trim_start().starts_with("sequenceDiagram")
            {
                assert!(
                    !diagram.contains(';') && !diagram.contains('#'),
                    "{rel} sequenceDiagram contains ';' or '#', which breaks GitHub Mermaid parsing"
                );
            }
        }
    }
    let design = fs::read_to_string(root.join("DESIGN.md")).unwrap();
    assert!(
        !design.contains("두 명령 모두 시작할 때 `herdr-core`")
            && !design.contains("비대화형 표준 입력이면 255"),
        "DESIGN.md still contains outdated preflight or stdin EOF exit-code description"
    );
}
