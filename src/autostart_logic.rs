use std::io;

pub(crate) fn run_command(executable: &[u16]) -> io::Result<Vec<u16>> {
    if executable.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path contains NUL",
        ));
    }
    if executable.contains(&(b'%' as u16)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path contains '%'; move herdvia to a path without '%' to avoid conhost environment variable expansion",
        ));
    }

    let mut command: Vec<u16> = "conhost.exe --headless \"".encode_utf16().collect();
    let mut backslashes = 0;
    for &unit in executable {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        let count = if unit == b'"' as u16 {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        command.resize(command.len() + count, b'\\' as u16);
        command.push(unit);
        backslashes = 0;
    }
    command.resize(command.len() + backslashes * 2, b'\\' as u16);
    command.extend("\" agent --log\0".encode_utf16());
    if command.len() - 1 > 259 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "autostart command exceeds the Run key's 259 UTF-16 unit limit; move herdvia to a shorter path",
        ));
    }
    Ok(command)
}

/// Whether a Run value is the command `install` would register for this executable.
/// Windows paths ignore case, so ASCII letters are compared without it.
pub(crate) fn registered_for(value: &[u16], executable: &[u16]) -> bool {
    let end = value
        .iter()
        .rposition(|&unit| unit != 0)
        .map_or(0, |i| i + 1);
    let fold = |unit: &u16| match u8::try_from(*unit) {
        Ok(byte) => u16::from(byte.to_ascii_lowercase()),
        Err(_) => *unit,
    };
    run_command(executable).is_ok_and(|command| {
        command[..command.len() - 1]
            .iter()
            .map(fold)
            .eq(value[..end].iter().map(fold))
    })
}

pub(crate) fn install_requires_stop(registered: bool, running: bool) -> io::Result<bool> {
    if !registered && running {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "an agent is running without an autostart registration; stop it before installing",
        ));
    }
    Ok(running)
}

pub(crate) fn validate_install(herdvia_dir_set: bool) -> io::Result<()> {
    if herdvia_dir_set {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unset HERDVIA_DIR before installing autostart; it uses the default data directory",
        ));
    }
    Ok(())
}

pub(crate) fn unstable_path(executable: &str, temp: &str) -> bool {
    let normalize = |path: &str| {
        let path = path.replace('/', "\\").to_ascii_lowercase();
        if let Some(unc) = path.strip_prefix(r"\\?\unc\") {
            format!(r"\\{unc}")
        } else {
            path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
        }
    };
    let executable = normalize(executable);
    let temp = normalize(temp);
    let temp = temp.trim_end_matches('\\');
    executable.split('\\').any(|part| part == "target")
        || (!temp.is_empty()
            && (executable == temp
                || executable
                    .strip_prefix(temp)
                    .is_some_and(|suffix| suffix.starts_with('\\'))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_always_quotes_the_path_and_terminates_with_nul() {
        for (path, expected) in [
            (
                r"C:\Tools\herdvia.exe",
                r#"conhost.exe --headless "C:\Tools\herdvia.exe" agent --log"#,
            ),
            (
                r"C:\Program Files\한글\herdvia.exe",
                r#"conhost.exe --headless "C:\Program Files\한글\herdvia.exe" agent --log"#,
            ),
            (
                r#"C:\Tools\a"b.exe"#,
                r#"conhost.exe --headless "C:\Tools\a\"b.exe" agent --log"#,
            ),
            (
                r#"C:\Tools\a\\"b.exe"#,
                r#"conhost.exe --headless "C:\Tools\a\\\\\"b.exe" agent --log"#,
            ),
            (
                r"C:\Tools\",
                r#"conhost.exe --headless "C:\Tools\\" agent --log"#,
            ),
        ] {
            let expected: Vec<u16> = expected.encode_utf16().chain([0]).collect();
            assert_eq!(
                run_command(&path.encode_utf16().collect::<Vec<_>>()).unwrap(),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn run_command_preserves_unpaired_surrogates() {
        let path = [
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
            b'.' as u16,
            0xdc00,
        ];
        let mut expected: Vec<u16> = "conhost.exe --headless \"C:\\".encode_utf16().collect();
        expected.extend([0xd800, b'.' as u16, 0xdc00]);
        expected.extend("\" agent --log\0".encode_utf16());
        assert_eq!(run_command(&path).unwrap(), expected);
    }

    #[test]
    fn run_command_rejects_embedded_nul() {
        assert_eq!(
            run_command(&[b'a' as u16, 0, b'b' as u16])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn run_command_rejects_percent_in_the_path() {
        for path in [r"C:\%TEMP%\herdvia.exe", r"C:\100%\herdvia.exe"] {
            assert_eq!(
                run_command(&path.encode_utf16().collect::<Vec<_>>())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput,
                "{path}"
            );
        }
    }

    #[test]
    fn run_command_allows_259_units_but_rejects_260_before_the_nul() {
        assert_eq!(run_command(&[b'a' as u16; 222]).unwrap().len(), 260);
        assert_eq!(
            run_command(&[b'a' as u16; 223]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn run_command_counts_escape_expansion_in_the_run_key_limit() {
        let mut path = vec![b'a' as u16; 221];
        path.push(b'\\' as u16);
        assert_eq!(
            run_command(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn registered_for_matches_only_this_executables_command() {
        let wide = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        let value = run_command(&wide(r"C:\Tools\herdvia.exe")).unwrap();
        // With or without the stored terminator.
        assert!(registered_for(&value, &wide(r"C:\Tools\herdvia.exe")));
        assert!(registered_for(
            &value[..value.len() - 1],
            &wide(r"C:\Tools\herdvia.exe")
        ));
        assert!(registered_for(&value, &wide(r"c:\tools\HERDVIA.exe")));
        assert!(!registered_for(&value, &wide(r"C:\Other\herdvia.exe")));
        assert!(!registered_for(
            &wide("something else"),
            &wide(r"C:\Tools\herdvia.exe")
        ));
        assert!(!registered_for(&[], &wide(r"C:\100%\herdvia.exe")));
    }

    #[test]
    fn validate_install_rejects_any_herdvia_dir_setting() {
        assert_eq!(
            validate_install(true).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn validate_install_allows_the_default_data_directory() {
        assert!(validate_install(false).is_ok());
    }

    #[test]
    fn install_refuses_a_running_manual_agent() {
        assert_eq!(
            install_requires_stop(false, true).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
    }

    #[test]
    fn install_stops_only_a_running_registered_agent() {
        for (registered, running, expected) in [
            (false, false, false),
            (true, false, false),
            (true, true, true),
        ] {
            assert_eq!(
                install_requires_stop(registered, running).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn unstable_path_matches_temp_case_insensitively_at_component_boundaries() {
        for (path, temp, expected) in [
            (r"C:\Temp\herdvia.exe", r"c:\temp", true),
            (r"C:/TEMP/nested/herdvia.exe", r"c:\temp\", true),
            (r"C:\Tempest\herdvia.exe", r"c:\temp", false),
            (r"C:\Tools\herdvia.exe", "", false),
            (r"C:\Temp", r"c:\temp", true),
        ] {
            assert_eq!(unstable_path(path, temp), expected, "{path}");
        }
    }

    #[test]
    fn unstable_path_matches_target_components() {
        for (path, expected) in [
            (r"C:\repo\target\release\herdvia.exe", true),
            (r"C:/repo/TARGET/release/herdvia.exe", true),
            (r"C:\repo\targeted\herdvia.exe", false),
            (r"C:\repo\notarget\herdvia.exe", false),
        ] {
            assert_eq!(unstable_path(path, r"C:\Temp"), expected, "{path}");
        }
    }

    #[test]
    fn unstable_path_handles_unc_and_extended_paths() {
        for (path, temp, expected) in [
            (
                r"\\server\share\Temp\herdvia.exe",
                r"\\SERVER\share\temp",
                true,
            ),
            (r"\\?\C:\Temp\herdvia.exe", r"C:\Temp", true),
            (r"C:\Temp\herdvia.exe", r"\\?\C:\Temp", true),
            (
                r"\\?\UNC\server\share\Temp\herdvia.exe",
                r"\\server\share\temp",
                true,
            ),
            (r"\\?\C:\Tempest\herdvia.exe", r"C:\Temp", false),
        ] {
            assert_eq!(unstable_path(path, temp), expected, "{path}");
        }
    }
}
