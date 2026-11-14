#[cfg(any(windows, test))]
mod pipe_name;
#[cfg(any(windows, test))]
mod pipe_state;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

use std::{ffi::OsString, io, path::PathBuf};

/// The HERDVIA_DIR value as the data directory. An empty or relative value would
/// follow the current directory, so only an absolute path is accepted.
fn override_dir(value: OsString) -> io::Result<PathBuf> {
    let dir = PathBuf::from(value);
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "HERDVIA_DIR must be an absolute path, not \"{}\"",
                dir.display()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdvia_dir_must_be_absolute() {
        for value in ["", ".", "rel", "rel/dir"] {
            let err = override_dir(value.into()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{value:?}");
            assert_eq!(
                err.to_string(),
                format!("HERDVIA_DIR must be an absolute path, not \"{value}\"")
            );
        }
        #[cfg(unix)]
        let absolute = "/tmp/herdvia";
        #[cfg(windows)]
        let absolute = r"C:\herdvia";
        assert_eq!(
            override_dir(absolute.into()).unwrap(),
            PathBuf::from(absolute)
        );
        // Rooted but drive-relative, or drive-relative without a root.
        #[cfg(windows)]
        for value in [r"\herdvia", r"C:herdvia"] {
            assert!(override_dir(value.into()).is_err(), "{value:?}");
        }
    }
}
