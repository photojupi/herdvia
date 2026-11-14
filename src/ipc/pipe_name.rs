pub(super) fn pipe_name(sid: &str, directory: Option<&[u16]>) -> String {
    let mut name = format!(r"\\.\pipe\hlink-{sid}");
    if let Some(directory) = directory {
        // Fixed FNV-1a over UTF-16LE: stable across processes and Rust versions.
        // This separates paths; the SID and pipe ACL provide authentication.
        let hash = directory
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        name.push_str(&format!("-{hash:016x}"));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_is_scoped_to_the_user_sid() {
        assert_eq!(
            pipe_name("S-1-5-21-123", None),
            r"\\.\pipe\hlink-S-1-5-21-123"
        );
        assert_ne!(
            pipe_name("S-1-5-21-123", None),
            pipe_name("S-1-5-21-456", None)
        );
    }

    #[test]
    fn override_names_are_stable_and_distinct_including_non_unicode_paths() {
        let first: Vec<_> = r"C:\test\one".encode_utf16().collect();
        let second: Vec<_> = r"C:\test\two".encode_utf16().collect();
        let name = pipe_name("S-1-5-21-123", Some(&first));
        assert_eq!(name, pipe_name("S-1-5-21-123", Some(&first)));
        assert_ne!(name, pipe_name("S-1-5-21-123", None));
        assert_ne!(name, pipe_name("S-1-5-21-123", Some(&second)));
        assert_ne!(
            pipe_name("S-1-5-21-123", Some(&[0xd800])),
            pipe_name("S-1-5-21-123", Some(&[0xfffd]))
        );
    }
}
