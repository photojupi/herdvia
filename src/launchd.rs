use crate::ipc;
use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::Path,
    process::{Command, Output},
};
use tokio::net::UnixStream;

const LABEL: &str = "io.github.photojupi.herdvia.agent";
const PLIST: &str = "Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist";
// A service that launchd respawned moments ago took 2.3s to leave after bootout
// (measured on macOS 27), so allow 10s of 250ms retries.
const BOOTSTRAP_RETRIES: u32 = if cfg!(test) { 4 } else { 40 };

pub async fn install() -> io::Result<()> {
    require_macos()?;
    install_at(
        &ipc::home_dir()?,
        &env::current_exe()?,
        env::var_os("HERDVIA_DIR").as_deref(),
        ipc::uid(),
        launchctl,
    )
    .await
}

pub fn uninstall() -> io::Result<()> {
    require_macos()?;
    uninstall_at(&ipc::home_dir()?, ipc::uid(), launchctl)
}

fn require_macos() -> io::Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "agent install/uninstall is only supported on macOS",
        ))
    }
}

fn launchctl(args: &[&OsStr]) -> io::Result<Output> {
    Command::new("/bin/launchctl").args(args).output()
}

fn check_output(operation: &str, output: &Output) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "launchctl {operation} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn service_missing(output: &Output) -> bool {
    // ESRCH (bootout) and launchctl's service-not-found status (print).
    matches!(output.status.code(), Some(3 | 113))
}

fn bootout(service: &str, run: &mut impl FnMut(&[&OsStr]) -> io::Result<Output>) -> io::Result<()> {
    let output = run(&[OsStr::new("bootout"), OsStr::new(service)])?;
    if service_missing(&output) {
        Ok(())
    } else {
        check_output("bootout", &output)
    }
}

async fn install_at(
    home: &Path,
    executable: &Path,
    herdvia_dir: Option<&OsStr>,
    uid: u32,
    mut run: impl FnMut(&[&OsStr]) -> io::Result<Output>,
) -> io::Result<()> {
    if herdvia_dir.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unset HERDVIA_DIR before installing the agent",
        ));
    }
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LABEL}");
    let output = run(&[OsStr::new("print"), OsStr::new(&service)])?;
    let loaded = output.status.success();
    if !loaded && !service_missing(&output) {
        check_output("print", &output)?;
    }
    let data = home.join("Library/Application Support/herdvia");
    match ipc::lock_dir(&data) {
        Ok(lock) => drop(lock),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            let service_pid = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.trim().strip_prefix("pid = ")?.parse::<i32>().ok());
            // A held lock is authoritative even when the socket is missing.
            // Only a matching socket PID identifies a managed agent for reinstall.
            let managed = match UnixStream::connect(data.join("agent.sock")).await {
                Ok(stream) => {
                    ipc::check_uid(&stream)?;
                    let peer_pid = stream.peer_cred()?.pid();
                    loaded && peer_pid.is_some() && peer_pid == service_pid
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    false
                }
                Err(err) => return Err(err),
            };
            if !managed {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "agent is running manually or cannot be identified as the launchd service; stop it before installing",
                ));
            }
        }
        Err(err) => return Err(err),
    }
    let contents = plist(home, executable)?;
    if unstable_path(executable, &env::temp_dir()) {
        eprintln!(
            "herdvia: warning: executable is in a temporary or target directory: {}",
            executable.display()
        );
    }
    let path = home.join(PLIST);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::create_dir_all(home.join("Library/Logs"))?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(contents.as_bytes())?;
    if loaded {
        bootout(&service, &mut run)?;
    }
    file.persist(&path).map_err(|err| err.error)?;
    let recovery_error = |err: io::Error| {
        io::Error::new(
            err.kind(),
            format!("{err}; agent is now stopped; run herdvia agent install again"),
        )
    };
    let mut retries = 0;
    loop {
        let output = run(&[
            OsStr::new("bootstrap"),
            OsStr::new(&domain),
            path.as_os_str(),
        ])
        .map_err(recovery_error)?;
        if !loaded || output.status.code() != Some(5) || retries == BOOTSTRAP_RETRIES {
            return check_output("bootstrap", &output).map_err(recovery_error);
        }
        // bootout may return before the old service has finished shutting down.
        retries += 1;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn uninstall_at(
    home: &Path,
    uid: u32,
    mut run: impl FnMut(&[&OsStr]) -> io::Result<Output>,
) -> io::Result<()> {
    bootout(&format!("gui/{uid}/{LABEL}"), &mut run)?;
    match fs::remove_file(home.join(PLIST)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn xml_path(path: &Path) -> io::Result<String> {
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch agent paths must be UTF-8",
        )
    })?;
    Ok(text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn plist(home: &Path, executable: &Path) -> io::Result<String> {
    let executable = xml_path(executable)?;
    let log = xml_path(&home.join("Library/Logs/herdvia.log"))?;
    let path = xml_path(&home.join(".local/bin"))?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{executable}</string>
        <string>agent</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
</dict>
</plist>
"#
    ))
}

fn unstable_path(executable: &Path, temp: &Path) -> bool {
    let executable = Path::new("/").join(executable.strip_prefix("/private").unwrap_or(executable));
    let temp = Path::new("/").join(temp.strip_prefix("/private").unwrap_or(temp));
    executable.starts_with(temp)
        || executable.starts_with("/tmp")
        || executable
            .components()
            .any(|part| part.as_os_str() == "target")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::{
            fd::AsRawFd,
            unix::{fs::PermissionsExt, process::ExitStatusExt},
        },
        process::ExitStatus,
    };
    use tokio::net::UnixListener;

    fn temp_home() -> tempfile::TempDir {
        // Leave room for the default data path within macOS's 104-byte socket limit.
        tempfile::Builder::new().prefix("h").tempdir().unwrap()
    }

    fn output(code: i32, stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: if code == 0 {
                Vec::new()
            } else {
                b"launchctl failure".to_vec()
            },
        }
    }

    fn args_owned(args: &[&OsStr]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn plist_contains_launch_agent_settings_and_escaped_paths() {
        let dir = temp_home();
        let home = dir.path().join("한 글 & <home>\"'");
        let executable = home.join("bin/herdvia");
        let xml = plist(&home, &executable).unwrap();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(
            xml.contains(
                "<key>Label</key>\n    <string>io.github.photojupi.herdvia.agent</string>"
            )
        );
        assert!(xml.contains(&format!(
            "<key>ProgramArguments</key>\n    <array>\n        <string>{}/한 글 &amp; &lt;home&gt;&quot;&apos;/bin/herdvia</string>\n        <string>agent</string>\n    </array>",
            dir.path().display()
        )));
        assert!(xml.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(xml.contains("<key>KeepAlive</key>\n    <true/>"));
        for key in ["StandardOutPath", "StandardErrorPath"] {
            assert!(xml.contains(&format!(
                "<key>{key}</key>\n    <string>{}/한 글 &amp; &lt;home&gt;&quot;&apos;/Library/Logs/herdvia.log</string>",
                dir.path().display()
            )));
        }
        assert!(xml.contains(&format!(
            "<key>EnvironmentVariables</key>\n    <dict>\n        <key>PATH</key>\n        <string>{}/한 글 &amp; &lt;home&gt;&quot;&apos;/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>",
            dir.path().display()
        )));
        assert!(!xml.contains("HERDVIA_DIR"));
        #[cfg(target_os = "macos")]
        {
            let path = dir.path().join("agent.plist");
            fs::write(&path, &xml).unwrap();
            let result = Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&path)
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
    }

    #[tokio::test]
    async fn rejects_herdvia_dir_even_when_empty_without_any_changes() {
        let dir = temp_home();
        for value in ["", "/custom"] {
            let home = dir.path().join("home");
            let err = install_at(
                &home,
                Path::new("/opt/herdvia"),
                Some(OsStr::new(value)),
                501,
                |_| panic!("HERDVIA_DIR must be rejected before launchctl"),
            )
            .await
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            assert!(err.to_string().contains("HERDVIA_DIR"));
            assert!(!home.exists());
        }
    }

    #[tokio::test]
    async fn refuses_manual_agent_even_with_an_old_plist_or_loaded_job() {
        let dir = temp_home();
        // Keep the socket path short enough for macOS's sockaddr_un.
        let home = dir.path();
        let data = home.join("Library/Application Support/herdvia");
        fs::create_dir_all(&data).unwrap();
        let listener = ipc::listen(&data).unwrap();
        let path = home.join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old plist").unwrap();
        for (code, status) in [(113, ""), (0, "state = waiting\n"), (0, "pid = 1\n")] {
            let err = install_at(home, Path::new("/opt/herdvia"), None, 501, |args| {
                assert_eq!(
                    args_owned(args),
                    ["print", "gui/501/io.github.photojupi.herdvia.agent"]
                );
                Ok(output(code, status))
            })
            .await
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
            assert!(err.to_string().contains("manually"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "old plist");
            assert!(!home.join("Library/Logs").exists());
        }
        drop(listener);
    }

    #[tokio::test]
    async fn refuses_locked_directory_with_missing_or_deleted_socket_without_changes() {
        for create_socket in [false, true] {
            let dir = temp_home();
            let home = dir.path();
            let data = home.join("Library/Application Support/herdvia");
            fs::create_dir_all(&data).unwrap();
            let lock = fs::File::open(&data).unwrap();
            // SAFETY: lock owns an open directory descriptor throughout this test.
            assert_eq!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            let _socket = if create_socket {
                let socket = UnixListener::bind(data.join("agent.sock")).unwrap();
                fs::remove_file(data.join("agent.sock")).unwrap();
                Some(socket)
            } else {
                None
            };
            let path = home.join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "old plist").unwrap();
            for (code, status) in [(113, ""), (0, "state = waiting\n"), (0, "pid = 1\n")] {
                let mut calls = Vec::new();
                let err = install_at(home, Path::new("/opt/herdvia"), None, 501, |args| {
                    calls.push(args_owned(args));
                    Ok(output(if args[0] == "print" { code } else { 0 }, status))
                })
                .await
                .unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
                assert_eq!(
                    calls,
                    [vec!["print", "gui/501/io.github.photojupi.herdvia.agent"]]
                );
                assert_eq!(fs::read_to_string(&path).unwrap(), "old plist");
                assert!(!home.join("Library/Logs").exists());
                assert!(!data.join("agent.sock").exists());
            }
        }
    }

    #[tokio::test]
    async fn ignores_unlocked_socket_and_releases_probe_lock_before_bootstrap() {
        let dir = temp_home();
        let data = dir.path().join("Library/Application Support/herdvia");
        fs::create_dir_all(&data).unwrap();
        let _socket = UnixListener::bind(data.join("agent.sock")).unwrap();
        install_at(dir.path(), Path::new("/opt/herdvia"), None, 501, |args| {
            if args[0] == "bootstrap" {
                let lock = fs::File::open(&data).unwrap();
                // SAFETY: lock owns a valid descriptor and is dropped after the probe.
                assert_eq!(
                    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
            }
            Ok(output(if args[0] == "print" { 113 } else { 0 }, ""))
        })
        .await
        .unwrap();
        assert!(
            dir.path()
                .join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist")
                .is_file()
        );
    }

    #[tokio::test]
    async fn installs_then_reinstalls_using_the_current_executable() {
        let dir = temp_home();
        let home = dir.path();
        let path = home.join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
        let mut calls = Vec::new();
        install_at(home, Path::new("/opt/first/herdvia"), None, 501, |args| {
            let args = args_owned(args);
            let code = if args[0] == "print" { 113 } else { 0 };
            if args[0] == "bootstrap" {
                assert!(
                    fs::read_to_string(&path)
                        .unwrap()
                        .contains("/opt/first/herdvia")
                );
            }
            calls.push(args);
            Ok(output(code, ""))
        })
        .await
        .unwrap();
        assert_eq!(
            calls,
            [
                vec![
                    "print".to_owned(),
                    "gui/501/io.github.photojupi.herdvia.agent".to_owned()
                ],
                vec![
                    "bootstrap".to_owned(),
                    "gui/501".to_owned(),
                    path.to_str().unwrap().to_owned()
                ],
            ]
        );
        assert!(home.join("Library/Logs").is_dir());

        let data = home.join("Library/Application Support/herdvia");
        fs::create_dir_all(&data).unwrap();
        let listener = ipc::listen(&data).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        calls.clear();
        install_at(home, Path::new("/opt/new/herdvia"), None, 501, |args| {
            let args = args_owned(args);
            if args[0] == "bootout" {
                assert!(
                    fs::read_to_string(&path)
                        .unwrap()
                        .contains("/opt/first/herdvia")
                );
            }
            if args[0] == "bootstrap" {
                assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o022, 0);
            }
            calls.push(args);
            Ok(output(
                0,
                &format!("state = running\n\tpid = {}\n", std::process::id()),
            ))
        })
        .await
        .unwrap();
        assert_eq!(
            calls,
            [
                vec![
                    "print".to_owned(),
                    "gui/501/io.github.photojupi.herdvia.agent".to_owned()
                ],
                vec![
                    "bootout".to_owned(),
                    "gui/501/io.github.photojupi.herdvia.agent".to_owned()
                ],
                vec![
                    "bootstrap".to_owned(),
                    "gui/501".to_owned(),
                    path.to_str().unwrap().to_owned()
                ],
            ]
        );
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("/opt/new/herdvia")
        );
        drop(listener);
    }

    #[tokio::test]
    async fn ignores_a_stale_socket_when_installing() {
        let dir = temp_home();
        let data = dir.path().join("Library/Application Support/herdvia");
        fs::create_dir_all(&data).unwrap();
        drop(UnixListener::bind(data.join("agent.sock")).unwrap());
        install_at(dir.path(), Path::new("/opt/herdvia"), None, 501, |args| {
            Ok(output(if args[0] == "print" { 113 } else { 0 }, ""))
        })
        .await
        .unwrap();
        assert!(
            dir.path()
                .join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist")
                .is_file()
        );
    }

    #[test]
    fn uninstall_is_idempotent_and_preserves_data_and_logs() {
        let dir = temp_home();
        let home = dir.path();
        let path = home.join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old plist").unwrap();
        let log = home.join("Library/Logs/herdvia.log");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "log").unwrap();
        let key = home.join("Library/Application Support/herdvia/secret.key");
        fs::create_dir_all(key.parent().unwrap()).unwrap();
        fs::write(&key, "key").unwrap();
        for code in [0, 3, 113] {
            let mut calls = Vec::new();
            uninstall_at(home, 501, |args| {
                calls.push(args_owned(args));
                Ok(output(code, ""))
            })
            .unwrap();
            assert_eq!(
                calls,
                [vec!["bootout", "gui/501/io.github.photojupi.herdvia.agent"]]
            );
            assert!(!path.exists());
            assert_eq!(fs::read_to_string(&log).unwrap(), "log");
            assert_eq!(fs::read_to_string(&key).unwrap(), "key");
        }
    }

    #[tokio::test]
    async fn launchctl_failures_preserve_old_plist_until_bootstrap_and_report_recovery() {
        let dir = temp_home();
        let path = dir
            .path()
            .join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old plist").unwrap();
        for failing_command in ["print", "bootout", "bootstrap"] {
            let mut attempts = 0;
            let err = install_at(dir.path(), Path::new("/opt/herdvia"), None, 501, |args| {
                if args[0] == "bootstrap" {
                    attempts += 1;
                }
                Ok(output(if args[0] == failing_command { 5 } else { 0 }, ""))
            })
            .await
            .unwrap_err();
            assert!(err.to_string().contains(failing_command));
            assert!(err.to_string().contains("launchctl failure"));
            if failing_command == "bootstrap" {
                assert_eq!(attempts, 5);
                let xml = fs::read_to_string(&path).unwrap();
                assert!(xml.contains("<string>/opt/herdvia</string>"));
                assert!(!xml.contains("old plist"));
                assert!(err.to_string().contains("agent is now stopped"));
                assert!(err.to_string().contains("herdvia agent install again"));
            } else {
                assert_eq!(fs::read_to_string(&path).unwrap(), "old plist");
            }
        }
        fs::write(&path, "old plist").unwrap();
        assert!(uninstall_at(dir.path(), 501, |_| Ok(output(5, ""))).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "old plist");
    }

    #[tokio::test]
    async fn retries_bootstrap_after_bootout_until_the_old_service_exits() {
        let dir = temp_home();
        let path = dir
            .path()
            .join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old plist").unwrap();
        let mut calls = Vec::new();
        let mut attempts = 0;
        install_at(
            dir.path(),
            Path::new("/opt/new/herdvia"),
            None,
            501,
            |args| {
                calls.push(args_owned(args));
                if args[0] == "bootstrap" {
                    attempts += 1;
                    assert!(
                        fs::read_to_string(&path)
                            .unwrap()
                            .contains("/opt/new/herdvia")
                    );
                    return Ok(output(if attempts < 3 { 5 } else { 0 }, ""));
                }
                Ok(output(0, ""))
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(
            calls[0],
            ["print", "gui/501/io.github.photojupi.herdvia.agent"]
        );
        assert_eq!(
            calls[1],
            ["bootout", "gui/501/io.github.photojupi.herdvia.agent"]
        );
        assert_eq!(calls.len(), 5);
        for call in &calls[2..] {
            assert_eq!(call, &["bootstrap", "gui/501", path.to_str().unwrap()]);
        }
    }

    #[tokio::test]
    async fn bootstrap_spawn_failure_keeps_new_plist_and_reports_recovery() {
        let dir = temp_home();
        let path = dir
            .path()
            .join("Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old plist").unwrap();
        let err = install_at(
            dir.path(),
            Path::new("/opt/new/herdvia"),
            None,
            501,
            |args| {
                if args[0] == "bootstrap" {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "launchctl unavailable",
                    ));
                }
                Ok(output(0, ""))
            },
        )
        .await
        .unwrap_err();
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("/opt/new/herdvia")
        );
        assert!(err.to_string().contains("launchctl unavailable"));
        assert!(err.to_string().contains("agent is now stopped"));
        assert!(err.to_string().contains("herdvia agent install again"));
    }

    #[test]
    fn warns_for_temporary_or_target_components_only() {
        let temp = Path::new("/custom/temp");
        for path in [
            "/tmp/herdvia",
            "/private/tmp/herdvia",
            "/custom/temp/herdvia",
            "/repo/target/release/herdvia",
        ] {
            assert!(unstable_path(Path::new(path), temp), "{path}");
        }
        for path in [
            "/opt/herdvia",
            "/repo/targeted/herdvia",
            "/custom/temporary/herdvia",
        ] {
            assert!(!unstable_path(Path::new(path), temp), "{path}");
        }
    }
}
