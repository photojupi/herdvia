//! M3 통합 테스트 공용 하네스.
//!
//! 노드 하나 = 임시 폴더 하나(HLINK_DIR, 가짜 HOME, 가짜 herdr-core 가 든 PATH 폴더).
//! 거는 쪽 A 와 받는 쪽 B 를 각자의 HLINK_DIR 로 띄우고, `hlink id` 의 첫 줄(EndpointId)을
//! 읽어 A 에는 B 를 outbound 로, B 에는 A 를 inbound 로 등록한 뒤 `hlink status` 로 연결을 기다린다.
//!
//! 에이전트는 작은 sh 감시 스크립트 밑에서 띄운다. 감시 스크립트는 표준 입력이 닫히면
//! (테스트가 정리하거나 테스트 프로세스가 죽으면) 에이전트를 끝내고 원격 자식 pid 파일을 정리한다.
//! 그래서 static 으로 재사용하는 공유 쌍도 프로세스나 임시 폴더를 남기지 않는다.
//! 표준 라이브러리만 쓴다. 제품 코드에 테스트용 뒷문을 요구하지 않는다.

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub const HLINK: &str = env!("CARGO_BIN_EXE_hlink");

/// hc 0.1.21이 실제로 넘기는 기본 connect 줄 (DESIGN.md 그대로).
pub const DEFAULT_LINE: &str = r#"sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x "$p" ] || { echo "herdr-core is not installed here" >&2; exit 127; }; exec "$p" connect'"#;

pub const NOT_INSTALLED_MSG: &str = "herdr-core is not installed here";

/// 세션을 지정한 connect 줄. 끝부분이 `HC_SESSION=<이름> exec "$p" connect'` 로 바뀐다.
pub fn session_line(name: &str) -> String {
    let tail = r#"exec "$p" connect'"#;
    assert!(DEFAULT_LINE.ends_with(tail));
    let head = &DEFAULT_LINE[..DEFAULT_LINE.len() - tail.len()];
    format!("{head}HC_SESSION={name} {tail}")
}

/// 가짜 herdr-core. @ROOT@ 는 노드 임시 폴더로 바뀐다. HC_SESSION 으로 동작을 고른다.
const FAKE_HERDR_CORE: &str = r#"#!/bin/sh
echo "${HC_SESSION:-<default>}" >> "@ROOT@/invoked"
if [ "$1" != connect ] || [ "$#" -ne 1 ]; then
  echo "fake herdr-core: bad args: $*" >&2
  exit 99
fi
case "$HC_SESSION" in
  ""|cat)
    exec cat ;;
  catexit3)
    cat
    exit 3 ;;
  exit7)
    exit 7 ;;
  sigterm)
    kill -TERM $$
    sleep 5
    exit 1 ;;
  stderr)
    i=0
    while [ $i -lt 300 ]; do
      echo "out $i"
      echo "err $i" >&2
      i=$((i+1))
    done
    exit 0 ;;
  big)
    cat "@ROOT@/pattern.bin" &
    cat > "@ROOT@/stdin_copy"
    wait
    exit 0 ;;
  sleep)
    sleep 1000 &
    echo $! > "@ROOT@/grandchild.pid"
    echo $$ > "@ROOT@/child.pid"
    echo ready
    wait
    exit 0 ;;
  *)
    echo "fake herdr-core: unknown mode $HC_SESSION" >&2
    exit 98 ;;
esac
"#;

/// 에이전트 감시 스크립트. $1=hlink, $2=노드 폴더, $3=rm 이면 끝날 때 노드 폴더를 지운다.
/// 첫 줄로 에이전트 pid 를 쓰고, 표준 입력이 닫힐 때까지 기다린다.
const AGENT_WRAPPER: &str = r#"
"$1" agent </dev/null >>"$2/agent.out" 2>>"$2/agent.err" &
a=$!
echo "$a"
read _x
kill "$a" 2>/dev/null
wait "$a" 2>/dev/null
for f in "$2/child.pid" "$2/grandchild.pid"; do
  [ -f "$f" ] && kill -9 "$(cat "$f")" 2>/dev/null
done
[ "$3" = rm ] && rm -rf "$2"
exit 0
"#;

// ---------------------------------------------------------------------------
// 임시 폴더
// ---------------------------------------------------------------------------

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        // 소켓 경로 길이 제한(macOS 104바이트) 때문에 이름을 짧게 둔다.
        let p = std::env::temp_dir().join(format!("hl3-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// 실행 결과
// ---------------------------------------------------------------------------

pub struct Out {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Out {
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// 명령을 실행하고 끝나기를 기다린다. 시간 안에 안 끝나면 죽이고 None.
pub fn try_run(mut cmd: Command, timeout: Duration) -> Option<Out> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("명령 실행 실패");
    let out_rx = spawn_reader(child.stdout.take().unwrap());
    let err_rx = spawn_reader(child.stderr.take().unwrap());
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    Some(Out {
        status,
        stdout: drain(&out_rx, "stdout"),
        stderr: drain(&err_rx, "stderr"),
    })
}

// ---------------------------------------------------------------------------
// 노드
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Fake {
    /// 가짜 herdr-core 를 에이전트 PATH 맨 앞 폴더에 둔다.
    InPath,
    /// 가짜 herdr-core 를 $HOME/.local/bin 에만 둔다.
    InHome,
    /// 어디에도 두지 않는다.
    Nowhere,
}

struct AgentProc {
    wrapper: Child,
    stdin: Option<ChildStdin>,
    pid: String,
}

pub struct Node {
    pub label: String,
    pub root: PathBuf,
    pub hlink_dir: PathBuf,
    pub home: PathBuf,
    pub bin: PathBuf,
    pub path_env: String,
    agent: Option<AgentProc>,
    remove_root_on_exit: bool,
    _tmp: TempDir,
}

impl Node {
    pub fn new(label: &str) -> Self {
        Self::create(label, false)
    }

    /// persistent: static 에 두고 끝내 drop 되지 않는 노드. 감시 스크립트가 폴더까지 지운다.
    fn create(label: &str, persistent: bool) -> Self {
        let tmp = TempDir::new();
        let root = tmp.0.clone();
        let hlink_dir = root.join("hl");
        let home = root.join("home");
        let bin = root.join("bin");
        fs::create_dir(&hlink_dir).unwrap();
        fs::set_permissions(&hlink_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&home).unwrap();
        fs::create_dir(&bin).unwrap();
        let path_env = format!("{}:/usr/bin:/bin", bin.display());
        let n = Node {
            label: label.to_string(),
            root,
            hlink_dir,
            home,
            bin,
            path_env,
            agent: None,
            remove_root_on_exit: persistent,
            _tmp: tmp,
        };
        assert!(
            n.sock().as_os_str().len() < 100,
            "소켓 경로가 너무 김: {}",
            n.sock().display()
        );
        n.set_fake(Fake::InPath);
        n
    }

    pub fn sock(&self) -> PathBuf {
        self.hlink_dir.join("agent.sock")
    }

    pub fn peers_file(&self) -> PathBuf {
        self.hlink_dir.join("peers")
    }

    pub fn key_file(&self) -> PathBuf {
        self.hlink_dir.join("secret.key")
    }

    /// 가짜 herdr-core 의 위치를 바꾼다. 에이전트가 떠 있어도 다음 실행부터 반영된다.
    pub fn set_fake(&self, fake: Fake) {
        let in_path = self.bin.join("herdr-core");
        let home_bin = self.home.join(".local/bin");
        let in_home = home_bin.join("herdr-core");
        let _ = fs::remove_file(&in_path);
        let _ = fs::remove_file(&in_home);
        let script = FAKE_HERDR_CORE.replace("@ROOT@", self.root.to_str().unwrap());
        let install = |p: &Path| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, &script).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        match fake {
            Fake::InPath => install(&in_path),
            Fake::InHome => install(&in_home),
            Fake::Nowhere => {}
        }
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HLINK_DIR", &self.hlink_dir)
            .env("PATH", &self.path_env)
            .env("HOME", &self.home)
            .env_remove("HC_SESSION")
            .env_remove("RUST_LOG");
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(HLINK);
        c.args(args);
        self.env(&mut c);
        c
    }

    /// `hlink <args>` 를 실행한다 (agent·via 제외 단발 명령용).
    pub fn hlink(&self, args: &[&str]) -> Out {
        try_run(self.cmd(args), Duration::from_secs(30))
            .unwrap_or_else(|| panic!("[{}] hlink {args:?} 가 30초 안에 끝나지 않음", self.label))
    }

    /// `hlink id` 의 (EndpointId, 지문).
    pub fn id(&self) -> (String, String) {
        let o = self.hlink(&["id"]);
        assert_eq!(o.code(), Some(0), "[{}] hlink id 실패. stderr: {}", self.label, o.err());
        let s = o.out();
        let mut lines = s.lines();
        let id = lines.next().unwrap_or("").trim().to_string();
        let fp = lines.next().unwrap_or("").trim().to_string();
        assert!(!id.is_empty(), "[{}] hlink id 출력이 비어 있음", self.label);
        (id, fp)
    }

    pub fn allow(&self, name: &str, id: &str, flag: &str) {
        let o = self.hlink(&["allow", name, id, flag]);
        assert_eq!(
            o.code(),
            Some(0),
            "[{}] hlink allow {name} {id} {flag} 실패. stderr: {}",
            self.label,
            o.err()
        );
    }

    pub fn revoke(&self, name: &str) {
        let o = self.hlink(&["revoke", name]);
        assert_eq!(
            o.code(),
            Some(0),
            "[{}] hlink revoke {name} 실패. stderr: {}",
            self.label,
            o.err()
        );
    }

    pub fn status(&self) -> Out {
        self.hlink(&["status"])
    }

    pub fn peers_text(&self) -> String {
        fs::read_to_string(self.peers_file()).unwrap_or_default()
    }

    // --- 에이전트 -----------------------------------------------------------

    pub fn agent_running(&self) -> bool {
        self.agent.as_ref().map(|a| process_alive(&a.pid)).unwrap_or(false)
    }

    pub fn start_agent(&mut self) {
        assert!(self.agent.is_none(), "[{}] 에이전트가 이미 떠 있음", self.label);
        let mut c = Command::new("/bin/sh");
        c.arg("-c")
            .arg(AGENT_WRAPPER)
            .arg("hlink-agent-wrapper")
            .arg(HLINK)
            .arg(&self.root)
            .arg(if self.remove_root_on_exit { "rm" } else { "keep" });
        self.env(&mut c);
        let mut w = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("에이전트 감시 스크립트 실행 실패");
        let stdin = w.stdin.take();
        let mut line = String::new();
        BufReader::new(w.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid = line.trim().to_string();
        assert!(!pid.is_empty(), "[{}] 에이전트 pid 를 받지 못함", self.label);
        self.agent = Some(AgentProc {
            wrapper: w,
            stdin,
            pid: pid.clone(),
        });

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if !process_alive(&pid) {
                panic!(
                    "[{}] hlink agent 가 바로 끝남\n로그:\n{}",
                    self.label,
                    self.agent_log()
                );
            }
            if self.sock().exists() && UnixStream::connect(self.sock()).is_ok() {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "[{}] hlink agent 소켓이 15초 안에 준비되지 않음\n로그:\n{}",
                    self.label,
                    self.agent_log()
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 에이전트에 시그널을 보내 죽인다 (기본 KILL). 감시 스크립트도 거둔다.
    pub fn kill_agent(&mut self, sig: &str) {
        let Some(mut a) = self.agent.take() else { return };
        let _ = Command::new("kill")
            .args([format!("-{sig}"), a.pid.clone()])
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_alive(&a.pid) {
            if Instant::now() > deadline {
                let _ = Command::new("kill")
                    .args(["-9", &a.pid])
                    .stderr(Stdio::null())
                    .status();
            }
            thread::sleep(Duration::from_millis(20));
        }
        drop(a.stdin.take());
        reap(&mut a.wrapper, Duration::from_secs(10));
    }

    /// 감시 스크립트의 표준 입력을 닫아 에이전트를 TERM 으로 끝낸다.
    pub fn stop_agent(&mut self) {
        let Some(mut a) = self.agent.take() else { return };
        drop(a.stdin.take());
        if !reap(&mut a.wrapper, Duration::from_secs(15)) {
            let _ = Command::new("kill")
                .args(["-9", &a.pid])
                .stderr(Stdio::null())
                .status();
            let _ = a.wrapper.kill();
            let _ = a.wrapper.wait();
        }
    }

    pub fn agent_log(&self) -> String {
        let mut s = String::new();
        for f in ["agent.out", "agent.err"] {
            if let Ok(b) = fs::read(self.root.join(f)) {
                s.push_str(&String::from_utf8_lossy(&b));
            }
        }
        if let Ok(rd) = fs::read_dir(&self.hlink_dir) {
            for e in rd.flatten() {
                if e.file_name() == "secret.key" {
                    continue;
                }
                if e.file_type().map(|t| t.is_file()).unwrap_or(false)
                    && let Ok(b) = fs::read(e.path())
                {
                    s.push_str(&String::from_utf8_lossy(&b));
                }
            }
        }
        s
    }

    // --- via ---------------------------------------------------------------

    pub fn spawn_via(&self, peer: &str, line: &str) -> Via {
        let mut c = Command::new(HLINK);
        c.arg("via").arg(peer).arg(line);
        self.env(&mut c);
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hlink via 실행 실패");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let err_rx = spawn_reader(child.stderr.take().unwrap());
        Via {
            child,
            stdin,
            stdout,
            out_rx: None,
            err_rx,
            pending: Vec::new(),
        }
    }

    /// 입력을 다 쓰고 표준 입력을 닫은 뒤 via 가 끝나기를 기다린다.
    pub fn run_via(&self, peer: &str, line: &str, input: &[u8]) -> Out {
        self.run_via_timeout(peer, line, input, Duration::from_secs(30))
    }

    pub fn run_via_timeout(&self, peer: &str, line: &str, input: &[u8], t: Duration) -> Out {
        let mut v = self.spawn_via(peer, line);
        let _done = v.feed(input.to_vec());
        v.finish(t)
    }

    // --- 가짜 herdr-core 흔적 ----------------------------------------------

    pub fn invoked(&self) -> Option<String> {
        fs::read_to_string(self.root.join("invoked")).ok()
    }

    pub fn read_pid(&self, name: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = fs::read_to_string(self.root.join(name)) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            if Instant::now() > deadline {
                panic!("[{}] {name} 를 읽지 못함", self.label);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 이전 검증의 흔적을 지운다. 남은 원격 프로세스는 pid 파일로 정리한다.
    pub fn clear_state(&self) {
        kill_pid_files(&self.root);
        for f in ["invoked", "child.pid", "grandchild.pid", "PWNED", "stdin_copy", "pattern.bin"] {
            let _ = fs::remove_file(self.root.join(f));
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop_agent();
        // 테스트가 실패했거나 구현이 정리를 못 했을 때 남은 원격 프로세스를 치운다.
        kill_pid_files(&self.root);
    }
}

fn kill_pid_files(root: &Path) {
    for f in ["child.pid", "grandchild.pid"] {
        if let Ok(pid) = fs::read_to_string(root.join(f)) {
            let pid = pid.trim();
            if !pid.is_empty() {
                let _ = Command::new("kill")
                    .args(["-9", pid])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

fn reap(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// `hlink agent` 를 직접 띄워 시작을 거부하는지 본다. 10초 안에 0이 아닌 코드로 끝나야 한다.
pub fn expect_agent_refused(n: &Node, what: &str) -> Out {
    let o = try_run(n.cmd(&["agent"]), Duration::from_secs(10))
        .unwrap_or_else(|| panic!("[{what}] 에이전트가 시작을 거부하지 않고 계속 돎"));
    assert_ne!(o.code(), Some(0), "[{what}] 에이전트가 코드 0으로 끝남. stderr: {}", o.err());
    assert!(!o.err().trim().is_empty(), "[{what}] 거부 사유가 stderr 에 없음");
    o
}

// ---------------------------------------------------------------------------
// status 해석
// ---------------------------------------------------------------------------

/// status 출력에서 피어 이름이 낱말로 들어 있는 줄.
pub fn status_line<'a>(out: &'a str, peer: &str) -> Option<&'a str> {
    out.lines().find(|l| {
        l.split(|c: char| c.is_whitespace() || matches!(c, ',' | ':' | '|' | '=' | '[' | ']' | '(' | ')'))
            .any(|t| t == peer)
    })
}

pub const PATH_WORDS: [&str; 4] = ["direct", "relay", "직접", "중계"];

/// status 한 줄이 "연결됨" 을 뜻하는지. 형식이 정해져 있지 않아 흔한 표현을 받는다.
pub fn says_connected(line: &str) -> bool {
    let l = line.to_lowercase();
    let negative = [
        "disconnected",
        "not connected",
        "unconnected",
        "offline",
        "끊김",
        "끊어짐",
        "미연결",
        "연결 안",
        "연결안",
    ];
    if negative.iter().any(|n| l.contains(n)) {
        return false;
    }
    ["connected", "online", "연결됨"]
        .iter()
        .chain(PATH_WORDS.iter())
        .any(|p| l.contains(p))
}

/// `node` 의 status 가 `peer` 를 연결됨으로 보일 때까지 기다린다.
/// status 형식을 해석하지 못하면 via 왕복으로 연결을 확인하고 경고만 남긴다
/// (형식 자체는 status 전용 테스트가 따로 본다).
pub fn wait_connected(node: &Node, peer: &str, callee: Option<&Node>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last: String;
    loop {
        let o = node.status();
        last = format!("코드 {:?}\nstdout:\n{}stderr:\n{}", o.code(), o.out(), o.err());
        if o.code() == Some(0)
            && let Some(l) = status_line(&o.out(), peer)
            && says_connected(l)
        {
            return;
        }
        if Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    if let Some(callee) = callee {
        for _ in 0..3 {
            let o = node.run_via(peer, &session_line("cat"), b"probe");
            if o.code() == Some(0) && o.stdout == b"probe" {
                eprintln!(
                    "경고: [{}] status 가 {peer} 를 연결됨으로 보이지 않지만 via 왕복은 됨. 마지막 status:\n{last}",
                    node.label
                );
                callee.clear_state();
                return;
            }
        }
    }
    panic!(
        "[{}] {timeout:?} 안에 {peer} 와 연결되지 않음. 마지막 status:\n{last}\n{} 로그:\n{}{}",
        node.label,
        node.label,
        node.agent_log(),
        callee
            .map(|c| format!("\n{} 로그:\n{}", c.label, c.agent_log()))
            .unwrap_or_default()
    );
}

// ---------------------------------------------------------------------------
// 쌍
// ---------------------------------------------------------------------------

/// 거는 쪽 A 와 받는 쪽 B. A 의 피어 이름 "b", B 의 피어 이름 "a".
pub struct Pair {
    pub a: Node,
    pub b: Node,
    pub a_id: String,
    pub b_id: String,
}

impl Pair {
    /// A: b=outbound, B: a=inbound 로 새로 띄우고 연결까지 기다린다.
    pub fn new() -> Self {
        Self::build("--outbound", "--inbound", false)
    }

    /// a_grants_b: A 가 B 에게 주는 권한 플래그, b_grants_a: B 가 A 에게 주는 권한 플래그.
    pub fn build(a_grants_b: &str, b_grants_a: &str, persistent: bool) -> Self {
        let mut a = Node::create("A", persistent);
        let mut b = Node::create("B", persistent);
        let (a_id, _) = a.id();
        let (b_id, _) = b.id();
        a.allow("b", &b_id, a_grants_b);
        b.allow("a", &a_id, b_grants_a);
        b.start_agent();
        a.start_agent();
        let p = Pair { a, b, a_id, b_id };
        let a_out = a_grants_b != "--inbound";
        let b_in = b_grants_a != "--outbound";
        if a_out && b_in {
            wait_connected(&p.a, "b", Some(&p.b), Duration::from_secs(30));
        }
        p
    }

    fn reset(&self) {
        for n in [&self.a, &self.b] {
            assert!(
                n.agent_running(),
                "공유 쌍의 {} 에이전트가 죽어 있음. 로그:\n{}",
                n.label,
                n.agent_log()
            );
            n.clear_state();
            n.set_fake(Fake::InPath);
        }
    }
}

static SHARED: OnceLock<Mutex<Pair>> = OnceLock::new();

/// 테스트 바이너리 하나에서 재사용하는 A→B 쌍. 쓰는 동안 잠가서 검증끼리 섞이지 않게 한다.
/// 공유 쌍을 쓰는 테스트는 에이전트를 죽이거나 피어 목록을 바꾸지 않는다.
pub fn shared() -> MutexGuard<'static, Pair> {
    let m = SHARED.get_or_init(|| Mutex::new(Pair::build("--outbound", "--inbound", true)));
    let g = m.lock().unwrap_or_else(|e| e.into_inner());
    g.reset();
    g
}

// ---------------------------------------------------------------------------
// via
// ---------------------------------------------------------------------------

pub fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}

fn drain(rx: &Receiver<Vec<u8>>, what: &str) -> Vec<u8> {
    let mut v = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(c) => v.extend_from_slice(&c),
            Err(RecvTimeoutError::Disconnected) => return v,
            Err(RecvTimeoutError::Timeout) => {
                panic!("프로세스가 끝났는데 {what} 파이프가 10초 안에 닫히지 않음")
            }
        }
    }
}

pub struct Via {
    pub child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    out_rx: Option<Receiver<Vec<u8>>>,
    err_rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Via {
    /// 표준 출력 읽기를 시작한다. 시작 전에는 아무도 읽지 않아 via 의 표준 출력이 막힌다.
    pub fn start_reading_stdout(&mut self) {
        if self.out_rx.is_none() {
            self.out_rx = Some(spawn_reader(self.stdout.take().unwrap()));
        }
    }

    pub fn write(&mut self, data: &[u8]) {
        let s = self.stdin.as_mut().expect("표준 입력이 이미 닫힘");
        s.write_all(data).expect("via 표준 입력 쓰기 실패");
        s.flush().unwrap();
    }

    /// 별도 스레드에서 입력을 다 쓰고 표준 입력을 닫는다. 끝나면 신호가 온다.
    pub fn feed(&mut self, data: Vec<u8>) -> Receiver<()> {
        let mut s = self.stdin.take().expect("표준 입력이 이미 닫힘");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = s.write_all(&data);
            drop(s);
            let _ = tx.send(());
        });
        rx
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub fn read_stdout_exact(&mut self, n: usize, timeout: Duration) -> Vec<u8> {
        self.start_reading_stdout();
        let deadline = Instant::now() + timeout;
        while self.pending.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.out_rx.as_ref().unwrap().recv_timeout(left) {
                Ok(c) => self.pending.extend_from_slice(&c),
                Err(RecvTimeoutError::Timeout) => panic!(
                    "via 표준 출력에서 {n}바이트를 {timeout:?} 안에 받지 못함 (받은 것 {}바이트)",
                    self.pending.len()
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    let err: Vec<u8> = self.err_rx.try_iter().flatten().collect();
                    panic!(
                        "via 표준 출력이 {n}바이트 전에 닫힘: {:?}, stderr: {}",
                        String::from_utf8_lossy(&self.pending),
                        String::from_utf8_lossy(&err)
                    )
                }
            }
        }
        self.pending.drain(..n).collect()
    }

    pub fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    pub fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("via 가 {timeout:?} 안에 끝나지 않음");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn finish(mut self, timeout: Duration) -> Out {
        self.close_stdin();
        self.start_reading_stdout();
        let status = self.wait(timeout);
        let mut stdout = std::mem::take(&mut self.pending);
        stdout.extend(drain(self.out_rx.as_ref().unwrap(), "stdout"));
        let stderr = drain(&self.err_rx, "stderr");
        Out {
            status,
            stdout,
            stderr,
        }
    }

    /// 표준 입력을 열어 둔 채 via 가 끝나기를 기다린다 (원격에서 끊긴 경우를 볼 때).
    pub fn wait_end_keep_stdin(mut self, timeout: Duration) -> Out {
        self.start_reading_stdout();
        let status = self.wait(timeout);
        let mut stdout = std::mem::take(&mut self.pending);
        stdout.extend(drain(self.out_rx.as_ref().unwrap(), "stdout"));
        let stderr = drain(&self.err_rx, "stderr");
        Out {
            status,
            stdout,
            stderr,
        }
    }
}

impl Drop for Via {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// 잡동사니
// ---------------------------------------------------------------------------

/// 큰 바이트열을 비교하되 실패 시 수 MB 를 찍지 않는다.
pub fn assert_bytes_eq(actual: &[u8], expected: &[u8], what: &str) {
    if actual == expected {
        return;
    }
    let first_diff = actual
        .iter()
        .zip(expected)
        .position(|(a, b)| a != b)
        .unwrap_or(actual.len().min(expected.len()));
    panic!(
        "{what}: 바이트 불일치. 길이 실제 {} / 기대 {}, 첫 차이 위치 {}",
        actual.len(),
        expected.len(),
        first_diff
    );
}

pub fn all_bytes(repeat: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(256 * repeat);
    for _ in 0..repeat {
        v.extend(0u8..=255);
    }
    v
}

pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| ((i.wrapping_mul(2654435761).wrapping_add(seed)) >> 13) as u8)
        .collect()
}

pub fn process_alive(pid: &str) -> bool {
    let o = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let s = s.trim();
    !s.is_empty() && !s.starts_with('Z')
}

/// 주어진 프로세스들이 모두 사라질 때까지 기다린다.
pub fn wait_gone(pids: &[&str], timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while pids.iter().any(|p| process_alive(p)) {
        if Instant::now() > deadline {
            let alive: Vec<&&str> = pids.iter().filter(|p| process_alive(p)).collect();
            panic!("[{what}] {timeout:?} 가 지나도 원격 프로세스가 남음: {alive:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn mode_of(p: &Path) -> u32 {
    fs::symlink_metadata(p)
        .unwrap_or_else(|e| panic!("{} 메타데이터 읽기 실패: {e}", p.display()))
        .permissions()
        .mode()
        & 0o777
}

/// EndpointId 문자열 앞 16자를 4자씩 `-` 로 묶은 지문.
pub fn fingerprint_of(id: &str) -> String {
    format!("{}-{}-{}-{}", &id[0..4], &id[4..8], &id[8..12], &id[12..16])
}

/// peers 파일에 `이름 EndpointId 권한` 줄이 있는지.
pub fn has_peer_line(peers: &str, name: &str, id: &str, perm: &str) -> bool {
    peers
        .lines()
        .any(|l| l.split_whitespace().collect::<Vec<_>>() == [name, id, perm])
}

/// peers 파일에 그 이름으로 시작하는 줄이 있는지.
pub fn has_peer_name(peers: &str, name: &str) -> bool {
    peers
        .lines()
        .any(|l| l.split_whitespace().next() == Some(name))
}
