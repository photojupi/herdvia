//! `herdvia doctor`: a read-only check of the installation and the agent.
//!
//! It creates and changes nothing: not the data directory, `secret.key`, `peers`,
//! the automatic start registration, or the agent's state. The only programs it runs
//! are `herdr-core --version`. The checks gather facts; the pure functions below
//! turn them into `ok`, `warn`, or `fail` lines.

use crate::{
    control, ipc,
    peers::{self, Peer, Permission},
};
use iroh::{EndpointId, SecretKey, Watcher};
use std::{
    env,
    fs::{self, Metadata},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// The herdr-core (hc) version whose connect line herdvia was verified with.
pub const VERIFIED_CORE_VERSION: &str = "0.1.21";

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, PartialEq, Eq)]
struct Check {
    level: Level,
    text: String,
}

fn check(level: Level, text: impl Into<String>) -> Check {
    Check {
        level,
        text: text.into(),
    }
}

fn line(check: &Check) -> String {
    let level = match check.level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "fail",
    };
    format!("{level:<4} {}", check.text)
}

fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.level == Level::Fail))
}

// ---------------------------------------------------------------------------
// The agent's side: the reply to the `doctor` control request.
// ---------------------------------------------------------------------------

/// The agent's answer to the `doctor` control request.
pub(crate) fn agent_report(endpoint: &iroh::Endpoint) -> String {
    let exe = env::current_exe().ok().map(|p| p.display().to_string());
    let relay = endpoint
        .home_relay_status()
        .get()
        .into_iter()
        .find(|status| status.is_connected())
        .map(|status| status.url().to_string());
    format_agent_info(VERSION, exe.as_deref(), relay.as_deref())
}

/// `key value` lines. `exe` is left out when the agent cannot find its own path.
fn format_agent_info(version: &str, exe: Option<&str>, relay: Option<&str>) -> String {
    let mut text = format!("version {version}\n");
    if let Some(exe) = exe {
        text.push_str(&format!("exe {exe}\n"));
    }
    text.push_str(&format!("relay {}\n", relay.unwrap_or("none")));
    text
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AgentInfo {
    version: Option<String>,
    exe: Option<String>,
    /// "none" when the agent has no home relay connection; None when it did not say.
    relay: Option<String>,
}

fn parse_agent_info(text: &str) -> AgentInfo {
    let mut info = AgentInfo::default();
    for line in text.lines() {
        // The value is the rest of the line: a path may contain spaces. Unknown keys are skipped.
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = Some(value.to_owned());
        match key {
            "version" => info.version = value,
            "exe" => info.exe = value,
            "relay" => info.relay = value,
            _ => {}
        }
    }
    info
}

// ---------------------------------------------------------------------------
// Judgements. No file system, network, or environment access below this line,
// except in `run` and the helpers after it.
// ---------------------------------------------------------------------------

fn executable_check(exe: Result<&Path, &str>, unstable: bool) -> Check {
    match exe {
        Ok(exe) if unstable => check(
            Level::Warn,
            format!(
                "executable: herdvia {VERSION} at {}, a temporary or build (target) directory; herdvia agent install records this path, so put herdvia somewhere lasting (the install script uses {INSTALL_DIR}) and run it from there",
                exe.display()
            ),
        ),
        Ok(exe) => check(
            Level::Ok,
            format!("executable: herdvia {VERSION} at {}", exe.display()),
        ),
        Err(err) => check(
            Level::Warn,
            format!(
                "executable: herdvia {VERSION}, but its path is unknown ({err}); herdvia agent install needs it, so run herdvia by its full path"
            ),
        ),
    }
}

#[derive(Debug)]
enum DirState {
    Missing,
    Directory,
    NotDirectory,
    ForeignOwner,
    Unreadable(String),
    /// This user's directory, but this user cannot list it or reach the files in it.
    Inaccessible(String),
    /// The agent refuses it (`inspect_dir`): an ACL entry that allows access (macOS), or
    /// another owner, a reparse point, or a DACL that lets other accounts in and that this
    /// user cannot change (Windows). The message says how to fix it.
    Refused(String),
    /// Its DACL lets other accounts in, and herdvia restricts it the next time it runs
    /// (Windows). The note says so.
    Loose(String),
}

/// The agent socket path on Unix, its length in bytes, and whether it fits in a socket address.
struct Socket {
    path: String,
    len: usize,
    fits: bool,
}

fn data_dir_check(dir: &Path, from_env: bool, state: &DirState, socket: Option<&Socket>) -> Check {
    let mut text = format!("data directory: {}", dir.display());
    if from_env {
        text.push_str(" (from HERDVIA_DIR; automatic start always uses the default directory)");
    }
    if let Some(socket) = socket.filter(|s| !s.fits) {
        return check(
            Level::Fail,
            format!(
                "{text}; the agent socket path {} is {} bytes, too long for a Unix socket; {}",
                socket.path,
                socket.len,
                if from_env {
                    "set a shorter HERDVIA_DIR"
                } else {
                    "use a shorter home directory path"
                }
            ),
        );
    }
    match state {
        DirState::Directory => check(Level::Ok, text),
        DirState::Missing => check(
            Level::Warn,
            format!(
                "{text}; it does not exist yet, so this PC has no identity; herdvia id creates it"
            ),
        ),
        DirState::NotDirectory => check(
            Level::Fail,
            format!(
                "{text}; it is not a directory{NOT_DIRECTORY_NOTE}; move it aside so that herdvia can create the directory"
            ),
        ),
        DirState::ForeignOwner => check(
            Level::Fail,
            format!("{text}; it is owned by another user; give it back to this user (chown)"),
        ),
        DirState::Unreadable(err) => check(
            Level::Fail,
            format!("{text}; it cannot be read: {err}; check its owner and permissions"),
        ),
        DirState::Inaccessible(err) => check(
            Level::Fail,
            format!("{text}; this user cannot open it ({err}); {DIR_FIX}"),
        ),
        DirState::Refused(err) => check(Level::Fail, format!("{text}; {err}")),
        DirState::Loose(note) => check(Level::Warn, format!("{text}; {note}")),
    }
}

#[derive(Debug)]
enum KeyState {
    Missing,
    Present(EndpointId),
    /// Present, and herdvia restricts its DACL the next time it runs (Windows).
    Loose(EndpointId, String),
    /// The agent's refusal, with advice.
    BadPermissions(String),
    Unreadable(String),
}

fn key_check(state: &KeyState) -> Check {
    match state {
        KeyState::Missing => check(
            Level::Warn,
            "secret.key: missing, so this PC has no identity yet; herdvia id creates it",
        ),
        KeyState::Present(id) => check(
            Level::Ok,
            format!(
                "secret.key: present, fingerprint {}",
                peers::fingerprint(*id)
            ),
        ),
        KeyState::Loose(id, note) => check(
            Level::Warn,
            format!(
                "secret.key: present, fingerprint {}; {note}",
                peers::fingerprint(*id)
            ),
        ),
        KeyState::BadPermissions(err) => check(Level::Fail, format!("secret.key: {err}")),
        KeyState::Unreadable(err) => check(
            Level::Fail,
            format!(
                "secret.key: {err}; restore it from a backup (removing it makes herdvia id create a new identity that every peer must allow again)"
            ),
        ),
    }
}

fn peers_check(path: &Path, peers: Result<&[Peer], &io::Error>) -> Check {
    match peers {
        Ok([]) => check(Level::Ok, "peers: none registered"),
        Ok(peers) => {
            let count = |p: Permission| peers.iter().filter(|peer| peer.permission == p).count();
            check(
                Level::Ok,
                format!(
                    "peers: {} registered (in {}, out {}, both {})",
                    peers.len(),
                    count(Permission::In),
                    count(Permission::Out),
                    count(Permission::Both)
                ),
            )
        }
        // The loader's message already names the file.
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            check(Level::Fail, format!("peers: {}", refusal(err)))
        }
        // A malformed line.
        Err(err) if err.kind() == io::ErrorKind::InvalidData => check(
            Level::Fail,
            format!(
                "peers: {err} ({}); fix that line, or remove it and register the peer again with herdvia allow",
                path.display()
            ),
        ),
        Err(err) => check(
            Level::Fail,
            format!(
                "peers: {err} ({}); it must be a regular file that this user can read",
                path.display()
            ),
        ),
    }
}

#[derive(Debug)]
enum CoreFound {
    /// Where the agent finds it.
    Agent(PathBuf),
    /// Only on this shell's PATH, which the agent started at login does not use (macOS).
    #[cfg_attr(windows, allow(dead_code))]
    ShellOnly(PathBuf),
    Missing,
}

/// The version in `herdr-core --version` output such as `hc 0.1.21`.
fn core_version(output: &str) -> Option<&str> {
    let version = output.lines().next()?.split_whitespace().last()?;
    let version = version.strip_prefix('v').unwrap_or(version);
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then_some(version)
}

fn herdr_install_command(windows: bool) -> &'static str {
    if windows {
        r#"powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex""#
    } else {
        "curl -fsSL https://releases.herdr.dev/core/install.sh | sh"
    }
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `searched`: where the agent looks. `link`: where to link a herdr-core found only on PATH.
fn core_check(
    found: &CoreFound,
    version: Option<Result<&str, &str>>,
    windows: bool,
    searched: &str,
    link: &Path,
    log: &str,
) -> Check {
    let path = match found {
        CoreFound::Missing => {
            return check(
                Level::Fail,
                format!(
                    "herdr-core: not found ({searched}); herdvia does not install it. Install it with Herdr's install script: {}",
                    herdr_install_command(windows)
                ),
            );
        }
        CoreFound::Agent(path) | CoreFound::ShellOnly(path) => path,
    };
    let mut level = Level::Ok;
    let mut text = format!("herdr-core: {}", path.display());
    match version {
        Some(Ok(v)) if v == VERIFIED_CORE_VERSION => text.push_str(&format!(" (hc {v})")),
        Some(Ok(v)) => {
            level = Level::Warn;
            text.push_str(&format!(
                " (hc {v}); herdvia was verified with the connect line of hc {VERIFIED_CORE_VERSION}. If that line changed, the remote agent rejects it: look for \"rejected command\" in the agent log ({log})"
            ));
        }
        Some(Err(err)) => {
            level = Level::Warn;
            text.push_str(&format!(
                "; its --version failed ({err}); reinstall it with Herdr's install script: {}",
                herdr_install_command(windows)
            ));
        }
        None => {}
    }
    if let CoreFound::ShellOnly(path) = found {
        level = Level::Warn;
        text.push_str(&format!(
            "; it is only on this shell's PATH, and the agent started at login searches {searched}, so it will not find it. Link it: mkdir -p {} && ln -s {} {}",
            sh_quote(&link.parent().unwrap_or(link).to_string_lossy()),
            sh_quote(&path.to_string_lossy()),
            sh_quote(&link.to_string_lossy())
        ));
    }
    check(level, text)
}

#[derive(Debug)]
enum Autostart {
    /// Linux and other Unix systems.
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
    Missing,
    /// `target`: the registered executable (macOS) or Run command (Windows).
    Registered {
        target: String,
        same: bool,
    },
    Unreadable(String),
}

fn autostart_check(state: &Autostart) -> Check {
    match state {
        Autostart::Unsupported => check(
            Level::Warn,
            "automatic start: not supported on this OS; start herdvia agent another way, for example from a systemd user service",
        ),
        Autostart::Missing => check(
            Level::Warn,
            "automatic start: not registered; run herdvia agent install",
        ),
        Autostart::Registered { same: true, .. } => {
            check(Level::Ok, "automatic start: registered for this executable")
        }
        Autostart::Registered {
            target,
            same: false,
        } => check(
            Level::Warn,
            format!(
                "automatic start: registered for {target}, not this executable; run herdvia agent install again with this one"
            ),
        ),
        Autostart::Unreadable(err) => check(
            Level::Warn,
            format!(
                "automatic start: the registration cannot be read ({err}); run herdvia agent install again"
            ),
        ),
    }
}

#[cfg(any(windows, test))]
fn elevated_check() -> Check {
    check(
        Level::Warn,
        "automatic start: this process runs as administrator, and herdvia agent install refuses that; run it from a normal PowerShell window",
    )
}

#[derive(Debug)]
enum AgentReply {
    /// Nothing listens in this data directory.
    Absent,
    /// The connection or both requests failed.
    Failed(String),
    /// Accepts the connection but does not finish answering within `AGENT_TIMEOUT`.
    Silent,
    /// Answers `status` but not `doctor`: an agent older than this executable.
    Old,
    Info(AgentInfo),
}

/// `start`: how to start or restart the agent here.
fn agent_check(reply: &AgentReply, this_exe: Option<&Path>, same_exe: bool, start: &str) -> Check {
    match reply {
        AgentReply::Absent => check(
            Level::Fail,
            format!("agent: not running for this data directory; {start}"),
        ),
        AgentReply::Failed(err) => check(
            Level::Fail,
            format!("agent: does not answer correctly ({err}); restart it: {start}"),
        ),
        AgentReply::Silent => check(
            Level::Fail,
            format!(
                "agent: it may be running, but it did not answer within {} seconds; restart it: {start}",
                AGENT_TIMEOUT.as_secs()
            ),
        ),
        AgentReply::Old => check(
            Level::Warn,
            format!(
                "agent: running, but older than this executable (it does not know the doctor request); restart it: {start}"
            ),
        ),
        AgentReply::Info(info) => {
            let version = info.version.as_deref().unwrap_or("(unknown version)");
            let exe = info.exe.as_deref().unwrap_or("(unknown path)");
            if version == VERSION && same_exe {
                check(
                    Level::Ok,
                    format!("agent: running herdvia {version} from {exe}"),
                )
            } else {
                check(
                    Level::Warn,
                    format!(
                        "agent: running herdvia {version} from {exe}, not this executable (herdvia {VERSION} at {}); if herdvia was upgraded, restart the agent: {start}",
                        this_exe.map_or("(unknown path)".into(), |p| p.display().to_string())
                    ),
                )
            }
        }
    }
}

fn relay_check(info: &AgentInfo) -> Option<Check> {
    Some(match info.relay.as_deref()? {
        "none" => check(
            Level::Warn,
            "relay: the agent has no home relay connection (an agent that just started may need a few seconds); check that this PC can make outgoing HTTPS connections and whether it needs a proxy: herdvia does not use proxy environment variables",
        ),
        url => check(Level::Ok, format!("relay: connected to {url}")),
    })
}

/// One line per peer of a `status` reply.
fn peer_checks(status: &str, macos: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    for row in status.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let fields: Vec<_> = row.split_whitespace().collect();
        let [name, permission, state, path, rtt] = fields[..] else {
            checks.push(check(
                Level::Warn,
                format!("peer status: unrecognized line {row:?}; compare herdvia status"),
            ));
            continue;
        };
        let peer = format!("peer {name} ({permission})");
        checks.push(match (state, path) {
            ("connected", "direct") => check(Level::Ok, format!("{peer}: connected directly, {rtt}")),
            ("connected", "relay") => {
                let mut text = format!("{peer}: connected through a relay, {rtt}");
                if macos {
                    text.push_str("; if it stays that way and the peer is on the same local network as this Mac, allow herdvia under System Settings > Privacy & Security > Local Network");
                }
                check(Level::Ok, text)
            }
            ("connected", _) => check(Level::Ok, format!("{peer}: connected, path unknown")),
            _ if permission == "in" => check(
                Level::Warn,
                format!(
                    "{peer}: not connected; it connects only when that PC calls this one, so on that PC allow this one with --outbound and run its agent"
                ),
            ),
            _ => check(
                Level::Warn,
                format!(
                    "{peer}: not connected; its agent is off, or it has not allowed this PC (on that PC: herdvia pending, then herdvia accept)"
                ),
            ),
        });
    }
    if checks.is_empty() {
        checks.push(check(
            Level::Warn,
            "peer connections: no peers are registered; register one with herdvia allow, or with herdvia accept for a PC that herdvia pending lists",
        ));
    }
    checks
}

fn pending_check(pending: &str) -> Check {
    match pending
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .count()
    {
        0 => check(Level::Ok, "pending: no rejected connection attempts"),
        n => check(
            Level::Ok,
            format!(
                "pending: {n} rejected connection attempt{} from unregistered PCs; review them with herdvia pending and accept one with herdvia accept",
                if n == 1 { "" } else { "s" }
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Gathering facts. Reads only.
// ---------------------------------------------------------------------------

struct Report(Vec<Check>);

impl Report {
    fn add(&mut self, check: Check) {
        println!("{}", line(&check));
        self.0.push(check);
    }
}

pub async fn run() -> io::Result<i32> {
    let mut report = Report(Vec::new());
    let exe = env::current_exe().map_err(|e| e.to_string());
    report.add(executable_check(
        exe.as_deref().map_err(String::as_str),
        exe.as_deref().is_ok_and(unstable_path),
    ));
    let exe = exe.ok();
    let from_env = env::var_os("HERDVIA_DIR").is_some();
    let dir = match ipc::data_dir() {
        Ok(dir) => {
            let socket = socket(&dir);
            let state = dir_state(&dir);
            let dir_check = data_dir_check(&dir, from_env, &state, socket.as_ref());
            let usable = dir_check.level != Level::Fail;
            report.add(dir_check);
            // A data directory that the agent refuses explains the key, peers, and agent
            // checks too, so they are left out rather than given misleading advice.
            if matches!(
                state,
                DirState::Missing | DirState::Directory | DirState::Loose(_)
            ) {
                report.add(key_check(&key_state(&dir.join("secret.key"))));
                report.add(peers_state_check(&dir));
            }
            usable.then_some(dir)
        }
        Err(err) => {
            report.add(check(
                Level::Fail,
                if from_env {
                    format!("data directory: {err}; set it to a full path, or unset it")
                } else {
                    format!("data directory: {err}; set it, or set HERDVIA_DIR")
                },
            ));
            None
        }
    };

    let home = ipc::home_dir();
    match &home {
        Ok(home) => {
            let (found, searched, link) = find_core(home);
            let version = match &found {
                CoreFound::Agent(path) | CoreFound::ShellOnly(path) => {
                    Some(core_version_of(path).await)
                }
                CoreFound::Missing => None,
            };
            report.add(core_check(
                &found,
                version
                    .as_ref()
                    .map(|v| v.as_deref().map_err(String::as_str)),
                cfg!(windows),
                &searched,
                &link,
                AGENT_LOG,
            ));
        }
        Err(err) => report.add(check(
            Level::Fail,
            format!("herdr-core: cannot look for it: {err}; set HOME to your home directory"),
        )),
    }
    report.add(autostart_check(&autostart(&home, exe.as_deref())));
    #[cfg(windows)]
    {
        if crate::launchd::elevated().unwrap_or(false) {
            report.add(elevated_check());
        }
    }

    if let Some(dir) = dir {
        let start = if from_env || !(cfg!(target_os = "macos") || cfg!(windows)) {
            "run herdvia agent"
        } else {
            "run herdvia agent install (or herdvia agent in a terminal)"
        };
        let mut status = None;
        let reply = match ask(&dir, "doctor").await {
            None => AgentReply::Silent,
            Some(Ok(text)) => AgentReply::Info(parse_agent_info(&text)),
            Some(Err(err)) if absent(&err) => AgentReply::Absent,
            // An older agent drops the connection on an unknown request.
            Some(Err(err)) => match ask(&dir, "status").await {
                None => AgentReply::Silent,
                Some(Ok(text)) => {
                    status = Some(text);
                    AgentReply::Old
                }
                Some(Err(_)) => AgentReply::Failed(err.to_string()),
            },
        };
        let same_exe = match (&reply, &exe) {
            (AgentReply::Info(info), Some(exe)) => {
                info.exe.as_deref().is_some_and(|a| same_path(a, exe))
            }
            _ => false,
        };
        report.add(agent_check(&reply, exe.as_deref(), same_exe, start));
        if let AgentReply::Info(info) = &reply
            && let Some(relay) = relay_check(info)
        {
            report.add(relay);
        }
        if matches!(reply, AgentReply::Info(_) | AgentReply::Old) {
            let no_answer = format!(
                "the agent did not answer within {} seconds; restart it: {start}",
                AGENT_TIMEOUT.as_secs()
            );
            let status = match status {
                Some(status) => Some(Ok(status)),
                None => ask(&dir, "status").await,
            };
            let stuck = status.is_none();
            match status {
                Some(Ok(status)) => {
                    for c in peer_checks(&status, cfg!(target_os = "macos")) {
                        report.add(c);
                    }
                }
                Some(Err(err)) => report.add(check(
                    Level::Warn,
                    format!("peer connections: the agent did not answer status ({err}); compare herdvia status"),
                )),
                None => report.add(check(
                    Level::Warn,
                    format!("peer connections: {no_answer}"),
                )),
            }
            // A stuck agent is not asked again, so the wait does not add up.
            if !stuck {
                match ask(&dir, "pending").await {
                    Some(Ok(pending)) => report.add(pending_check(&pending)),
                    Some(Err(err)) => report.add(check(
                        Level::Warn,
                        format!("pending: the agent did not answer ({err}); restart it: {start}"),
                    )),
                    None => report.add(check(Level::Warn, format!("pending: {no_answer}"))),
                }
            }
        }
    }
    Ok(exit_code(&report.0))
}

/// How long doctor waits for each control request, from connecting to the end of the answer.
const AGENT_TIMEOUT: Duration = Duration::from_secs(3);

/// A control request, or None when the agent does not finish answering within `AGENT_TIMEOUT`.
async fn ask(dir: &Path, command: &str) -> Option<io::Result<String>> {
    tokio::time::timeout(AGENT_TIMEOUT, control::request(dir, command))
        .await
        .ok()
}

fn absent(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// The same file, by path or after resolving links.
fn same_path(a: &str, b: &Path) -> bool {
    Path::new(a) == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

#[cfg(unix)]
fn unstable_path(exe: &Path) -> bool {
    crate::launchd::unstable_path(exe, &env::temp_dir())
}

#[cfg(windows)]
fn unstable_path(exe: &Path) -> bool {
    crate::autostart_logic::unstable_path(
        &exe.to_string_lossy(),
        &env::temp_dir().to_string_lossy(),
    )
}

#[cfg(unix)]
fn socket(dir: &Path) -> Option<Socket> {
    let path = dir.join("agent.sock");
    Some(Socket {
        len: path.as_os_str().len(),
        // Validates the length as bind and connect do, without creating anything.
        fits: std::os::unix::net::SocketAddr::from_pathname(&path).is_ok(),
        path: path.display().to_string(),
    })
}

#[cfg(windows)]
fn socket(_dir: &Path) -> Option<Socket> {
    None
}

fn dir_state(dir: &Path) -> DirState {
    // The agent refuses a symbolic link, and a junction on Windows (ipc::prepare_dir).
    match fs::symlink_metadata(dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => DirState::Missing,
        Err(err) => DirState::Unreadable(err.to_string()),
        Ok(metadata) if !metadata.is_dir() => DirState::NotDirectory,
        Ok(metadata) if !owned(&metadata) => DirState::ForeignOwner,
        // Reads its entries, then looks up a name inside it (which needs search permission).
        Ok(_) => match fs::read_dir(dir).and_then(|_| fs::metadata(dir.join("."))) {
            Ok(_) => match inspect_dir(dir) {
                Ok(None) => DirState::Directory,
                Ok(Some(note)) => DirState::Loose(note),
                Err(err) => DirState::Refused(err.to_string()),
            },
            Err(err) => DirState::Inaccessible(err.to_string()),
        },
    }
}

#[cfg(unix)]
fn owned(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == ipc::uid()
}

/// The owner is checked by ipc::inspect_dir on Windows.
#[cfg(windows)]
fn owned(_metadata: &Metadata) -> bool {
    true
}

/// The peers file, read-only with the agent's own checks. A DACL that the next run restricts
/// (Windows) gives a warning, unless the content is broken, which that run does not fix.
fn peers_state_check(dir: &Path) -> Check {
    let path = dir.join("peers");
    match open_private(&path) {
        Ok((file, Some(note))) => match peers::read(file) {
            Ok(_) => check(Level::Warn, format!("peers: {note}")),
            Err(err) => peers_check(&path, Err(&err)),
        },
        _ => peers_check(&path, peers::load(dir).as_deref()),
    }
}

/// Opens the key read-only with the agent's own checks. Never creates it.
fn key_state(path: &Path) -> KeyState {
    let (mut file, note) = match open_private(path) {
        Ok(opened) => opened,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return KeyState::Missing,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            return KeyState::BadPermissions(refusal(&err));
        }
        Err(err) => return KeyState::Unreadable(format!("{}: {err}", path.display())),
    };
    let mut bytes = Vec::new();
    if let Err(err) = file.read_to_end(&mut bytes) {
        return KeyState::Unreadable(format!("{}: {err}", path.display()));
    }
    match <[u8; 32]>::try_from(bytes.as_slice()) {
        Ok(bytes) => {
            let id = SecretKey::from_bytes(&bytes).public();
            match note {
                None => KeyState::Present(id),
                Some(note) => KeyState::Loose(id, note),
            }
        }
        Err(_) => KeyState::Unreadable("secret.key must contain exactly 32 bytes".into()),
    }
}

/// The agent's checks of the data directory, without changing it. On Windows, a note when
/// its DACL lets other accounts in and herdvia restricts it the next time it runs.
#[cfg(unix)]
fn inspect_dir(dir: &Path) -> io::Result<Option<String>> {
    ipc::check_dir(dir).map(|()| None)
}

#[cfg(windows)]
fn inspect_dir(dir: &Path) -> io::Result<Option<String>> {
    ipc::inspect_dir(dir)
}

/// The agent's checks of the key or peers, without changing it, with that note (Windows).
#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<(fs::File, Option<String>)> {
    ipc::private_file(path).map(|file| (file, None))
}

#[cfg(windows)]
fn open_private(path: &Path) -> io::Result<(fs::File, Option<String>)> {
    ipc::inspect_file(path)
}

/// A refusal of the key or peers, with `PRIVATE_FILE_FIX` unless it says what to do itself.
fn refusal(err: &io::Error) -> String {
    if names_its_fix(err) {
        err.to_string()
    } else {
        format!("{err}; {PRIVATE_FILE_FIX}")
    }
}

/// The ACL refusal (macOS) says how to remove the ACL; the owner and mode refusal does not.
#[cfg(unix)]
fn names_its_fix(err: &io::Error) -> bool {
    err.to_string().contains("chmod -N")
}

/// herdvia's own refusals (the kind of entry, the owner, a DACL with the icacls command) say
/// what is required; an error from the system does not.
#[cfg(windows)]
fn names_its_fix(err: &io::Error) -> bool {
    err.raw_os_error().is_none()
}

#[cfg(unix)]
const PRIVATE_FILE_FIX: &str = "make this user its owner and run chmod 600 on it";
#[cfg(windows)]
const PRIVATE_FILE_FIX: &str =
    "check that it is a regular file, not a link, that this user can read";

#[cfg(unix)]
const DIR_FIX: &str = "run chmod 700 on it";
#[cfg(windows)]
const DIR_FIX: &str = "give this user access to it";

#[cfg(unix)]
const NOT_DIRECTORY_NOTE: &str = " (a symbolic link is not accepted)";
#[cfg(windows)]
const NOT_DIRECTORY_NOTE: &str = " (a junction or symbolic link is not accepted)";

#[cfg(windows)]
const INSTALL_DIR: &str = r"%LOCALAPPDATA%\Programs\herdvia";
#[cfg(unix)]
const INSTALL_DIR: &str = "~/.local/bin";

#[cfg(target_os = "macos")]
const AGENT_LOG: &str = "~/Library/Logs/herdvia.log";
#[cfg(windows)]
const AGENT_LOG: &str = r"%LOCALAPPDATA%\herdvia\herdvia.log";
#[cfg(not(any(target_os = "macos", windows)))]
const AGENT_LOG: &str = "the agent's standard error";

/// Where herdr-core is, as the agent looks for it, with what was searched and where to link it.
#[cfg(unix)]
fn find_core(home: &Path) -> (CoreFound, String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let executable = |dirs: &[PathBuf]| {
        dirs.iter().map(|d| d.join("herdr-core")).find(|p| {
            fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    };
    let local = home.join(".local/bin");
    let link = local.join("herdr-core");
    // Relative entries are made absolute against the current directory, so that the path
    // shown and the link target suggested below do not depend on where they are used.
    // An empty entry means the current directory, as it does to the shell.
    let shell: Vec<PathBuf> = env::var_os("PATH")
        .map(|p| {
            env::split_paths(&p)
                .filter_map(|d| {
                    if d.as_os_str().is_empty() {
                        env::current_dir().ok()
                    } else {
                        std::path::absolute(d).ok()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        // The PATH that the LaunchAgent gives the agent; the connect line also tries ~/.local/bin.
        let mut agent = vec![local.clone()];
        agent.extend(env::split_paths(crate::launchd::SYSTEM_PATH));
        let searched = format!("{}:{}", local.display(), crate::launchd::SYSTEM_PATH);
        if let Some(path) = executable(&agent) {
            (CoreFound::Agent(path), searched, link)
        } else if let Some(path) = executable(&shell) {
            (CoreFound::ShellOnly(path), searched, link)
        } else {
            (
                CoreFound::Missing,
                format!("{searched} or this shell's PATH"),
                link,
            )
        }
    } else {
        let mut dirs = shell;
        dirs.push(local.clone());
        let found = executable(&dirs).map_or(CoreFound::Missing, CoreFound::Agent);
        (found, format!("PATH and {}", local.display()), link)
    }
}

#[cfg(windows)]
fn find_core(home: &Path) -> (CoreFound, String, PathBuf) {
    // The path the Windows agent runs (exec/windows.rs).
    let path = home.join(r"AppData\Local\Programs\herdr-core\herdr-core.exe");
    let found = if fs::metadata(&path).is_ok() {
        CoreFound::Agent(path.clone())
    } else {
        CoreFound::Missing
    };
    (found, path.display().to_string(), path)
}

/// How much of the `--version` output is read.
const CORE_OUTPUT_LIMIT: u64 = 4096;

/// The version that `herdr-core --version` prints, within 2 seconds.
async fn core_version_of(path: &Path) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Its own process group, so that a timeout also ends what it started (as exec does).
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        let mut head = Vec::new();
        (&mut stdout)
            .take(CORE_OUTPUT_LIMIT)
            .read_to_end(&mut head)
            .await?;
        // Closing the pipe ends a program that keeps printing.
        drop(stdout);
        Ok::<_, io::Error>((child.wait().await?, head))
    })
    .await;
    let (status, head) = match finished {
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // SAFETY: the child leads its own process group with this pid and is not reaped yet.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            return Err("it did not finish within 2 seconds".into());
        }
        Ok(Err(err)) => return Err(err.to_string()),
        Ok(Ok(finished)) => finished,
    };
    if !status.success() {
        return Err(status.to_string());
    }
    let text = String::from_utf8_lossy(&head);
    core_version(&text).map(str::to_owned).ok_or_else(|| {
        let first: String = text.lines().next().unwrap_or("").chars().take(80).collect();
        format!("unrecognized output {:?}", first.trim())
    })
}

/// Returns a non-blocking warning message when `herdr-core` is missing or misconfigured on a target PC.
pub(crate) async fn core_preflight_warning() -> Option<String> {
    let home = ipc::home_dir().ok()?;
    let (found, searched, link) = find_core(&home);
    let version = match &found {
        CoreFound::Agent(path) | CoreFound::ShellOnly(path) => Some(core_version_of(path).await),
        CoreFound::Missing => None,
    };
    let check = core_check(
        &found,
        version
            .as_ref()
            .map(|r| r.as_deref().map_err(String::as_str)),
        cfg!(windows),
        &searched,
        &link,
        AGENT_LOG,
    );
    (check.level != Level::Ok).then_some(check.text)
}

#[cfg(unix)]
fn autostart(home: &io::Result<PathBuf>, exe: Option<&Path>) -> Autostart {
    if !cfg!(target_os = "macos") {
        return Autostart::Unsupported;
    }
    let home = match home {
        Ok(home) => home,
        Err(err) => return Autostart::Unreadable(err.to_string()),
    };
    match crate::launchd::registered_executable(home) {
        Ok(None) => Autostart::Missing,
        Ok(Some(target)) => Autostart::Registered {
            same: exe.is_some_and(|exe| same_path(&target, exe)),
            target,
        },
        Err(err) => Autostart::Unreadable(err.to_string()),
    }
}

#[cfg(windows)]
fn autostart(_home: &io::Result<PathBuf>, exe: Option<&Path>) -> Autostart {
    use std::os::windows::ffi::OsStrExt;
    match crate::launchd::registered_command() {
        Ok(None) => Autostart::Missing,
        Ok(Some(value)) => Autostart::Registered {
            same: exe.is_some_and(|exe| {
                crate::autostart_logic::registered_for(
                    &value,
                    &exe.as_os_str().encode_wide().collect::<Vec<_>>(),
                )
            }),
            target: String::from_utf16_lossy(&value)
                .trim_end_matches('\0')
                .to_owned(),
        },
        Err(err) => Autostart::Unreadable(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(c: &Check) -> Level {
        c.level
    }

    #[test]
    fn lines_start_with_the_level_and_any_fail_exits_with_1() {
        let checks = [
            check(Level::Ok, "a"),
            check(Level::Warn, "b"),
            check(Level::Fail, "c"),
        ];
        let lines: Vec<_> = checks.iter().map(line).collect();
        assert_eq!(lines, ["ok   a", "warn b", "fail c"]);
        assert_eq!(exit_code(&checks), 1);
        assert_eq!(exit_code(&checks[..2]), 0);
        assert_eq!(exit_code(&[]), 0);
    }

    #[test]
    fn agent_info_round_trips_and_ignores_unknown_keys() {
        let text = format_agent_info(
            "0.1.2",
            Some("/opt/my apps/herdvia"),
            Some("https://r.example/"),
        );
        assert_eq!(
            text,
            "version 0.1.2\nexe /opt/my apps/herdvia\nrelay https://r.example/\n"
        );
        assert_eq!(
            parse_agent_info(&format!("{text}future value\nbare\n")),
            AgentInfo {
                version: Some("0.1.2".into()),
                exe: Some("/opt/my apps/herdvia".into()),
                relay: Some("https://r.example/".into()),
            }
        );
        let text = format_agent_info("0.1.2", None, None);
        assert_eq!(text, "version 0.1.2\nrelay none\n");
        assert_eq!(parse_agent_info(&text).exe, None);
        assert_eq!(parse_agent_info(&text).relay.as_deref(), Some("none"));
    }

    #[test]
    fn executable_warns_for_unstable_or_unknown_paths() {
        let ok = executable_check(Ok(Path::new("/opt/herdvia")), false);
        assert_eq!(ok.level, Level::Ok);
        assert_eq!(
            ok.text,
            format!("executable: herdvia {VERSION} at /opt/herdvia")
        );
        let unstable = executable_check(Ok(Path::new("/tmp/herdvia")), true);
        assert_eq!(unstable.level, Level::Warn);
        assert!(
            unstable
                .text
                .starts_with(&format!("executable: herdvia {VERSION} at /tmp/herdvia,"))
        );
        assert!(unstable.text.contains("herdvia agent install"));
        assert_eq!(level(&executable_check(Err("gone"), false)), Level::Warn);
    }

    #[test]
    fn data_dir_reports_herdvia_dir_missing_directory_and_long_socket() {
        let dir = Path::new("/d");
        let ok = data_dir_check(dir, false, &DirState::Directory, None);
        assert_eq!(
            (ok.level, ok.text.as_str()),
            (Level::Ok, "data directory: /d")
        );
        let env = data_dir_check(dir, true, &DirState::Directory, None);
        assert_eq!(env.level, Level::Ok);
        assert!(env.text.contains("HERDVIA_DIR"), "{}", env.text);
        let missing = data_dir_check(dir, false, &DirState::Missing, None);
        assert_eq!(missing.level, Level::Warn);
        assert!(missing.text.contains("herdvia id"), "{}", missing.text);
        for state in [
            DirState::NotDirectory,
            DirState::ForeignOwner,
            DirState::Unreadable("denied".into()),
            DirState::Inaccessible("denied".into()),
            DirState::Refused("remove the ACL".into()),
        ] {
            assert_eq!(
                level(&data_dir_check(dir, false, &state, None)),
                Level::Fail,
                "{state:?}"
            );
        }
        assert_eq!(
            data_dir_check(
                dir,
                false,
                &DirState::Refused("remove the ACL".into()),
                None
            )
            .text,
            "data directory: /d; remove the ACL"
        );
        let loose = data_dir_check(
            dir,
            false,
            &DirState::Loose("restricted later".into()),
            None,
        );
        assert_eq!(
            (loose.level, loose.text.as_str()),
            (Level::Warn, "data directory: /d; restricted later")
        );
        let long = Socket {
            path: "/d/agent.sock".into(),
            len: 200,
            fits: false,
        };
        let fail = data_dir_check(dir, true, &DirState::Missing, Some(&long));
        assert_eq!(fail.level, Level::Fail);
        assert!(
            fail.text.contains("200 bytes") && fail.text.contains("shorter HERDVIA_DIR"),
            "{}",
            fail.text
        );
        let short = Socket { fits: true, ..long };
        assert_eq!(
            level(&data_dir_check(
                dir,
                false,
                &DirState::Directory,
                Some(&short)
            )),
            Level::Ok
        );
    }

    #[test]
    fn key_shows_only_the_fingerprint() {
        let key = SecretKey::generate();
        let present = key_check(&KeyState::Present(key.public()));
        assert_eq!(present.level, Level::Ok);
        assert_eq!(
            present.text,
            format!(
                "secret.key: present, fingerprint {}",
                peers::fingerprint(key.public())
            )
        );
        assert!(!present.text.contains(&key.public().to_string()));
        assert_eq!(level(&key_check(&KeyState::Missing)), Level::Warn);
        let wide = key_check(&KeyState::BadPermissions("k must be 0600".into()));
        assert_eq!(
            (wide.level, wide.text.as_str()),
            (Level::Fail, "secret.key: k must be 0600")
        );
        let loose = key_check(&KeyState::Loose(key.public(), "restricted later".into()));
        assert_eq!(loose.level, Level::Warn);
        assert_eq!(
            loose.text,
            format!(
                "secret.key: present, fingerprint {}; restricted later",
                peers::fingerprint(key.public())
            )
        );
        assert_eq!(
            level(&key_check(&KeyState::Unreadable("short".into()))),
            Level::Fail
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_state_reads_without_creating_and_applies_the_agents_checks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        assert!(matches!(key_state(&path), KeyState::Missing));
        assert!(!path.exists());
        assert!(matches!(
            key_state(&dir.path().join("none/secret.key")),
            KeyState::Missing
        ));
        assert!(!dir.path().join("none").exists());
        let key = peers::load_key(dir.path()).unwrap();
        assert!(matches!(key_state(&path), KeyState::Present(id) if id == key.public()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(key_state(&path), KeyState::BadPermissions(e) if e.ends_with(PRIVATE_FILE_FIX))
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, [1; 31]).unwrap();
        assert!(matches!(key_state(&path), KeyState::Unreadable(_)));
    }

    #[cfg(windows)]
    #[test]
    fn checks_the_content_of_peers_that_the_next_run_restricts() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        ipc::prepare_dir(&dir).unwrap();
        let path = dir.join("peers");
        fs::write(&path, "broken\n").unwrap();
        // An explicit grant to Users, which the next run removes.
        let status = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-5-32-545:R"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let note = ipc::inspect_file(&path).unwrap().1.unwrap();
        // Restricting it does not fix the content.
        let broken = peers_state_check(&dir);
        assert_eq!(broken.level, Level::Fail);
        assert!(
            broken
                .text
                .starts_with("peers: invalid or duplicate peer name in peers ("),
            "{}",
            broken.text
        );
        let id = SecretKey::generate().public();
        fs::write(&path, format!("work {id} both\n")).unwrap();
        assert_eq!(ipc::inspect_file(&path).unwrap().1.as_deref(), Some(&*note));
        let valid = peers_state_check(&dir);
        assert_eq!(valid.level, Level::Warn);
        assert_eq!(valid.text, format!("peers: {note}"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn allow_acls_on_the_directory_or_its_files_fail_without_changes() {
        let chmod = ipc::test_chmod;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        let key = peers::load_key(&dir).unwrap();
        let peer = Peer {
            name: "work".into(),
            id: key.public(),
            permission: Permission::Both,
        };
        peers::allow(&dir, peer).unwrap();

        let path = dir.join("secret.key");
        chmod(&["+a", "everyone allow read"], &path);
        assert!(
            matches!(key_state(&path), KeyState::BadPermissions(e) if e.contains("chmod -N") && !e.contains(PRIVATE_FILE_FIX))
        );
        chmod(&["-N"], &path);
        assert!(matches!(key_state(&path), KeyState::Present(id) if id == key.public()));
        let path = dir.join("peers");
        chmod(&["+a", "everyone allow write"], &path);
        let err = peers::load(&dir).unwrap_err();
        let refused = peers_check(&path, Err(&err));
        assert_eq!(refused.level, Level::Fail);
        assert_eq!(refused.text, format!("peers: {err}"));

        assert!(matches!(dir_state(&dir), DirState::Directory));
        chmod(&["+a", "everyone deny delete"], &dir);
        assert!(matches!(dir_state(&dir), DirState::Directory));
        chmod(&["+a", "everyone allow list,search"], &dir);
        let state = dir_state(&dir);
        assert!(
            matches!(&state, DirState::Refused(e) if e.contains("chmod -N")),
            "{state:?}"
        );
        assert_eq!(
            level(&data_dir_check(&dir, false, &state, None)),
            Level::Fail
        );
        // Read-only: the ACL is still there.
        assert!(matches!(dir_state(&dir), DirState::Refused(_)));
    }

    #[cfg(windows)]
    #[test]
    fn an_open_dacl_warns_until_the_next_run_restricts_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("herdvia");
        fs::create_dir(&dir).unwrap();
        // As the parent of %APPDATA%\herdvia passes on a grant to another group.
        let status = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)R"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let key = SecretKey::generate();
        let path = dir.join("secret.key");
        fs::write(&path, key.to_bytes()).unwrap();
        let state = dir_state(&dir);
        let warn = data_dir_check(&dir, false, &state, None);
        assert_eq!(warn.level, Level::Warn, "{}", warn.text);
        assert!(
            warn.text.contains("restricts it the next time it runs"),
            "{}",
            warn.text
        );
        let state = key_state(&path);
        assert!(
            matches!(&state, KeyState::Loose(id, _) if *id == key.public()),
            "{state:?}"
        );
        assert_eq!(key_check(&state).level, Level::Warn);
        // Read-only: the next look finds the same.
        assert!(matches!(dir_state(&dir), DirState::Loose(_)));
        peers::load_key(&dir).unwrap();
        assert!(matches!(dir_state(&dir), DirState::Directory));
        assert!(matches!(key_state(&path), KeyState::Present(id) if id == key.public()));
    }

    #[test]
    fn peers_counts_directions_and_fails_with_the_loader_error() {
        let peer = |name: &str, permission| Peer {
            name: name.into(),
            id: SecretKey::generate().public(),
            permission,
        };
        let path = Path::new("/d/peers");
        let none = peers_check(path, Ok(&[]));
        assert_eq!(
            (none.level, none.text.as_str()),
            (Level::Ok, "peers: none registered")
        );
        let list = [
            peer("a", Permission::In),
            peer("b", Permission::Out),
            peer("c", Permission::Both),
            peer("d", Permission::Both),
        ];
        let some = peers_check(path, Ok(&list));
        assert_eq!(some.level, Level::Ok);
        assert_eq!(some.text, "peers: 4 registered (in 1, out 1, both 2)");
        let bad = peers_check(
            path,
            Err(&crate::frame::invalid("invalid EndpointId in peers")),
        );
        assert_eq!(bad.level, Level::Fail);
        assert!(
            bad.text
                .starts_with("peers: invalid EndpointId in peers (/d/peers)"),
            "{}",
            bad.text
        );
        let wide = peers_check(
            path,
            Err(&io::Error::new(
                io::ErrorKind::PermissionDenied,
                "/d/peers must be owned by this user with permissions no wider than 0600",
            )),
        );
        assert_eq!(wide.level, Level::Fail);
        #[cfg(unix)]
        assert_eq!(
            wide.text,
            format!(
                "peers: /d/peers must be owned by this user with permissions no wider than 0600; {PRIVATE_FILE_FIX}"
            )
        );
        // Not a malformed line, so no advice to fix one.
        let other = peers_check(path, Err(&io::Error::other("too many links")));
        assert_eq!(other.level, Level::Fail);
        assert_eq!(
            other.text,
            "peers: too many links (/d/peers); it must be a regular file that this user can read"
        );
    }

    #[test]
    fn private_file_advice_only_where_the_refusal_gives_none() {
        let system = io::Error::from_raw_os_error(5);
        assert_eq!(refusal(&system), format!("{system}; {PRIVATE_FILE_FIX}"));
        #[cfg(unix)]
        let own = "/d/k has an access control list (ACL) entry that allows access; remove the ACL with chmod -N '/d/k'";
        #[cfg(windows)]
        let own = r"C:\d\k must be owned by this user, Administrators, or SYSTEM, not S-1-5-21-2";
        let own = io::Error::new(io::ErrorKind::PermissionDenied, own);
        assert_eq!(refusal(&own), own.to_string());
    }

    #[test]
    fn core_version_takes_the_last_word_of_the_first_line() {
        assert_eq!(core_version("hc 0.1.21\n"), Some("0.1.21"));
        assert_eq!(core_version("hc v0.2.0\nbuilt somewhere\n"), Some("0.2.0"));
        assert_eq!(core_version("0.1.21"), Some("0.1.21"));
        for output in ["", "\n", "hc unknown", "usage: hc"] {
            assert_eq!(core_version(output), None, "{output:?}");
        }
    }

    #[test]
    fn core_checks_location_version_and_install_hint() {
        let link = Path::new("/h/.local/bin/herdr-core");
        let core = |found: &CoreFound, version, windows| {
            core_check(
                found,
                version,
                windows,
                "/h/.local/bin:/usr/bin",
                link,
                "LOG",
            )
        };
        let agent = CoreFound::Agent("/h/.local/bin/herdr-core".into());
        let ok = core(&agent, Some(Ok(VERIFIED_CORE_VERSION)), false);
        assert_eq!(ok.level, Level::Ok);
        assert_eq!(
            ok.text,
            format!("herdr-core: /h/.local/bin/herdr-core (hc {VERIFIED_CORE_VERSION})")
        );
        let newer = core(&agent, Some(Ok("0.1.22")), false);
        assert_eq!(newer.level, Level::Warn);
        assert!(
            newer.text.contains("hc 0.1.22")
                && newer.text.contains("rejected command")
                && newer.text.contains("LOG"),
            "{}",
            newer.text
        );
        let broken = core(&agent, Some(Err("exit status: 99")), false);
        assert_eq!(broken.level, Level::Warn);
        assert!(
            broken
                .text
                .contains("its --version failed (exit status: 99)"),
            "{}",
            broken.text
        );

        let shell = CoreFound::ShellOnly("/opt/it's/herdr-core".into());
        let only = core(&shell, Some(Ok(VERIFIED_CORE_VERSION)), false);
        assert_eq!(only.level, Level::Warn);
        assert!(
            only.text
                .ends_with(r"mkdir -p '/h/.local/bin' && ln -s '/opt/it'\''s/herdr-core' '/h/.local/bin/herdr-core'"),
            "{}",
            only.text
        );

        let missing = core(&CoreFound::Missing, None, false);
        assert_eq!(missing.level, Level::Fail);
        assert!(
            missing
                .text
                .starts_with("herdr-core: not found (/h/.local/bin:/usr/bin)"),
            "{}",
            missing.text
        );
        assert!(
            missing
                .text
                .contains("https://releases.herdr.dev/core/install.sh")
        );
        assert!(
            core(&CoreFound::Missing, None, true)
                .text
                .contains("https://releases.herdr.dev/core/install.ps1")
        );
    }

    #[test]
    fn autostart_warns_unless_registered_for_this_executable() {
        assert_eq!(
            level(&autostart_check(&Autostart::Registered {
                target: "/a".into(),
                same: true
            })),
            Level::Ok
        );
        let other = autostart_check(&Autostart::Registered {
            target: "/old/herdvia".into(),
            same: false,
        });
        assert_eq!(other.level, Level::Warn);
        assert!(
            other.text.contains("/old/herdvia") && other.text.contains("herdvia agent install"),
            "{}",
            other.text
        );
        for state in [
            Autostart::Missing,
            Autostart::Unsupported,
            Autostart::Unreadable("bad".into()),
        ] {
            assert_eq!(level(&autostart_check(&state)), Level::Warn, "{state:?}");
        }
        assert!(
            autostart_check(&Autostart::Missing)
                .text
                .contains("herdvia agent install")
        );
        assert_eq!(elevated_check().level, Level::Warn);
    }

    #[test]
    fn agent_fails_when_absent_and_warns_when_old_or_different() {
        let exe = Some(Path::new("/opt/herdvia"));
        let start = "run herdvia agent install";
        let absent = agent_check(&AgentReply::Absent, exe, false, start);
        assert_eq!(absent.level, Level::Fail);
        assert!(absent.text.ends_with(start), "{}", absent.text);
        assert_eq!(
            level(&agent_check(
                &AgentReply::Failed("eof".into()),
                exe,
                false,
                start
            )),
            Level::Fail
        );
        let silent = agent_check(&AgentReply::Silent, exe, false, start);
        assert_eq!(silent.level, Level::Fail);
        assert_eq!(
            silent.text,
            "agent: it may be running, but it did not answer within 3 seconds; restart it: run herdvia agent install"
        );
        let old = agent_check(&AgentReply::Old, exe, false, start);
        assert_eq!(old.level, Level::Warn);
        assert!(old.text.contains("older"), "{}", old.text);
        let info = |version: &str| AgentInfo {
            version: Some(version.into()),
            exe: Some("/opt/herdvia".into()),
            relay: Some("none".into()),
        };
        let same = agent_check(&AgentReply::Info(info(VERSION)), exe, true, start);
        assert_eq!(same.level, Level::Ok);
        assert_eq!(
            same.text,
            format!("agent: running herdvia {VERSION} from /opt/herdvia")
        );
        assert_eq!(
            level(&agent_check(
                &AgentReply::Info(info("0.0.1")),
                exe,
                true,
                start
            )),
            Level::Warn
        );
        assert_eq!(
            level(&agent_check(
                &AgentReply::Info(info(VERSION)),
                exe,
                false,
                start
            )),
            Level::Warn
        );
    }

    #[test]
    fn relay_none_warns_about_https_and_proxies() {
        let mut info = AgentInfo::default();
        assert_eq!(relay_check(&info), None);
        info.relay = Some("none".into());
        let none = relay_check(&info).unwrap();
        assert_eq!(none.level, Level::Warn);
        assert!(
            none.text.contains("HTTPS") && none.text.contains("proxy"),
            "{}",
            none.text
        );
        info.relay = Some("https://r.example/".into());
        assert_eq!(
            relay_check(&info).unwrap(),
            check(Level::Ok, "relay: connected to https://r.example/")
        );
    }

    #[test]
    fn peers_status_hints_depend_on_path_and_direction() {
        let header = "name permission connection path rtt\n";
        assert_eq!(peer_checks(header, false).len(), 1);
        assert_eq!(peer_checks(header, false)[0].level, Level::Warn);
        let status = format!(
            "{header}a out connected direct 4.12ms\nb both connected relay 238.82ms\nc out disconnected - -\nd in disconnected - -\ne both disconnected - -\nf in connected unknown -\nbroken line\n"
        );
        let checks = peer_checks(&status, true);
        let levels: Vec<_> = checks.iter().map(level).collect();
        assert_eq!(
            levels,
            [
                Level::Ok,
                Level::Ok,
                Level::Warn,
                Level::Warn,
                Level::Warn,
                Level::Ok,
                Level::Warn
            ]
        );
        assert_eq!(checks[0].text, "peer a (out): connected directly, 4.12ms");
        assert!(
            checks[1].text.contains("Local Network"),
            "{}",
            checks[1].text
        );
        assert!(
            !peer_checks(&status, false)[1]
                .text
                .contains("Local Network")
        );
        for i in [2, 4] {
            assert!(
                checks[i].text.contains("herdvia accept"),
                "{}",
                checks[i].text
            );
        }
        assert!(checks[3].text.contains("--outbound"), "{}", checks[3].text);
    }

    #[test]
    fn pending_counts_entries_after_the_header() {
        let header = "fingerprint id attempts last-seen\n";
        assert_eq!(
            pending_check(header),
            check(Level::Ok, "pending: no rejected connection attempts")
        );
        let two = pending_check(&format!("{header}x a 1 0s\ny b 3 9s\n"));
        assert_eq!(two.level, Level::Ok);
        assert!(
            two.text
                .starts_with("pending: 2 rejected connection attempts")
                && two.text.contains("herdvia accept"),
            "{}",
            two.text
        );
    }
}
