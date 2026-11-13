//! hlink 실행 파일을 바깥에서 검증하는 루프백 통합 테스트 (M2).
//!
//! 각 테스트는 임시 폴더를 HLINK_DIR 로 주고 `hlink agent` 를 띄운 뒤 `hlink via` 를 실행한다.
//! 에이전트가 허용하는 줄은 herdr-core connect 줄 두 형태뿐이므로, 임시 폴더에 가짜
//! `herdr-core` 셸 스크립트를 만들어 에이전트의 PATH 앞에 넣고 HC_SESSION 값으로 동작을 고른다.
//! 표준 라이브러리만 쓴다.

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HLINK: &str = env!("CARGO_BIN_EXE_hlink");

/// hc 0.1.21이 실제로 넘기는 기본 connect 줄 (DESIGN.md 그대로).
const DEFAULT_LINE: &str = r#"sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x "$p" ] || { echo "herdr-core is not installed here" >&2; exit 127; }; exec "$p" connect'"#;

const NOT_INSTALLED_MSG: &str = "herdr-core is not installed here";

/// 세션을 지정한 connect 줄. 끝부분이 `HC_SESSION=<이름> exec "$p" connect'` 로 바뀐다.
fn session_line(name: &str) -> String {
    let tail = r#"exec "$p" connect'"#;
    assert!(DEFAULT_LINE.ends_with(tail));
    let head = &DEFAULT_LINE[..DEFAULT_LINE.len() - tail.len()];
    format!("{head}HC_SESSION={name} {tail}")
}

/// 가짜 herdr-core. @ROOT@ 는 테스트 임시 폴더로 바뀐다.
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

// ---------------------------------------------------------------------------
// 공용 도구
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        // macOS 소켓 경로 길이 제한(104바이트) 때문에 짧은 /tmp 를 쓴다.
        let p = PathBuf::from(format!("/tmp/hlt-{}-{}-{}", std::process::id(), n, nanos));
        fs::create_dir(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fake {
    /// 가짜 herdr-core 를 에이전트 PATH 맨 앞 폴더에 둔다.
    InPath,
    /// 가짜 herdr-core 를 $HOME/.local/bin 에만 둔다.
    InHome,
    /// 어디에도 두지 않는다.
    Nowhere,
}

struct Fixture {
    root: PathBuf,
    hlink_dir: PathBuf,
    home: PathBuf,
    path_env: String,
    agent: Option<Child>,
    _tmp: TempDir,
}

impl Fixture {
    fn new(fake: Fake) -> Self {
        let tmp = TempDir::new();
        let root = tmp.0.clone();
        let hlink_dir = root.join("hl");
        let home = root.join("home");
        let bin = root.join("bin");
        fs::create_dir(&hlink_dir).unwrap();
        fs::set_permissions(&hlink_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&home).unwrap();
        fs::create_dir(&bin).unwrap();

        let script = FAKE_HERDR_CORE.replace("@ROOT@", root.to_str().unwrap());
        let install = |dir: &Path| {
            fs::create_dir_all(dir).unwrap();
            let p = dir.join("herdr-core");
            fs::write(&p, &script).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        match fake {
            Fake::InPath => install(&bin),
            Fake::InHome => install(&home.join(".local/bin")),
            Fake::Nowhere => {}
        }
        let path_env = if fake == Fake::InPath {
            format!("{}:/usr/bin:/bin", bin.display())
        } else {
            "/usr/bin:/bin".to_string()
        };
        Fixture {
            root,
            hlink_dir,
            home,
            path_env,
            agent: None,
            _tmp: tmp,
        }
    }

    fn sock(&self) -> PathBuf {
        self.hlink_dir.join("agent.sock")
    }

    fn start_agent(&mut self) {
        let out = File::create(self.root.join("agent.out")).unwrap();
        let err = File::create(self.root.join("agent.err")).unwrap();
        let child = Command::new(HLINK)
            .arg("agent")
            .env("HLINK_DIR", &self.hlink_dir)
            .env("PATH", &self.path_env)
            .env("HOME", &self.home)
            .env_remove("HC_SESSION")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("hlink agent 실행 실패");
        self.agent = Some(child);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(st) = self.agent.as_mut().unwrap().try_wait().unwrap() {
                panic!("hlink agent 가 바로 끝남: {st}\n로그:\n{}", self.agent_log());
            }
            if self.sock().exists() && UnixStream::connect(self.sock()).is_ok() {
                break;
            }
            if Instant::now() > deadline {
                panic!("hlink agent 소켓이 10초 안에 준비되지 않음\n로그:\n{}", self.agent_log());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_agent(&mut self) {
        if let Some(mut a) = self.agent.take() {
            let _ = a.kill();
            let _ = a.wait();
        }
    }

    fn agent_log(&self) -> String {
        let mut s = String::new();
        for f in ["agent.out", "agent.err"] {
            if let Ok(b) = fs::read(self.root.join(f)) {
                s.push_str(&String::from_utf8_lossy(&b));
            }
        }
        s
    }

    fn spawn_via(&self, line: &str) -> Via {
        let mut child = Command::new(HLINK)
            .arg("via")
            .arg("local")
            .arg(line)
            .env("HLINK_DIR", &self.hlink_dir)
            .env_remove("HC_SESSION")
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
    fn run_via(&self, line: &str, input: &[u8]) -> Out {
        let mut v = self.spawn_via(line);
        let _done = v.feed(input.to_vec());
        v.finish(Duration::from_secs(30))
    }

    fn invoked(&self) -> Option<String> {
        fs::read_to_string(self.root.join("invoked")).ok()
    }

    fn read_pid(&self, name: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = fs::read_to_string(self.root.join(name)) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            if Instant::now() > deadline {
                panic!("{name} 를 읽지 못함");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill_agent();
        // 테스트가 실패했거나 구현이 정리를 못 했을 때 남은 원격 프로세스를 치운다.
        for f in ["child.pid", "grandchild.pid"] {
            if let Ok(pid) = fs::read_to_string(self.root.join(f)) {
                let _ = Command::new("kill")
                    .args(["-9", pid.trim()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

struct Out {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Out {
    fn code(&self) -> Option<i32> {
        self.status.code()
    }
    fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> Receiver<Vec<u8>> {
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
                panic!("via 가 끝났는데 {what} 파이프가 10초 안에 닫히지 않음")
            }
        }
    }
}

struct Via {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    out_rx: Option<Receiver<Vec<u8>>>,
    err_rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Via {
    /// 표준 출력 읽기를 시작한다. 시작 전에는 아무도 읽지 않아 via 의 표준 출력이 막힌다.
    fn start_reading_stdout(&mut self) {
        if self.out_rx.is_none() {
            self.out_rx = Some(spawn_reader(self.stdout.take().unwrap()));
        }
    }

    fn write(&mut self, data: &[u8]) {
        let s = self.stdin.as_mut().expect("표준 입력이 이미 닫힘");
        s.write_all(data).expect("via 표준 입력 쓰기 실패");
        s.flush().unwrap();
    }

    /// 별도 스레드에서 입력을 다 쓰고 표준 입력을 닫는다. 끝나면 신호가 온다.
    fn feed(&mut self, data: Vec<u8>) -> Receiver<()> {
        let mut s = self.stdin.take().expect("표준 입력이 이미 닫힘");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = s.write_all(&data);
            drop(s);
            let _ = tx.send(());
        });
        rx
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn read_stdout_exact(&mut self, n: usize, timeout: Duration) -> Vec<u8> {
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
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "via 표준 출력이 {n}바이트 전에 닫힘: {:?}",
                    String::from_utf8_lossy(&self.pending)
                ),
            }
        }
        self.pending.drain(..n).collect()
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
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

    fn finish(mut self, timeout: Duration) -> Out {
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
}

impl Drop for Via {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 큰 바이트열을 비교하되 실패 시 수 MB 를 찍지 않는다.
fn assert_bytes_eq(actual: &[u8], expected: &[u8], what: &str) {
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

fn all_bytes(repeat: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(256 * repeat);
    for _ in 0..repeat {
        v.extend(0u8..=255);
    }
    v
}

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| ((i.wrapping_mul(2654435761).wrapping_add(seed)) >> 13) as u8)
        .collect()
}

fn process_alive(pid: &str) -> bool {
    let o = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let s = s.trim();
    !s.is_empty() && !s.starts_with('Z')
}

// ---------------------------------------------------------------------------
// (1) 표준 입력이 그대로 원격으로 가고 출력이 그대로 돌아온다
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_all_byte_values_default_line() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let input = all_bytes(4096); // 1 MiB, 0x00~0xFF 전부
    let out = f.run_via(DEFAULT_LINE, &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "기본 줄 cat 왕복");
    assert_eq!(f.invoked().as_deref(), Some("<default>\n"));
}

#[test]
fn roundtrip_all_byte_values_session_line() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let input = all_bytes(64);
    let out = f.run_via(&session_line("cat"), &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "세션 줄 cat 왕복");
    assert_eq!(f.invoked().as_deref(), Some("cat\n"));
}

#[test]
fn interactive_echo_without_closing_stdin() {
    // 묶어 보내지 않고 읽는 즉시 전달하는지: 표준 입력을 열어 둔 채 조각마다 메아리를 받는다.
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let mut v = f.spawn_via(&session_line("cat"));
    for i in 0..32u8 {
        let msg = [i, 0x00, 0xFF, b'\n', 0x80u8.wrapping_add(i)];
        v.write(&msg);
        let got = v.read_stdout_exact(msg.len(), Duration::from_secs(3));
        assert_eq!(got, msg, "조각 {i} 메아리 불일치");
    }
    thread::sleep(Duration::from_millis(300));
    assert!(v.running(), "표준 입력이 열려 있는데 via 가 먼저 끝남");
    let out = v.finish(Duration::from_secs(10));
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert!(out.stdout.is_empty(), "남은 출력이 있으면 안 됨: {:?}", out.stdout);
}

// ---------------------------------------------------------------------------
// (2) 표준 입력을 닫으면 원격이 끝나고 via 도 그 종료 코드로 끝난다
// ---------------------------------------------------------------------------

#[test]
fn closing_stdin_ends_remote_and_via_with_its_code() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let mut v = f.spawn_via(&session_line("catexit3"));
    v.write(b"abc");
    assert_eq!(v.read_stdout_exact(3, Duration::from_secs(3)), b"abc");
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

// ---------------------------------------------------------------------------
// (3) 0이 아닌 종료 코드 전달
// ---------------------------------------------------------------------------

#[test]
fn nonzero_exit_code_is_forwarded() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let out = f.run_via(&session_line("exit7"), b"");
    assert_eq!(out.code(), Some(7), "stderr: {}", out.err());
    assert!(out.stdout.is_empty());
}

#[test]
fn signal_exit_is_forwarded_as_128_plus_n() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let out = f.run_via(&session_line("sigterm"), b"");
    assert_eq!(out.code(), Some(128 + 15), "stderr: {}", out.err());
}

// ---------------------------------------------------------------------------
// (4) herdr-core 가 PATH 와 HOME 모두에 없을 때 127과 stderr 문구
// ---------------------------------------------------------------------------

#[test]
fn missing_herdr_core_gives_127_and_message() {
    for p in ["/usr/bin/herdr-core", "/bin/herdr-core"] {
        assert!(!Path::new(p).exists(), "전제 조건 위반: {p} 가 있음");
    }
    let mut f = Fixture::new(Fake::Nowhere);
    f.start_agent();
    for line in [DEFAULT_LINE.to_string(), session_line("cat")] {
        let out = f.run_via(&line, b"");
        assert_eq!(out.code(), Some(127), "stderr: {}", out.err());
        assert!(
            out.err().contains(NOT_INSTALLED_MSG),
            "stderr 에 미설치 문구가 없음: {:?}",
            out.err()
        );
        assert!(out.stdout.is_empty(), "stdout 에 무언가 섞임: {:?}", out.stdout);
    }
}

#[test]
fn herdr_core_found_via_home_fallback() {
    // PATH 에는 없고 $HOME/.local/bin 에만 있으면 그쪽으로 실행된다 (127 경우의 대조군).
    let mut f = Fixture::new(Fake::InHome);
    f.start_agent();
    let input = all_bytes(4);
    let out = f.run_via(DEFAULT_LINE, &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "HOME 폴백 cat 왕복");
}

// ---------------------------------------------------------------------------
// (5) stdout 과 stderr 가 섞이지 않는다
// ---------------------------------------------------------------------------

#[test]
fn stdout_and_stderr_stay_separate() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let out = f.run_via(&session_line("stderr"), b"");
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
// (6) 허용되지 않은 줄은 255 와 stderr 사유로 거부되고 실행되지 않는다
// ---------------------------------------------------------------------------

#[test]
fn disallowed_lines_are_rejected_and_not_executed() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let marker = f.root.join("PWNED");
    let m = marker.display().to_string();
    let token = "REJECTTOKEN4729";

    let cases: Vec<(&str, String)> = vec![
        ("임의 명령", format!("echo {token}; touch {m}")),
        ("임의 sh -c", format!("sh -c 'touch {m}'")),
        ("빈 줄", String::new()),
        ("connect 대신 다른 하위 명령", DEFAULT_LINE.replace("connect'", "serve'")),
        ("세션에 세미콜론·공백", session_line(&format!("x;touch {m};"))),
        ("세션에 명령 치환", session_line(&format!("$(touch {m})"))),
        ("세션에 백틱", session_line(&format!("`touch {m}`"))),
        ("세션에 슬래시", session_line("a/b")),
        ("세션에 작은따옴표", session_line(&format!("a';touch {m};'"))),
        ("세션에 비 ASCII", session_line("세션")),
        ("빈 세션", session_line("")),
        ("세션 65자", session_line(&"a".repeat(65))),
        ("줄 끝에 명령 덧붙임", format!("{DEFAULT_LINE}; touch {m}")),
        ("줄 끝에 && 덧붙임", format!("{DEFAULT_LINE} && touch {m}")),
        ("줄 끝에 개행", format!("{DEFAULT_LINE}\n")),
        ("줄 끝에 공백", format!("{DEFAULT_LINE} ")),
        ("줄 앞에 공백", format!(" {DEFAULT_LINE}")),
        ("줄 앞에 명령", format!("touch {m}; {DEFAULT_LINE}")),
        ("세션 줄 끝에 덧붙임", format!("{}; touch {m}", session_line("cat"))),
    ];

    for (name, line) in &cases {
        let out = f.run_via(line, b"");
        assert_eq!(
            out.code(),
            Some(255),
            "[{name}] 거부되어야 함. 줄: {line:?}, stderr: {}",
            out.err()
        );
        assert!(!out.err().trim().is_empty(), "[{name}] stderr 에 사유가 없음");
        assert!(out.stdout.is_empty(), "[{name}] stdout 에 무언가 섞임: {:?}", out.stdout);
        thread::sleep(Duration::from_millis(50));
        assert!(!marker.exists(), "[{name}] 거부된 줄이 실행됨 (표지 파일 생김)");
        assert_eq!(f.invoked(), None, "[{name}] 거부된 줄에서 herdr-core 가 실행됨");
    }

    // 거부한 줄은 로그에 남긴다 (에이전트 출력 또는 데이터 폴더 안 파일).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut logs = f.agent_log();
        if let Ok(rd) = fs::read_dir(&f.hlink_dir) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false)
                    && let Ok(b) = fs::read(e.path())
                {
                    logs.push_str(&String::from_utf8_lossy(&b));
                }
            }
        }
        if logs.contains(token) {
            break;
        }
        if Instant::now() > deadline {
            panic!("거부한 줄이 에이전트 로그에 남지 않음. 로그:\n{logs}");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // 거부 뒤에도 에이전트는 정상 동작한다.
    let out = f.run_via(&session_line("cat"), b"still alive");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"still alive");
}

#[test]
fn session_name_boundaries_are_accepted() {
    // 허용 문자 집합과 최대 길이(64)는 받아들여져 실제로 실행된다.
    // 가짜 herdr-core 는 모르는 모드에서 98 로 끝난다.
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let long = "a".repeat(64);
    for name in ["a.b_c-D9", "Z", long.as_str()] {
        let out = f.run_via(&session_line(name), b"");
        assert_eq!(out.code(), Some(98), "[{name}] 허용되어야 함. stderr: {}", out.err());
        assert!(out.err().contains(&format!("unknown mode {name}")));
    }
    let invoked = f.invoked().unwrap_or_default();
    assert_eq!(invoked, format!("a.b_c-D9\nZ\n{long}\n"));
}

// ---------------------------------------------------------------------------
// (7) 에이전트가 없을 때 via 는 기다리지 않고 255
// ---------------------------------------------------------------------------

fn assert_quick_255(f: &Fixture, what: &str) {
    let t = Instant::now();
    let out = f.run_via(DEFAULT_LINE, b"");
    let el = t.elapsed();
    assert_eq!(out.code(), Some(255), "[{what}] stderr: {}", out.err());
    assert!(el < Duration::from_secs(2), "[{what}] via 가 기다림: {el:?}");
    assert!(!out.err().trim().is_empty(), "[{what}] stderr 에 사유가 없음");
    assert!(out.stdout.is_empty(), "[{what}] stdout 에 무언가 섞임");
    assert_eq!(f.invoked(), None);
}

#[test]
fn no_agent_means_immediate_255() {
    let mut f = Fixture::new(Fake::InPath);
    // 데이터 폴더는 있지만 소켓이 없음
    assert_quick_255(&f, "소켓 없음");
    // 데이터 폴더 자체가 없음
    f.hlink_dir = f.root.join("does-not-exist");
    assert_quick_255(&f, "폴더 없음");
}

#[test]
fn stale_socket_means_immediate_255() {
    let f = Fixture::new(Fake::InPath);
    // 소켓 파일은 남아 있지만 듣는 쪽이 없음 (연결 거부)
    drop(UnixListener::bind(f.sock()).unwrap());
    assert!(f.sock().exists());
    assert_quick_255(&f, "죽은 소켓");
}

#[test]
fn agent_dying_midstream_gives_255() {
    // EXIT 없이 끊기면 stderr 에 사유를 쓰고 255
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let mut v = f.spawn_via(&session_line("sleep"));
    assert_eq!(v.read_stdout_exact(6, Duration::from_secs(5)), b"ready\n");
    f.kill_agent();
    let t = Instant::now();
    let out = v.finish(Duration::from_secs(10));
    assert!(t.elapsed() < Duration::from_secs(3), "에이전트가 죽은 뒤 via 가 늦게 끝남");
    assert_eq!(out.code(), Some(255), "stderr: {}", out.err());
    assert!(!out.err().trim().is_empty());
    assert!(out.stdout.is_empty(), "stdout 에 원격 외 바이트가 섞임: {:?}", out.stdout);
}

// ---------------------------------------------------------------------------
// (8) 수 MB 출력이 손실 없이 전달되고 그동안 입력 방향이 막히지 않는다
// ---------------------------------------------------------------------------

#[test]
fn multi_megabyte_output_while_input_flows() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let big_out = pattern(8 * 1024 * 1024, 1);
    fs::write(f.root.join("pattern.bin"), &big_out).unwrap();
    let big_in = pattern(2 * 1024 * 1024, 7);

    let mut v = f.spawn_via(&session_line("big"));
    // 표준 출력을 아직 읽지 않는다. 출력 방향이 꽉 막힌 상태에서도 입력은 원격에 닿아야 한다.
    let done = v.feed(big_in.clone());
    done.recv_timeout(Duration::from_secs(20))
        .expect("출력이 막힌 동안 via 표준 입력 쓰기가 막힘 (입력 방향이 출력 방향에 묶임)");

    let copy = f.root.join("stdin_copy");
    let deadline = Instant::now() + Duration::from_secs(20);
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
    let out = v.finish(Duration::from_secs(60));
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &big_out, "8 MiB 출력");
    assert_bytes_eq(&fs::read(&copy).unwrap(), &big_in, "2 MiB 입력");
}

#[test]
fn multi_megabyte_echo_full_duplex() {
    // cat 으로 수 MB 를 동시에 넣고 받는다. 한쪽이 다른 쪽을 기다리면 교착된다.
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    let input = pattern(6 * 1024 * 1024, 3);
    let out = f.run_via(&session_line("cat"), &input);
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_bytes_eq(&out.stdout, &input, "6 MiB 메아리");
}

// ---------------------------------------------------------------------------
// (9) via 여러 개 동시 실행
// ---------------------------------------------------------------------------

#[test]
fn many_concurrent_vias() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    const N: usize = 8;
    let mut vias: Vec<Via> = (0..N)
        .map(|i| {
            let mode = if i % 2 == 0 { "cat" } else { "catexit3" };
            f.spawn_via(&session_line(mode))
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
            let got = v.read_stdout_exact(msg.len(), Duration::from_secs(5));
            assert_eq!(String::from_utf8_lossy(&got), msg);
        }
    }

    // 동시에 큰 데이터를 흘린다
    let payloads: Vec<Vec<u8>> = (0..N).map(|i| pattern(512 * 1024, i as u32 + 100)).collect();
    let dones: Vec<Receiver<()>> = vias
        .iter_mut()
        .zip(&payloads)
        .map(|(v, p)| v.feed(p.clone()))
        .collect();
    let handles: Vec<_> = vias
        .into_iter()
        .map(|v| thread::spawn(move || v.finish(Duration::from_secs(60))))
        .collect();
    for d in dones {
        let _ = d.recv_timeout(Duration::from_secs(60));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let out = h.join().unwrap();
        let want = if i % 2 == 0 { 0 } else { 3 };
        assert_eq!(out.code(), Some(want), "via {i} stderr: {}", out.err());
        assert_bytes_eq(&out.stdout, &payloads[i], &format!("via {i}"));
    }
}

// ---------------------------------------------------------------------------
// (10) via 를 죽이면 원격 자식(프로세스 그룹)이 정리된다
// ---------------------------------------------------------------------------

#[test]
fn killing_via_cleans_up_remote_children() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();
    for sig in ["KILL", "TERM"] {
        let _ = fs::remove_file(f.root.join("child.pid"));
        let _ = fs::remove_file(f.root.join("grandchild.pid"));

        let mut v = f.spawn_via(&session_line("sleep"));
        assert_eq!(v.read_stdout_exact(6, Duration::from_secs(5)), b"ready\n");
        let child = f.read_pid("child.pid");
        let grandchild = f.read_pid("grandchild.pid");
        assert!(process_alive(&child), "[{sig}] 원격 자식이 처음부터 없음");
        assert!(process_alive(&grandchild), "[{sig}] 원격 손자가 처음부터 없음");

        let st = Command::new("kill")
            .args([&format!("-{sig}"), &v.child.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
        v.wait(Duration::from_secs(5));

        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(&child) || process_alive(&grandchild) {
            if Instant::now() > deadline {
                panic!(
                    "[{sig}] via 가 죽은 뒤 5초가 지나도 원격 프로세스가 남음 (자식 {}, 손자 {})",
                    process_alive(&child),
                    process_alive(&grandchild)
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    // 에이전트는 계속 동작한다.
    let out = f.run_via(&session_line("cat"), b"after kill");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert_eq!(out.stdout, b"after kill");
}

// ---------------------------------------------------------------------------
// (11) 소켓과 폴더 권한
// ---------------------------------------------------------------------------

fn mode_of(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn socket_and_dir_permissions_when_agent_creates_dir() {
    let mut f = Fixture::new(Fake::InPath);
    f.hlink_dir = f.root.join("fresh");
    assert!(!f.hlink_dir.exists());
    f.start_agent();
    assert_eq!(mode_of(&f.hlink_dir), 0o700, "데이터 폴더 권한");
    let meta = fs::symlink_metadata(f.sock()).unwrap();
    assert!(meta.file_type().is_socket(), "agent.sock 이 소켓이 아님");
    assert_eq!(mode_of(&f.sock()), 0o600, "소켓 권한");
}

#[test]
fn socket_and_dir_permissions_when_dir_is_too_open() {
    // 이미 있는 폴더가 0755 여도 결과는 0700 / 0600 이어야 한다.
    let mut f = Fixture::new(Fake::InPath);
    fs::set_permissions(&f.hlink_dir, fs::Permissions::from_mode(0o755)).unwrap();
    f.start_agent();
    assert_eq!(mode_of(&f.hlink_dir), 0o700, "데이터 폴더 권한");
    assert_eq!(mode_of(&f.sock()), 0o600, "소켓 권한");
}

// ---------------------------------------------------------------------------
// (12) via 의 표준 출력에는 원격 바이트 외에 아무것도 섞이지 않는다
// ---------------------------------------------------------------------------

#[test]
fn via_stdout_carries_only_remote_bytes() {
    let mut f = Fixture::new(Fake::InPath);
    f.start_agent();

    // 원격이 아무것도 쓰지 않으면 stdout 은 정확히 비어 있다.
    let out = f.run_via(&session_line("cat"), b"");
    assert_eq!(out.code(), Some(0), "stderr: {}", out.err());
    assert!(out.stdout.is_empty(), "빈 원격 출력인데 stdout 에 {:?}", out.stdout);

    let out = f.run_via(&session_line("exit7"), b"");
    assert_eq!(out.code(), Some(7));
    assert!(out.stdout.is_empty(), "exit7 인데 stdout 에 {:?}", out.stdout);

    // 원격 stderr 가 있어도 stdout 은 원격 stdout 그대로다.
    let out = f.run_via(&session_line("stderr"), b"");
    let expected: String = (0..300).map(|i| format!("out {i}\n")).collect();
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);

    // 거부·오류일 때도 stdout 은 비어 있다.
    let out = f.run_via("echo hi", b"");
    assert_eq!(out.code(), Some(255));
    assert!(out.stdout.is_empty(), "거부인데 stdout 에 {:?}", out.stdout);

    // 바이트 하나도 앞뒤에 붙지 않는다.
    let out = f.run_via(DEFAULT_LINE, b"\x00");
    assert_eq!(out.code(), Some(0));
    assert_eq!(out.stdout, b"\x00");
}
