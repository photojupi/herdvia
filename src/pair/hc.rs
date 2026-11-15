use crate::{ipc, log};
use std::{
    env, fs,
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

pub fn windows_via_command(exe: &Path, peer: &str) -> String {
    let exe_str = exe.to_string_lossy();
    let needs_quotes = exe_str.is_empty()
        || exe_str
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"'));
    if !needs_quotes {
        return format!("{exe_str} via {peer}");
    }

    let mut quoted = String::with_capacity(exe_str.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for ch in exe_str.chars() {
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
    format!("{quoted} via {peer}")
}

pub fn via_command_for_current_os(exe: &Path, peer: &str) -> String {
    if cfg!(windows) {
        windows_via_command(exe, peer)
    } else {
        sh_via_command(exe, peer)
    }
}

pub fn manual_hc_add_line(exe: &Path, peer: &str) -> String {
    if cfg!(windows) {
        let via = windows_via_command(exe, peer);
        format!(
            "hc machine add --via '{}' --name {peer}",
            via.replace('\'', "''")
        )
    } else {
        let via = sh_via_command(exe, peer);
        format!(
            "hc machine add --via '{}' --name {peer}",
            via.replace('\'', r"'\''")
        )
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

fn find_hc() -> Option<PathBuf> {
    let bin_name = if cfg!(windows) { "hc.exe" } else { "hc" };
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(bin_name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
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
            let line = manual_hc_add_line(
                Path::new(r"C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe"),
                "home",
            );
            assert_eq!(
                line,
                r#"hc machine add --via '"C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe" via home' --name home"#
            );
        }
    }
}