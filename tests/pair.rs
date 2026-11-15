#![cfg(unix)]

mod common;

use common::{Node, session_line, wait_connected};
use herdvia::frame::{CONTROL, ERROR, EXIT, STDIN, STDOUT};
use std::{
    fs,
    io::{BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
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

/// The menu number (with a newline) of the LAN entry showing `fingerprint`.
fn lan_choice(menu: &str, fingerprint: &str) -> String {
    for line in menu.lines() {
        if line.contains(&format!("(fingerprint {fingerprint})"))
            && let Some((number, _)) = line.trim().split_once(')')
        {
            return format!("{number}\n");
        }
    }
    panic!("no LAN entry with fingerprint {fingerprint} in:\n{menu}");
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

/// A fake `hc` for `register`: `machine ls --json` prints `ls_json` (or fails when it is None),
/// `machine add` exits with `add_rc`. Each call's arguments go to the returned file, one per
/// line, followed by `END`.
fn install_register_hc(node: &Node, ls_json: Option<&str>, add_rc: i32) -> std::path::PathBuf {
    let record = node.root.join("hc_calls");
    let ls = match ls_json {
        Some(json) => format!("printf '%s' {}; exit 0", sh_single_quote(json)),
        None => "echo 'hc: no server' >&2; exit 3".into(),
    };
    let script = format!(
        "#!/bin/sh\n{{ printf '%s\\n' \"$@\"; echo END; }} >> {}\ncase \"$1 $2 $3\" in\n  'machine ls --json') {ls} ;;\n  'machine add --via') [ \"$add_rc\" -eq 0 ] || echo 'hc: cannot add' >&2; exit \"$add_rc\" ;;\nesac\nexit 64\n",
        sh_single_quote(&record.to_string_lossy()),
    )
    .replace("\"$add_rc\"", &add_rc.to_string());
    let hc_path = node.bin.join("hc");
    fs::write(&hc_path, script).unwrap();
    fs::set_permissions(&hc_path, fs::Permissions::from_mode(0o755)).unwrap();
    record
}

/// The calls the fake `hc` recorded, each as its arguments.
fn hc_calls(record: &std::path::Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(record).unwrap_or_default();
    text.split_terminator("END\n")
        .map(|call| call.lines().map(str::to_owned).collect())
        .collect()
}

fn json_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A node with `home` paired outbound; no agent runs.
fn register_node(label: &str) -> Node {
    let node = Node::new(label);
    node.allow("home", &Node::new("reg-peer").id().0, "--outbound");
    node
}

#[test]
fn register_adds_the_machine_when_hc_does_not_list_it() {
    // An empty list, a list hc fails to print, and one that cannot be read: each goes on to add.
    for ls in [
        Some(r#"{"machines":[]}"#),
        None,
        Some("not json"),
        Some(r#"{"machines":[{"name":"home","target":"/x/herdvia via home""#),
    ] {
        let node = register_node("reg-add");
        let record = install_register_hc(&node, ls, 0);
        let o = node.herdvia(&["register", "home"]);
        assert_eq!(o.code(), Some(0), "{ls:?} stderr: {}", o.err());
        assert_eq!(o.out(), "Registered machine 'home' in Herdr.\n", "{ls:?}");
        assert_eq!(o.err(), "", "{ls:?}");
        let calls = hc_calls(&record);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], ["machine", "ls", "--json"]);
        assert_eq!(calls[1][..3], ["machine", "add", "--via"]);
        assert_eq!(
            calls[1][3],
            format!("{} via home", common::HERDVIA),
            "{calls:?}"
        );
        assert_eq!(calls[1][4..], ["--name", "home"]);
    }
}

#[test]
fn register_does_nothing_when_herdr_already_has_this_executable() {
    let node = register_node("reg-same");
    // The target exactly as this executable registers it, learned from a first run.
    let record = install_register_hc(&node, Some(r#"{"machines":[]}"#), 0);
    assert_eq!(node.herdvia(&["register", "home"]).code(), Some(0));
    let via = hc_calls(&record)[1][3].clone();
    fs::remove_file(&record).unwrap();

    let ls = format!(
        r#"{{"machines":[{{"name":"other","target":"x via home","via":true}},{{"host":"h","name":"home","session":null,"target":{},"version":"0.1.21","via":true}}]}}"#,
        json_string(&via)
    );
    let record = install_register_hc(&node, Some(&ls), 9);
    let o = node.herdvia(&["register", "home"]);
    assert_eq!(o.code(), Some(0), "stderr: {}", o.err());
    assert_eq!(o.out(), "home is already registered in Herdr\n");
    assert_eq!(o.err(), "");
    assert_eq!(hc_calls(&record), [["machine", "ls", "--json"]]);
}

#[test]
fn register_refuses_a_machine_that_uses_another_executable_or_no_herdvia() {
    let cases = [
        (
            r#"{"name":"home","target":"/old/place/herdvia via home","via":true}"#,
            "herdvia: Herdr's home uses another herdvia executable (/old/place/herdvia); remove it with hc machine rm home, then run herdvia register home\n",
        ),
        (
            r#"{"name":"home","target":"me@home.example","via":false}"#,
            "herdvia: Herdr already has a machine named home that does not go through herdvia to peer home; rename or remove it (hc machine rename / hc machine rm), then run herdvia register home\n",
        ),
        // herdvia, but to another peer: the message must not say it does not use herdvia.
        (
            r#"{"name":"home","target":"/x/herdvia via office","via":true}"#,
            "herdvia: Herdr already has a machine named home that does not go through herdvia to peer home; rename or remove it (hc machine rename / hc machine rm), then run herdvia register home\n",
        ),
    ];
    for (entry, expected) in cases {
        let node = register_node("reg-other");
        let record = install_register_hc(&node, Some(&format!(r#"{{"machines":[{entry}]}}"#)), 0);
        let o = node.herdvia(&["register", "home"]);
        assert_eq!(o.code(), Some(1), "{entry} stderr: {}", o.err());
        assert_eq!(o.out(), "");
        assert_eq!(o.err(), expected);
        assert_eq!(hc_calls(&record), [["machine", "ls", "--json"]], "{entry}");
    }
}

#[test]
fn register_prints_the_manual_command_when_hc_fails_or_is_missing() {
    let node = register_node("reg-fail");
    let record = install_register_hc(&node, Some(r#"{"machines":[]}"#), 2);
    let o = node.herdvia(&["register", "home"]);
    assert_eq!(o.code(), Some(1), "stderr: {}", o.err());
    assert_eq!(o.out(), "");
    let manual = format!(
        "  hc machine add --via '{} via home' --name home\n",
        common::HERDVIA
    );
    assert_eq!(
        o.err(),
        format!(
            "herdvia: failed to register 'home' in Herdr (hc: cannot add). To register manually, run:\n{manual}"
        )
    );
    assert_eq!(hc_calls(&record).len(), 2);

    // No hc on PATH (only the node's bin, /usr/bin, and /bin) or under the node's HOME.
    fs::remove_file(node.bin.join("hc")).unwrap();
    for dir in ["/usr/bin", "/bin"] {
        assert!(
            !std::path::Path::new(dir).join("hc").exists(),
            "{dir}/hc exists; this test would run it"
        );
    }
    let o = node.herdvia(&["register", "home"]);
    assert_eq!(o.code(), Some(1), "stderr: {}", o.err());
    assert_eq!(o.out(), "");
    assert_eq!(
        o.err(),
        format!(
            "herdvia: 'hc' was not found, so 'home' was not added to Herdr. After installing Herdr, run:\n{manual}"
        )
    );
}

#[test]
fn register_says_the_outcome_is_unknown_when_hc_machine_add_times_out() {
    let node = register_node("reg-slow");
    let script = "#!/bin/sh\ncase \"$1 $2 $3\" in\n  'machine ls --json') printf '%s' '{\"machines\":[]}'; exit 0 ;;\n  'machine add --via') exec sleep 60 ;;\nesac\nexit 64\n";
    let hc_path = node.bin.join("hc");
    fs::write(&hc_path, script).unwrap();
    fs::set_permissions(&hc_path, fs::Permissions::from_mode(0o755)).unwrap();

    let started = Instant::now();
    let o = node.herdvia(&["register", "home"]);
    assert!(
        started.elapsed() >= Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(o.code(), Some(1), "stderr: {}", o.err());
    assert_eq!(o.out(), "");
    assert_eq!(
        o.err(),
        format!(
            "herdvia: hc machine add did not finish within 15 seconds, so whether Herdr registered 'home' is unknown; check hc machine ls, and if 'home' is missing, run:\n  hc machine add --via '{} via home' --name home\n",
            common::HERDVIA
        )
    );
}

#[test]
fn register_needs_an_outbound_peer_and_creates_nothing() {
    let node = Node::new("reg-none");
    let record = install_register_hc(&node, Some(r#"{"machines":[]}"#), 0);
    let missing = node.root.join("none");
    let mut c = node.cmd(&["register", "home"]);
    c.env("HERDVIA_DIR", &missing);
    let o = common::try_run(c, Duration::from_secs(10)).expect("register did not finish");
    assert_eq!(o.code(), Some(255), "stdout: {}", o.out());
    assert_eq!(
        o.err(),
        "herdvia: no outbound peer named home; pair it with herdvia connect first\n"
    );
    assert!(!missing.exists(), "register created the data folder");

    node.allow("lap", &Node::new("reg-in").id().0, "--inbound");
    let before = fs::read_dir(&node.herdvia_dir).unwrap().count();
    for name in ["home", "lap"] {
        let o = node.herdvia(&["register", name]);
        assert_eq!(o.code(), Some(255), "{name} stdout: {}", o.out());
        assert_eq!(o.out(), "");
        assert_eq!(
            o.err(),
            format!("herdvia: no outbound peer named {name}; pair it with herdvia connect first\n")
        );
    }
    assert_eq!(fs::read_dir(&node.herdvia_dir).unwrap().count(), before);
    assert!(!record.exists(), "hc ran for a peer that is not outbound");
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
    let (b_id, b_fp) = b.id();
    let hc_record = install_fake_hc(&a);

    b.start_agent();
    a.start_agent();

    let mut listen_proc = Interactive::spawn(&b, &["listen"]);
    listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));

    let started = Instant::now();
    let mut connect_proc = Interactive::spawn(&a, &["connect", "home"]);

    // Even a single LAN candidate is offered as a menu, to be matched against the fingerprint
    // that listen shows.
    let menu = connect_proc.wait_for_substr("or 0]:", Duration::from_secs(10));
    assert!(
        menu.contains("Choose the PC whose fingerprint matches"),
        "menu: {menu}"
    );
    assert!(menu.contains("0) Paste an invite token instead"), "{menu}");
    connect_proc.send_line(&lan_choice(&menu, &b_fp));

    let a_out_so_far = connect_proc.wait_for_substr("[y/N]:", Duration::from_secs(10));
    let b_out_so_far = listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(10));
    assert!(
        b_out_so_far.contains(&format!("This PC's fingerprint: {b_fp}")),
        "listen output: {b_out_so_far}"
    );

    let sas_a = extract_sas(&a_out_so_far);
    let sas_b = extract_sas(&b_out_so_far);
    assert_eq!(sas_a, sas_b);

    connect_proc.send_line("y\n");
    connect_proc.wait_for_substr(
        "Waiting for approval on the other PC...",
        Duration::from_secs(10),
    );
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

/// Two real agents with no relay: once B revokes A, A's next dial is refused, and A's status
/// shows the 403 reason B's agent sent below the usual table.
#[test]
fn status_shows_the_reason_a_peer_gave_for_rejecting_this_pc() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("rej-a");
    let mut b = Node::new("rej-b");
    let (a_id, _) = a.id();
    let b_fp = b.id().1;
    install_fake_hc(&a);
    b.start_agent();
    a.start_agent();

    // Pairing over the LAN gives A the address of B, so A's agent dials it without n0.
    let mut listen_proc = Interactive::spawn(&b, &["listen"]);
    listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));
    let mut connect_proc = Interactive::spawn(&a, &["connect", "home"]);
    let menu = connect_proc.wait_for_substr("or 0]:", Duration::from_secs(10));
    connect_proc.send_line(&lan_choice(&menu, &b_fp));
    connect_proc.wait_for_substr("[y/N]:", Duration::from_secs(10));
    listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(10));
    connect_proc.send_line("y\n");
    connect_proc.wait_for_substr(
        "Waiting for approval on the other PC...",
        Duration::from_secs(10),
    );
    listen_proc.send_line("y\n");
    let (b_code, _, b_err) = listen_proc.finish(Duration::from_secs(10));
    let (a_code, _, a_err) = connect_proc.finish(Duration::from_secs(10));
    assert_eq!(b_code, 0, "listen stderr: {b_err}");
    assert_eq!(a_code, 0, "connect stderr: {a_err}");

    let b_peers = b.peers_text();
    let name_on_b = b_peers
        .lines()
        .find(|l| l.contains(&a_id))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("A is not in B's peers:\n{b_peers}"))
        .to_owned();
    b.revoke(&name_on_b);

    let o = common::wait_status(
        &a,
        "\nhome: rejected by the peer (403 peer not allowed); on home run herdvia pending, then herdvia accept. Retrying in ",
        Duration::from_secs(30),
    );
    assert_eq!(o.err(), "", "stdout: {}", o.out());
    let out = o.out();
    let (table, reasons) = out.split_once("\n\n").expect("no reason lines");
    assert_eq!(
        table, "name permission connection path rtt\nhome out disconnected - -",
        "{out}"
    );
    let lines: Vec<_> = reasons.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    let secs = lines[0]
        .rsplit_once(" Retrying in ")
        .and_then(|(_, rest)| rest.strip_suffix("s."))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(secs.parse::<u64>().is_ok(), "{out}");
    assert_eq!(lines[1], "Run herdvia agent reload to retry now.", "{out}");
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
    let menu = connect_proc.wait_for_substr("or 0]:", Duration::from_secs(10));
    connect_proc.send_line(&lan_choice(&menu, &b.id().1));
    connect_proc.wait_for_substr("[y/N]:", Duration::from_secs(10));
    connect_proc.send_line("y\n");
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
fn a_blank_line_at_the_lan_menu_cancels_and_other_input_is_an_error() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("menu-a");
    let mut b = Node::new("menu-b");
    b.start_agent();
    a.start_agent();

    let mut listen_proc = Interactive::spawn(&b, &["listen"]);
    listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));

    // Enter alone cancels, as EOF does; a number out of range or other text is an error.
    for (input, code, expected) in [
        ("\n", 1, "herdvia: pairing cancelled\n"),
        ("99\n", 255, "herdvia: invalid selection\n"),
        ("x\n", 255, "herdvia: invalid selection\n"),
    ] {
        let mut connect_proc = Interactive::spawn(&a, &["connect", "home"]);
        connect_proc.wait_for_substr("or 0]:", Duration::from_secs(10));
        connect_proc.send_line(input);
        let (got, _, err) = connect_proc.finish(Duration::from_secs(10));
        assert_eq!((got, err.as_str()), (code, expected), "{input:?}");
    }
    assert!(a.peers_text().trim().is_empty());

    let _ = listen_proc.child.kill();
    let _ = listen_proc.child.wait();
}

#[test]
fn approval_on_the_target_saves_nothing_on_the_caller_until_its_user_confirms() {
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut a = Node::new("conf-a");
    let mut b = Node::new("conf-b");
    let (_, b_fp) = b.id();
    let hc_record = install_fake_hc(&a);
    b.start_agent();
    a.start_agent();

    let mut listen_proc = Interactive::spawn(&b, &["listen"]);
    listen_proc.wait_for_substr("hvi1-", Duration::from_secs(10));

    let mut connect_proc = Interactive::spawn(&a, &["connect", "home"]);
    let menu = connect_proc.wait_for_substr("or 0]:", Duration::from_secs(10));
    connect_proc.send_line(&lan_choice(&menu, &b_fp));
    let a_out = connect_proc.wait_for_substr("[y/N]:", Duration::from_secs(10));
    let b_out = listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(10));
    assert_eq!(extract_sas(&a_out), extract_sas(&b_out));

    // The target approves first; its Approved reaches the caller's agent before the caller's
    // user answers.
    listen_proc.send_line("y\n");
    let (b_code, _, b_err) = listen_proc.finish(Duration::from_secs(10));
    assert_eq!(b_code, 0, "listen stderr: {b_err}");
    thread::sleep(Duration::from_millis(500));
    assert!(a.peers_text().trim().is_empty());
    assert!(!hc_record.exists());

    connect_proc.send_line("n\n");
    let (a_code, _, a_err) = connect_proc.finish(Duration::from_secs(10));
    assert_eq!(a_code, 1, "connect stderr: {a_err}");
    assert!(a_err.contains("pairing cancelled"), "{a_err}");
    assert!(a.peers_text().trim().is_empty());
    assert!(
        !hc_record.exists(),
        "hc machine add ran without confirmation"
    );
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
    let a_out = connect_proc.wait_for_substr("[y/N]:", Duration::from_secs(30));
    let b_out = listen_proc.wait_for_substr("Allow? [y/N]:", Duration::from_secs(30));
    assert_eq!(extract_sas(&a_out), extract_sas(&b_out));

    connect_proc.send_line("y\n");
    connect_proc.wait_for_substr(
        "Waiting for approval on the other PC...",
        Duration::from_secs(10),
    );
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

// ---------------------------------------------------------------------------
// 확인 입력과 겹쳐 쪼개져 도착한 에이전트 프레임
// ---------------------------------------------------------------------------

/// 에이전트가 쓰는 대로의 프레임 바이트: 머리(태그, 길이) 다음에 내용.
fn frame_bytes(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// 프레임 하나. 연결이 닫혔으면 None.
fn read_frame(stream: &mut UnixStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).ok()?;
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).unwrap();
    Some((header[0], payload))
}

/// `node` 의 에이전트 대신 소켓에서 `status` 에 답하고, 첫 `connect`/`listen` 세션을 `session` 에 넘긴다.
fn fake_agent(
    node: &Node,
    session: impl FnOnce(UnixStream) + Send + 'static,
) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(node.sock()).unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let Some((tag, request)) = read_frame(&mut stream) else {
                continue;
            };
            assert_eq!(tag, CONTROL);
            match request.as_slice() {
                b"status" => stream
                    .write_all(&frame_bytes(EXIT, &0_i32.to_be_bytes()))
                    .unwrap(),
                b"connect" | b"listen" => return session(stream),
                other => panic!("unexpected request {other:?}"),
            }
        }
    })
}

/// 가짜 에이전트가 `before` 프레임들을 보낸 뒤 `reply` 를 `split` 바이트까지만 보낸다.
/// 그 사이 사용자가 y 를 입력하면 approve 를 받은 뒤 `reply` 의 나머지와 `after` 를 보낸다.
fn split_reply_session(
    before: Vec<Vec<u8>>,
    reply: Vec<u8>,
    split: usize,
    after: Vec<u8>,
    partial_tx: mpsc::Sender<()>,
) -> impl FnOnce(UnixStream) + Send + 'static {
    move |mut stream| {
        let (tag, _) = read_frame(&mut stream).unwrap();
        assert_eq!(tag, STDIN);
        for frame in before {
            stream.write_all(&frame).unwrap();
        }
        let (head, rest) = reply.split_at(split);
        stream.write_all(head).unwrap();
        partial_tx.send(()).unwrap();
        let answer = read_frame(&mut stream).unwrap();
        assert_eq!(answer, (STDIN, b"approve".to_vec()));
        stream.write_all(rest).unwrap();
        stream.write_all(&after).unwrap();
        // CLI 가 끝날 때까지 연결을 열어 둔다.
        let _ = stream.read(&mut [0u8; 1]);
    }
}

/// 프롬프트가 뜨고 일부 프레임이 CLI 에 닿은 뒤 y 를 보내고 종료 코드와 stderr 를 돌려준다.
fn answer_yes_during_a_split_frame(
    mut proc: Interactive,
    prompt: &str,
    partial_rx: mpsc::Receiver<()>,
) -> (i32, String) {
    proc.wait_for_substr(prompt, Duration::from_secs(10));
    partial_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // CLI 가 프레임 앞부분을 읽고 나머지를 기다리는 동안 입력이 도착하게 한다.
    thread::sleep(Duration::from_millis(300));
    proc.send_line("y\n");
    let (code, _, err) = proc.finish(Duration::from_secs(10));
    (code, err)
}

#[test]
fn connect_keeps_an_agent_frame_split_around_the_confirmation_answer() {
    let target: iroh::EndpointId = Node::new("split-t").id().0.parse().unwrap();
    let invite = herdvia::pair::codec::encode_invite(target, &[7; 16]);
    let sas = frame_bytes(STDOUT, b"SAS target 0000-1111-2222-3333 123-4567\n");
    let error = frame_bytes(ERROR, b"target rejected pairing");
    let rejected = frame_bytes(STDOUT, b"REJECTED busy\n");
    // 태그 뒤에서 쪼갠 ERROR, 내용 중간에서 쪼갠 REJECTED(뒤에 EXIT).
    let cases = [
        (
            error,
            1,
            Vec::new(),
            255,
            "herdvia: target rejected pairing\n",
        ),
        (
            rejected,
            8,
            frame_bytes(EXIT, &1_i32.to_be_bytes()),
            1,
            "herdvia: pairing rejected: busy\n",
        ),
    ];
    for (reply, split, after, code, expected) in cases {
        let a = Node::new("split-c");
        let (partial_tx, partial_rx) = mpsc::channel();
        let agent = fake_agent(
            &a,
            split_reply_session(vec![sas.clone()], reply, split, after, partial_tx),
        );
        let proc = Interactive::spawn(&a, &["connect", "home", &invite]);
        let (got, err) =
            answer_yes_during_a_split_frame(proc, "Same code on both PCs? [y/N]:", partial_rx);
        assert_eq!((got, err.as_str()), (code, expected));
        agent.join().unwrap();
        assert!(a.peers_text().trim().is_empty());
    }
}

#[test]
fn listen_keeps_an_agent_frame_split_around_the_allow_answer() {
    // listen 은 READY 를 받으면 LAN 에 비콘을 보낸다.
    let _guard = LAN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let b = Node::new("split-l");
    let id = Node::new("split-t").id().0;
    let ready = format!("READY {id} 1 {} hvi1-unused fake-host\n", "ab".repeat(16));
    let before = vec![
        frame_bytes(STDOUT, ready.as_bytes()),
        frame_bytes(STDOUT, b"PROMPT caller x 0000-1111-2222-3333 123-4567\n"),
    ];
    let (partial_tx, partial_rx) = mpsc::channel();
    let agent = fake_agent(
        &b,
        split_reply_session(
            before,
            frame_bytes(ERROR, b"caller went away"),
            3,
            Vec::new(),
            partial_tx,
        ),
    );
    let proc = Interactive::spawn(&b, &["listen"]);
    let (code, err) = answer_yes_during_a_split_frame(proc, "Allow? [y/N]:", partial_rx);
    assert_eq!(code, 255, "stderr: {err}");
    assert_eq!(
        err.lines().last(),
        Some("herdvia: caller went away"),
        "{err}"
    );
    agent.join().unwrap();
}
