const PREFIX: &str = "sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x \"$p\" ] || { echo \"herdr-core is not installed here\" >&2; exit 127; }; ";
const SUFFIX: &str = "exec \"$p\" connect'";

pub fn allowed(line: &str) -> bool {
    unix_session(line).is_some()
}

/// The session name of an allowed Unix line: `Some(None)` without one, `None` if not allowed.
fn unix_session(line: &str) -> Option<Option<&str>> {
    let middle = line
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(SUFFIX))?;
    if middle.is_empty() {
        return Some(None);
    }
    let name = middle
        .strip_prefix("HC_SESSION=")
        .and_then(|s| s.strip_suffix(' '))?;
    ((1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
    .then_some(Some(name))
}

/// The session name in a line that this platform runs, for the agent log.
pub(crate) fn session(line: &str) -> Option<&str> {
    #[cfg(windows)]
    return windows_session(line).flatten();
    #[cfg(not(windows))]
    unix_session(line).flatten()
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WindowsLine<'a> {
    UnixProbe,
    Connect(Option<&'a str>),
}

#[cfg(any(windows, test))]
pub(crate) fn classify_windows(line: &str) -> Option<WindowsLine<'_>> {
    if allowed(line) {
        Some(WindowsLine::UnixProbe)
    } else {
        windows_session(line).map(WindowsLine::Connect)
    }
}

#[cfg(any(windows, test))]
fn windows_session(line: &str) -> Option<Option<&str>> {
    const PREFIX: &str = r#"cmd /d /c "if exist .\AppData\Local\Programs\herdr-core\herdr-core.exe (.\AppData\Local\Programs\herdr-core\herdr-core.exe connect"#;
    const SUFFIX: &str = r#") else (echo herdr-core is not installed here 1>&2 & exit /b 127)""#;
    let middle = line.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    if middle.is_empty() {
        return Some(None);
    }
    let name = middle.strip_prefix(" --session ")?;
    if (1..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        Some(Some(name))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const BASE: &str = "sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x \"$p\" ] || { echo \"herdr-core is not installed here\" >&2; exit 127; }; exec \"$p\" connect'";

    fn session(name: &str) -> String {
        BASE.replace("; exec", &format!("; HC_SESSION={name} exec"))
    }

    #[test]
    fn accepts_only_the_two_exact_connect_templates() {
        assert!(allowed(BASE));
        assert!(allowed(&session("Abc-09._")));
        assert!(allowed(&session(&"x".repeat(64))));
    }

    #[cfg(unix)]
    #[test]
    fn names_the_session_of_a_line_it_runs() {
        assert_eq!(super::session(BASE), None);
        assert_eq!(super::session(&session("Abc-09._")), Some("Abc-09._"));
        assert_eq!(super::session(&session("a b")), None);
    }

    #[test]
    fn rejects_shell_injection_and_template_changes() {
        for name in ["", "a b", "a;b", "$(id)", "a\nb", "a'b", "한글", "a/b"] {
            assert!(!allowed(&session(name)), "{name:?}");
        }
        assert!(!allowed(&session(&"x".repeat(65))));
        for line in [
            "id".to_owned(),
            format!("{BASE}\n"),
            format!(" {BASE}"),
            format!("{BASE}; id"),
            BASE.replace("connect", "connect "),
            BASE.replace("127", "0"),
        ] {
            assert!(!allowed(&line), "{line:?}");
        }
    }

    const WINDOWS: &str = r#"cmd /d /c "if exist .\AppData\Local\Programs\herdr-core\herdr-core.exe (.\AppData\Local\Programs\herdr-core\herdr-core.exe connect) else (echo herdr-core is not installed here 1>&2 & exit /b 127)""#;

    fn windows_line(name: &str) -> String {
        WINDOWS.replace("connect)", &format!("connect --session {name})"))
    }

    #[test]
    fn windows_templates_extract_only_exact_session_arguments() {
        assert_eq!(windows_session(WINDOWS), Some(None));
        for name in ["a", "0", "a0-._", &"a".repeat(32)] {
            assert_eq!(windows_session(&windows_line(name)), Some(Some(name)));
        }
    }

    #[test]
    fn windows_classifies_unix_probe_separately_from_direct_execution() {
        assert_eq!(classify_windows(BASE), Some(WindowsLine::UnixProbe));
        assert_eq!(
            classify_windows(&session("Abc-09._")),
            Some(WindowsLine::UnixProbe)
        );
        assert_eq!(classify_windows(WINDOWS), Some(WindowsLine::Connect(None)));
        assert_eq!(
            classify_windows(&windows_line("a")),
            Some(WindowsLine::Connect(Some("a")))
        );
        for line in [
            format!("{BASE}; id"),
            format!("{BASE}\n"),
            BASE.replace("connect", "connect --session a"),
            format!("{WINDOWS}\n"),
        ] {
            assert_eq!(classify_windows(&line), None, "{line:?}");
        }
    }

    #[test]
    fn windows_templates_reject_injection_and_small_changes() {
        for name in [
            "",
            "A",
            "aB",
            "_a",
            ".a",
            "-a",
            "a b",
            "a&b",
            "a|b",
            "a%b",
            "a!b",
            "a\"b",
            "a\nb",
            "a\rb",
            "a\0b",
            "한글",
            &"a".repeat(33),
        ] {
            assert_eq!(windows_session(&windows_line(name)), None, "{name:?}");
        }
        for line in [
            BASE.to_owned(),
            format!(" {WINDOWS}"),
            format!("{WINDOWS}\n"),
            format!("{WINDOWS}&whoami"),
            WINDOWS.replace("connect)", "connect )"),
            WINDOWS.replace("127", "0"),
            WINDOWS.replace("cmd /d", "cmd /D"),
        ] {
            assert_eq!(windows_session(&line), None, "{line:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_rejects_windows_templates() {
        assert!(!allowed(WINDOWS));
        assert!(!allowed(&windows_line("a")));
    }
}
