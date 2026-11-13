const PREFIX: &str = "sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x \"$p\" ] || { echo \"herdr-core is not installed here\" >&2; exit 127; }; ";
const SUFFIX: &str = "exec \"$p\" connect'";

pub fn allowed(line: &str) -> bool {
    let Some(middle) = line
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(SUFFIX))
    else {
        return false;
    };
    if middle.is_empty() {
        return true;
    }
    let Some(name) = middle
        .strip_prefix("HC_SESSION=")
        .and_then(|s| s.strip_suffix(' '))
    else {
        return false;
    };
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
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
}
