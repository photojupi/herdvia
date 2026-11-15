use crate::{ipc, log};
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Debug, PartialEq, Eq)]
pub enum HcResult {
    Added,
    MissingHc {
        manual_cmd: String,
    },
    /// `timed_out`: hc did not finish, so whether it added the machine is unknown.
    Failed {
        reason: String,
        manual_cmd: String,
        timed_out: bool,
    },
}

/// What Herdr's machine list says about a machine name.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed {
    /// Reached with the command this executable would register.
    Same,
    /// Reached through another herdvia executable, as hc prints that program.
    OtherHerdvia(String),
    /// A machine by that name that does not go through herdvia.
    Foreign,
}

fn sh_safe_unquoted(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/:@%+=,-".contains(&b))
}

fn sh_quote(text: &str) -> String {
    if sh_safe_unquoted(text) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

pub fn sh_via_command(exe: &Path, peer: &str) -> String {
    let exe_str = exe.to_string_lossy();
    format!("{} via {peer}", sh_quote(&exe_str))
}

fn quote_windows_arg(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in text.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                quoted.push('\\');
            }
            quoted.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                quoted.push('\\');
            }
            backslashes = 0;
            quoted.push(ch);
        }
    }
    for _ in 0..(backslashes * 2) {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

pub fn windows_via_command(exe: &Path, peer: &str) -> String {
    let exe_str = exe.to_string_lossy();
    let needs_quotes = exe_str.is_empty()
        || exe_str
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"'));
    if !needs_quotes {
        return format!("{exe_str} via {peer}");
    }
    format!("{} via {peer}", quote_windows_arg(&exe_str))
}

pub fn via_command_for_current_os(exe: &Path, peer: &str) -> String {
    if cfg!(windows) {
        windows_via_command(exe, peer)
    } else {
        sh_via_command(exe, peer)
    }
}

pub fn windows_manual_hc_add_line(exe: &Path, peer: &str) -> String {
    let via = windows_via_command(exe, peer);
    if !via.contains('"') {
        format!(
            "hc machine add --via '{}' --name {peer}",
            via.replace('\'', "''")
        )
    } else {
        let cmd_token = quote_windows_arg(&via);
        let inner = &cmd_token[1..cmd_token.len() - 1];
        format!(
            "& {{ $PSNativeCommandArgumentPassing = 'Legacy'; hc machine add --via '{}' --name {peer} }}",
            inner.replace('\'', "''")
        )
    }
}

pub fn sh_manual_hc_add_line(exe: &Path, peer: &str) -> String {
    let via = sh_via_command(exe, peer);
    format!(
        "hc machine add --via '{}' --name {peer}",
        via.replace('\'', r"'\''")
    )
}

pub fn manual_hc_add_line(exe: &Path, peer: &str) -> String {
    if cfg!(windows) {
        windows_manual_hc_add_line(exe, peer)
    } else {
        sh_manual_hc_add_line(exe, peer)
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn hc_on_path(path_var: &OsStr, bin_name: &str) -> Option<PathBuf> {
    for dir in env::split_paths(path_var) {
        // Empty and relative entries name the current directory; hc found there is not run.
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(bin_name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn find_hc() -> Option<PathBuf> {
    let bin_name = if cfg!(windows) { "hc.exe" } else { "hc" };
    if let Some(path) = env::var_os("PATH").and_then(|p| hc_on_path(&p, bin_name)) {
        return Some(path);
    }
    let home = ipc::home_dir().ok()?;
    let fallback = if cfg!(windows) {
        home.join(r"AppData\Local\Programs\herdr-core\hc.exe")
    } else {
        home.join(".local/bin/hc")
    };
    is_executable_file(&fallback).then_some(fallback)
}

/// How a run of `hc` ended.
enum HcRun {
    Exited(std::process::Output),
    /// It could not be started or waited for; the sanitized reason.
    Failed(String),
    /// It was still running after 15 seconds and was killed.
    TimedOut,
}

/// Runs `hc` with `args` for up to 15 seconds, then kills it (on Unix, its whole process group).
async fn run_hc(hc_path: &Path, args: &[&str]) -> HcRun {
    let mut cmd = tokio::process::Command::new(hc_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return HcRun::Failed(log::sanitize(&err.to_string())),
    };

    #[cfg(unix)]
    let child_pid = child.id();

    match tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await {
        Ok(Ok(output)) => HcRun::Exited(output),
        Ok(Err(err)) => HcRun::Failed(log::sanitize(&err.to_string())),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                unsafe {
                    let _ = libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            HcRun::TimedOut
        }
    }
}

pub async fn register_machine(exe: &Path, peer: &str) -> HcResult {
    let manual_cmd = manual_hc_add_line(exe, peer);
    let Some(hc_path) = find_hc() else {
        return HcResult::MissingHc { manual_cmd };
    };
    let via_cmd = via_command_for_current_os(exe, peer);
    let failed = |reason, timed_out| HcResult::Failed {
        reason,
        manual_cmd: manual_cmd.clone(),
        timed_out,
    };
    match run_hc(
        &hc_path,
        &["machine", "add", "--via", &via_cmd, "--name", peer],
    )
    .await
    {
        HcRun::Exited(output) if output.status.success() => HcResult::Added,
        HcRun::Exited(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = stderr
                .lines()
                .chain(stdout.lines())
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(log::sanitize)
                .unwrap_or_else(|| output.status.to_string());
            failed(detail, false)
        }
        HcRun::Failed(reason) => failed(reason, false),
        HcRun::TimedOut => failed("hc machine add timed out after 15 seconds".into(), true),
    }
}

/// What `hc machine ls --json` says about `peer`. None when hc is missing, fails, prints
/// something this cannot read, or does not list `peer`: the caller then just adds it.
pub async fn listed_machine(exe: &Path, peer: &str) -> Option<Listed> {
    let hc_path = find_hc()?;
    let HcRun::Exited(output) = run_hc(&hc_path, &["machine", "ls", "--json"]).await else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    let machines = machines(std::str::from_utf8(&output.stdout).ok()?)?;
    classify(&machines, peer, &via_command_for_current_os(exe, peer))
}

/// One entry of `hc machine ls --json`, with the fields `register` looks at.
#[derive(Debug, PartialEq, Eq)]
struct Machine {
    name: String,
    target: Option<String>,
    via: Option<bool>,
}

/// `expected` is the `--via` value this executable would register for `peer`. An entry whose
/// fields cannot tell gives None, never a guess.
fn classify(machines: &[Machine], peer: &str, expected: &str) -> Option<Listed> {
    let machine = machines.iter().find(|m| m.name == peer)?;
    if !machine.via? {
        return Some(Listed::Foreign);
    }
    let target = machine.target.as_deref()?;
    if target == expected {
        return Some(Listed::Same);
    }
    // The program part, as hc prints it: quoted when the path needs it.
    let program = target.strip_suffix(&format!(" via {peer}"));
    let file = program.and_then(|p| p.trim_matches(['\'', '"']).rsplit(['/', '\\']).next());
    match (program, file) {
        (Some(program), Some("herdvia" | "herdvia.exe")) => {
            Some(Listed::OtherHerdvia(log::sanitize(program)))
        }
        _ => Some(Listed::Foreign),
    }
}

/// The `machines` of `hc machine ls --json`. None for anything else, including a truncated or
/// otherwise invalid document; entries without a string `name` are left out.
fn machines(text: &str) -> Option<Vec<Machine>> {
    let mut reader = JsonReader {
        text,
        pos: 0,
        depth: 0,
    };
    let doc = reader.value()?;
    reader.skip_space();
    if reader.pos != text.len() {
        return None;
    }
    let Json::Object(fields) = doc else {
        return None;
    };
    let Some((_, Json::Array(entries))) = fields.into_iter().find(|(k, _)| k == "machines") else {
        return None;
    };
    let mut machines = Vec::new();
    for entry in entries {
        let Json::Object(fields) = entry else {
            return None;
        };
        let field = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let Some(Json::String(name)) = field("name") else {
            continue;
        };
        machines.push(Machine {
            name: name.clone(),
            target: match field("target") {
                Some(Json::String(target)) => Some(target.clone()),
                _ => None,
            },
            via: match field("via") {
                Some(Json::Bool(via)) => Some(*via),
                _ => None,
            },
        });
    }
    Some(machines)
}

/// A JSON value, as far as `machines` needs it.
enum Json {
    Other,
    Bool(bool),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// A small JSON reader for hc's machine list (no JSON crate in the tree).
struct JsonReader<'a> {
    text: &'a str,
    pos: usize,
    depth: usize,
}

impl JsonReader<'_> {
    /// Deeper documents are refused rather than read with unbounded recursion.
    const MAX_DEPTH: usize = 32;

    fn skip_space(&mut self) {
        let rest = &self.text[self.pos..];
        self.pos += rest.len() - rest.trim_start_matches([' ', '\t', '\n', '\r']).len();
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.text[self.pos..].starts_with(token);
        if found {
            self.pos += token.len();
        }
        found
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_space();
        if self.eat("true") {
            return Some(Json::Bool(true));
        }
        if self.eat("false") {
            return Some(Json::Bool(false));
        }
        if self.eat("null") {
            return Some(Json::Other);
        }
        match self.text[self.pos..].bytes().next()? {
            b'"' => self.string().map(Json::String),
            b'[' | b'{' => {
                self.depth += 1;
                if self.depth > Self::MAX_DEPTH {
                    return None;
                }
                let value = if self.eat("[") {
                    self.array()
                } else {
                    self.eat("{");
                    self.object()
                };
                self.depth -= 1;
                value
            }
            b'-' | b'0'..=b'9' => {
                let rest = &self.text[self.pos..];
                let len = rest.len()
                    - rest
                        .trim_start_matches(|c: char| c.is_ascii_digit() || "+-.eE".contains(c))
                        .len();
                self.pos += len;
                Some(Json::Other)
            }
            _ => None,
        }
    }

    fn array(&mut self) -> Option<Json> {
        let mut items = Vec::new();
        self.skip_space();
        if self.eat("]") {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_space();
            if self.eat("]") {
                return Some(Json::Array(items));
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    fn object(&mut self) -> Option<Json> {
        let mut fields = Vec::new();
        self.skip_space();
        if self.eat("}") {
            return Some(Json::Object(fields));
        }
        loop {
            self.skip_space();
            if !self.text[self.pos..].starts_with('"') {
                return None;
            }
            let key = self.string()?;
            self.skip_space();
            if !self.eat(":") {
                return None;
            }
            fields.push((key, self.value()?));
            self.skip_space();
            if self.eat("}") {
                return Some(Json::Object(fields));
            }
            if !self.eat(",") {
                return None;
            }
        }
    }

    /// A string starting at its opening quote.
    fn string(&mut self) -> Option<String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let rest = &self.text[self.pos..];
            let run = rest.find(['"', '\\']).unwrap_or(rest.len());
            if rest[..run].chars().any(|c| c < ' ') {
                return None;
            }
            out.push_str(&rest[..run]);
            self.pos += run;
            if self.eat("\"") {
                return Some(out);
            }
            if !self.eat("\\") {
                return None;
            }
            let escaped = match self.text[self.pos..].bytes().next()? {
                b'u' => {
                    self.pos += 1;
                    let high = self.hex4()?;
                    let code = if (0xD800..0xDC00).contains(&high) {
                        if !self.eat("\\u") {
                            return None;
                        }
                        let low = self.hex4()?;
                        if !(0xDC00..0xE000).contains(&low) {
                            return None;
                        }
                        0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                    } else {
                        high
                    };
                    out.push(char::from_u32(code)?);
                    continue;
                }
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                _ => return None,
            };
            self.pos += 1;
            out.push(escaped);
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.text.get(self.pos..self.pos + 4)?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        self.pos += 4;
        u32::from_str_radix(digits, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn machine(name: &str, target: Option<&str>, via: Option<bool>) -> Machine {
        Machine {
            name: name.into(),
            target: target.map(str::to_owned),
            via,
        }
    }

    #[test]
    fn machines_reads_the_shape_hc_prints() {
        // As hc 0.1.21 prints it, with fields register does not use.
        let text = r#"{"machines":[{"host":"mini","id":"9079","keeps_running_after_logout":null,"name":"macmini","session":null,"state":"connected","target":"/Users/jupi/.local/bin/herdvia via macmini","temporary":false,"version":"0.1.21","via":true},{"host":"jupid","name":"win1","since_ms":1794751955566,"state":"unreachable","target":"/Users/jupi/.local/bin/herdvia via win1","via":true,"why":"cannot reach: herdvia: peer did not connect within 5 seconds"},{"name":"vps","target":"can@10.0.0.7:2222","via":false}]}"#;
        assert_eq!(
            machines(text).unwrap(),
            [
                machine(
                    "macmini",
                    Some("/Users/jupi/.local/bin/herdvia via macmini"),
                    Some(true)
                ),
                machine(
                    "win1",
                    Some("/Users/jupi/.local/bin/herdvia via win1"),
                    Some(true)
                ),
                machine("vps", Some("can@10.0.0.7:2222"), Some(false)),
            ]
        );
        assert_eq!(machines(" {\"machines\" : [ ] }\n").unwrap(), []);
    }

    #[test]
    fn machines_decodes_escapes_and_skips_nested_values() {
        let text = r#"{"x":{"machines":[{"name":"decoy"}]},"machines":[{"name":"h\u00e9 \ud83d\ude00","target":"\"C:\\Program Files\\herdvia\\herdvia.exe\" via h\/1\n","nested":{"a":[1,-2.5e3,{"b":[]}],"via":false},"via":true}]}"#;
        assert_eq!(
            machines(text).unwrap(),
            [machine(
                "h\u{e9} \u{1F600}",
                Some("\"C:\\Program Files\\herdvia\\herdvia.exe\" via h/1\n"),
                Some(true)
            )]
        );
    }

    #[test]
    fn machines_leaves_missing_fields_empty_and_skips_entries_without_a_name() {
        let text = r#"{"machines":[{"target":"/bin/herdvia via a","via":true},{"name":7},{"name":"a"},{"name":"b","target":null,"via":"yes"}]}"#;
        assert_eq!(
            machines(text).unwrap(),
            [machine("a", None, None), machine("b", None, None)]
        );
    }

    #[test]
    fn machines_refuses_anything_else() {
        let full = r#"{"machines":[{"name":"a","target":"/bin/herdvia via a","via":true}]}"#;
        for cut in 0..full.len() {
            assert_eq!(machines(&full[..cut]), None, "{:?}", &full[..cut]);
        }
        for text in [
            "",
            "not json",
            "[]",
            r#"{"machines":{}}"#,
            r#"{"other":[]}"#,
            r#"{"machines":[1]}"#,
            r#"{"machines":[]} trailing"#,
            r#"{"machines":[],}"#,
            r#"{"machines":["#,
            r#"{"machines":[{"name":"a\x"}]}"#,
            r#"{"machines":[{"name":"\ud800"}]}"#,
            "{\"machines\":[{\"name\":\"raw\nnewline\"}]}",
        ] {
            assert_eq!(machines(text), None, "{text:?}");
        }
        let deep = format!(
            "{{\"machines\":[],\"x\":{}{}}}",
            "[".repeat(100),
            "]".repeat(100)
        );
        assert_eq!(machines(&deep), None);
    }

    #[test]
    fn a_name_inside_a_string_is_not_a_machine() {
        let text = r#"{"machines":[{"name":"x","target":"{\"name\":\"home\",\"target\":\"/bin/herdvia via home\",\"via\":true}","via":false}]}"#;
        let list = machines(text).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(classify(&list, "home", "/bin/herdvia via home"), None);
    }

    #[test]
    fn classify_tells_this_executable_from_another_herdvia_and_from_other_machines() {
        let expected = "/Users/me/.local/bin/herdvia via home";
        let check = |target: Option<&str>, via: Option<bool>| {
            classify(&[machine("home", target, via)], "home", expected)
        };
        assert_eq!(check(Some(expected), Some(true)), Some(Listed::Same));
        for (target, old) in [
            ("/Users/me/old/herdvia via home", "/Users/me/old/herdvia"),
            ("'/tmp/my bin/herdvia' via home", "'/tmp/my bin/herdvia'"),
            (
                r#""C:\Program Files\herdvia\herdvia.exe" via home"#,
                r#""C:\Program Files\herdvia\herdvia.exe""#,
            ),
            (r"C:\h\herdvia.exe via home", r"C:\h\herdvia.exe"),
        ] {
            assert_eq!(
                check(Some(target), Some(true)),
                Some(Listed::OtherHerdvia(old.into())),
                "{target}"
            );
        }
        for (target, via) in [
            (Some(expected), Some(false)),
            (Some("can@vps"), Some(false)),
            (Some("ssh -p 2222 me@vps"), Some(true)),
            (Some("/bin/herdvia via other"), Some(true)),
            (Some("/bin/not-herdvia via home"), Some(true)),
            (Some("/bin/herdvia2 via home"), Some(true)),
        ] {
            assert_eq!(check(target, via), Some(Listed::Foreign), "{target:?}");
        }
        // Fields it cannot read give no answer rather than a guess.
        assert_eq!(check(Some(expected), None), None);
        assert_eq!(check(None, Some(true)), None);
        // Not listed, or listed under another name only.
        assert_eq!(classify(&[], "home", expected), None);
        assert_eq!(
            classify(
                &[machine("Home", Some(expected), Some(true))],
                "home",
                expected
            ),
            None
        );
    }

    #[test]
    fn classify_sanitizes_the_program_it_shows() {
        let target = "/tmp/\u{1b}[31m/herdvia via home";
        assert_eq!(
            classify(
                &[machine("home", Some(target), Some(true))],
                "home",
                "/x via home"
            ),
            Some(Listed::OtherHerdvia("/tmp/\\u{1b}[31m/herdvia".into()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn hc_is_not_found_through_a_relative_path_entry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let hc = dir.path().join("hc");
        fs::write(&hc, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&hc, fs::Permissions::from_mode(0o755)).unwrap();
        // The same directory, named relative to the current directory.
        let depth = env::current_dir().unwrap().components().count() - 1;
        let relative =
            PathBuf::from("../".repeat(depth)).join(dir.path().strip_prefix("/").unwrap());
        assert!(is_executable_file(&relative.join("hc")));

        assert_eq!(hc_on_path(dir.path().as_os_str(), "hc"), Some(hc));
        assert_eq!(hc_on_path(relative.as_os_str(), "hc"), None);
    }

    #[test]
    fn sh_via_command_quotes_spaces_and_single_quotes_for_posix_sh() {
        assert_eq!(
            sh_via_command(&PathBuf::from("/Users/you/.local/bin/herdvia"), "home"),
            "/Users/you/.local/bin/herdvia via home"
        );
        let tricky = PathBuf::from("/tmp/it's home/.local/bin/herdvia");
        let cmd = sh_via_command(&tricky, "home");
        assert_eq!(cmd, r"'/tmp/it'\''s home/.local/bin/herdvia' via home");

        #[cfg(unix)]
        {
            let script = format!("set -- {cmd}; printf '%s\\n%s\\n%s' \"$1\" \"$2\" \"$3\"");
            let out = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                "/tmp/it's home/.local/bin/herdvia\nvia\nhome"
            );
        }
    }

    #[test]
    fn windows_via_command_quotes_spaces_and_backslashes_for_command_line_to_argv_w() {
        assert_eq!(
            windows_via_command(Path::new(r"C:\Prog\herdvia.exe"), "home"),
            r"C:\Prog\herdvia.exe via home"
        );
        assert_eq!(
            windows_via_command(
                Path::new(r"C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe"),
                "home"
            ),
            r#""C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe" via home"#
        );
        assert_eq!(
            windows_via_command(Path::new(r#"C:\dir with space\"#), "home"),
            r#""C:\dir with space\\" via home"#
        );
        assert_eq!(
            windows_via_command(Path::new(r#"C:\weird\"quote\herdvia.exe"#), "home"),
            r#""C:\weird\\\"quote\herdvia.exe" via home"#
        );
    }

    // Win32 CommandLineToArgvW parser (post-argv[0] arguments) for cross-platform verification.
    fn parse_command_line_to_argv_w(cmd_line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut chars = cmd_line.chars().peekable();
        // Skip executable token.
        while chars.peek().is_some_and(|c| !c.is_ascii_whitespace()) {
            chars.next();
        }
        loop {
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }
            let mut cur = String::new();
            let mut in_quotes = false;
            while let Some(&ch) = chars.peek() {
                if !in_quotes && ch.is_ascii_whitespace() {
                    break;
                }
                if ch == '\\' {
                    let mut bs = 0usize;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        bs += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        cur.extend(std::iter::repeat_n('\\', bs / 2));
                        if bs.is_multiple_of(2) {
                            in_quotes = !in_quotes;
                        } else {
                            cur.push('"');
                        }
                    } else {
                        cur.extend(std::iter::repeat_n('\\', bs));
                    }
                } else if ch == '"' {
                    chars.next();
                    if in_quotes && chars.peek() == Some(&'"') {
                        chars.next();
                        cur.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                } else {
                    chars.next();
                    cur.push(ch);
                }
            }
            args.push(cur);
        }
        args
    }

    // Simulates PowerShell 5.1 (default Legacy) or PowerShell 7.3+ (default Standard, honoring
    // `& { $PSNativeCommandArgumentPassing = 'Legacy'; ... }`) building lpCommandLine for `hc.exe`.
    fn simulate_powershell_to_cmd_line(line: &str, ps7_default_standard: bool) -> String {
        let (body, legacy) = if let Some(inner) = line
            .strip_prefix("& { $PSNativeCommandArgumentPassing = 'Legacy'; ")
            .and_then(|s| s.strip_suffix(" }"))
        {
            (inner, true)
        } else {
            (line, !ps7_default_standard)
        };

        // Tokenize PowerShell argument mode tokens (barewords and single-quoted strings).
        let mut tokens = Vec::new();
        let mut chars = body.chars().peekable();
        while chars.peek().is_some() {
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }
            let mut tok = String::new();
            if chars.peek() == Some(&'\'') {
                chars.next();
                loop {
                    let ch = chars.next().expect("unterminated single quote");
                    if ch == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            tok.push('\'');
                        } else {
                            break;
                        }
                    } else {
                        tok.push(ch);
                    }
                }
            } else {
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_whitespace() {
                        break;
                    }
                    chars.next();
                    tok.push(ch);
                }
            }
            tokens.push(tok);
        }

        let mut cmd = tokens[0].clone();
        for arg in &tokens[1..] {
            cmd.push(' ');
            if legacy {
                // PowerShell Legacy NativeCommandParameterBinder: wraps in "..." if unquoted
                // whitespace exists without escaping inner quotes.
                let mut quote_count = 0usize;
                let mut space_outside = false;
                let bytes = arg.as_bytes();
                for i in 0..bytes.len() {
                    if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                        quote_count += 1;
                    } else if bytes[i].is_ascii_whitespace() && quote_count.is_multiple_of(2) {
                        space_outside = true;
                    }
                }
                if space_outside && quote_count.is_multiple_of(2) {
                    cmd.push('"');
                    cmd.push_str(arg);
                    cmd.push('"');
                } else {
                    cmd.push_str(arg);
                }
            } else {
                // PowerShell 7.3+ Standard passing uses CommandLineToArgvW quoting.
                if arg.chars().any(|c| c.is_whitespace() || c == '"') {
                    let via_like = windows_via_command(Path::new(arg), "x");
                    let _ = via_like;
                    // Standard CommandLineToArgvW escaping:
                    cmd.push('"');
                    let mut bs = 0usize;
                    for ch in arg.chars() {
                        if ch == '\\' {
                            bs += 1;
                        } else if ch == '"' {
                            cmd.extend(std::iter::repeat_n('\\', bs * 2 + 1));
                            cmd.push('"');
                            bs = 0;
                        } else {
                            cmd.extend(std::iter::repeat_n('\\', bs));
                            bs = 0;
                            cmd.push(ch);
                        }
                    }
                    cmd.extend(std::iter::repeat_n('\\', bs * 2));
                    cmd.push('"');
                } else {
                    cmd.push_str(arg);
                }
            }
        }
        cmd
    }

    #[test]
    fn windows_manual_hc_add_line_roundtrips_powershell_5_1_and_7_to_command_line_to_argv_w() {
        let cases = [
            r"C:\Users\jupi\AppData\Local\Programs\herdvia\herdvia.exe",
            r"C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe",
            r"C:\Users\O'Connor\Programs\herdvia\herdvia.exe",
            r"C:\Users\Jane O'Doe\Programs\herdvia\herdvia.exe",
            r"C:\dir with space\",
            r#"C:\weird\"quote\herdvia.exe"#,
            r"C:\100%PATH%dir with space\herdvia.exe",
        ];
        for raw_exe in cases {
            let exe = Path::new(raw_exe);
            let expected_via = windows_via_command(exe, "home");
            let ps_line = windows_manual_hc_add_line(exe, "home");

            for ps7 in [false, true] {
                let raw_cmd = simulate_powershell_to_cmd_line(&ps_line, ps7);
                let argv = parse_command_line_to_argv_w(&raw_cmd);
                assert_eq!(
                    argv,
                    vec![
                        "machine".to_owned(),
                        "add".to_owned(),
                        "--via".to_owned(),
                        expected_via.clone(),
                        "--name".to_owned(),
                        "home".to_owned(),
                    ],
                    "failed for exe={raw_exe:?} ps7={ps7} line={ps_line}"
                );
            }
        }
    }

    #[test]
    fn manual_hc_add_line_is_pasteable() {
        #[cfg(unix)]
        {
            let line = manual_hc_add_line(Path::new("/tmp/my bin/herdvia"), "home");
            let script = format!(
                "hc() {{ printf '%s\\n%s\\n%s\\n%s\\n%s\\n%s' \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$6\"; }}; {line}"
            );
            let out = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                "machine\nadd\n--via\n'/tmp/my bin/herdvia' via home\n--name\nhome"
            );
        }
        #[cfg(windows)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let shim_src = tmp.path().join("hc.rs");
            fs::write(
                &shim_src,
                "fn main() { for a in std::env::args().skip(1) { println!(\"{a}\"); } }\n",
            )
            .unwrap();
            let shim_exe = tmp.path().join("hc.exe");
            let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let status = std::process::Command::new(rustc)
                .arg(&shim_src)
                .arg("-o")
                .arg(&shim_exe)
                .status()
                .unwrap();
            assert!(status.success());

            let path_env = {
                let mut paths = vec![tmp.path().to_path_buf()];
                if let Some(existing) = env::var_os("PATH") {
                    paths.extend(env::split_paths(&existing));
                }
                env::join_paths(paths).unwrap()
            };

            for raw_exe in [
                r"C:\Users\jupi\AppData\Local\Programs\herdvia\herdvia.exe",
                r"C:\Users\Jane O'Doe\AppData\Local\Programs\herdvia\herdvia.exe",
                r"C:\dir with space\",
            ] {
                let exe = Path::new(raw_exe);
                let expected_via = windows_via_command(exe, "home");
                let line = manual_hc_add_line(exe, "home");

                for shell in ["powershell.exe", "pwsh.exe"] {
                    let Ok(out) = std::process::Command::new(shell)
                        .args(["-NoProfile", "-NonInteractive", "-Command", &line])
                        .env("PATH", &path_env)
                        .output()
                    else {
                        if shell == "pwsh.exe" {
                            continue;
                        }
                        panic!("powershell.exe must be available on Windows");
                    };
                    assert!(out.status.success(), "{shell} failed on {line}");
                    let got: Vec<_> = String::from_utf8(out.stdout)
                        .unwrap()
                        .lines()
                        .map(|s| s.trim_end_matches('\r').to_owned())
                        .collect();
                    assert_eq!(
                        got,
                        vec![
                            "machine".to_owned(),
                            "add".to_owned(),
                            "--via".to_owned(),
                            expected_via.clone(),
                            "--name".to_owned(),
                            "home".to_owned(),
                        ],
                        "{shell} argv mismatch for {raw_exe}"
                    );
                }
            }
        }
    }
}
