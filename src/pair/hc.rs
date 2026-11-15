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
    MissingHc { manual_cmd: String },
    Failed { reason: String, manual_cmd: String },
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

pub async fn register_machine(exe: &Path, peer: &str) -> HcResult {
    let manual_cmd = manual_hc_add_line(exe, peer);
    let Some(hc_path) = find_hc() else {
        return HcResult::MissingHc { manual_cmd };
    };
    let via_cmd = via_command_for_current_os(exe, peer);
    let mut cmd = tokio::process::Command::new(&hc_path);
    cmd.args(["machine", "add", "--via", &via_cmd, "--name", peer])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return HcResult::Failed {
                reason: log::sanitize(&err.to_string()),
                manual_cmd,
            };
        }
    };

    #[cfg(unix)]
    let child_pid = child.id();

    match tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => HcResult::Added,
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = stderr
                .lines()
                .chain(stdout.lines())
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(log::sanitize)
                .unwrap_or_else(|| output.status.to_string());
            HcResult::Failed {
                reason: detail,
                manual_cmd,
            }
        }
        Ok(Err(err)) => HcResult::Failed {
            reason: log::sanitize(&err.to_string()),
            manual_cmd,
        },
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                unsafe {
                    let _ = libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            HcResult::Failed {
                reason: "hc machine add timed out after 15 seconds".into(),
                manual_cmd,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
