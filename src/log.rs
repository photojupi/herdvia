//! The agent's log lines start with their UTC time, which lets the agent drop old ones.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_AGE: u64 = 180 * 24 * 60 * 60;
const DAY: u64 = 24 * 60 * 60;
/// How often the agent checks whether a trim is due. Tokio's timers stop while a Mac sleeps,
/// so it checks the wall clock every hour instead of waiting 24 hours.
pub(crate) const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The current UTC time for the start of a log line, such as `2026-11-14T14:17:03Z`.
pub(crate) fn stamp() -> String {
    timestamp(unix_now())
}

fn timestamp(secs: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem / 60 % 60,
        rem % 60
    )
}

/// The time at the start of a log line written with `stamp`, in seconds since the epoch.
fn parse(line: &[u8]) -> Option<u64> {
    if line.get(20) != Some(&b' ') {
        return None;
    }
    let text = std::str::from_utf8(&line[..20]).ok()?;
    let field = |start: usize, end: usize| text.get(start..end)?.parse::<i64>().ok();
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = y.div_euclid(400) * 146_097 + doe - 719_468;
    let secs = days * 86_400 + field(11, 13)? * 3_600 + field(14, 16)? * 60 + field(17, 19)?;
    // Formatting back rejects out-of-range fields, signs and wrong separators.
    let secs = u64::try_from(secs).ok()?;
    (timestamp(secs) == text).then_some(secs)
}

/// How many leading bytes to drop: through the last line stamped before `cutoff` that comes
/// before the first line stamped at or after it. Unstamped lines go with the next stamped line.
fn expired(data: &[u8], cutoff: u64) -> usize {
    let (mut cut, mut end) = (0, 0);
    for line in data.split_inclusive(|&b| b == b'\n') {
        end += line.len();
        match parse(line) {
            Some(time) if time >= cutoff => break,
            Some(_) => cut = end,
            None => {}
        }
    }
    cut
}

/// Drops the expired start of `file` in place, keeping its inode for the append handles that
/// write to it. Leaves the file alone when nothing has expired.
fn rewrite(file: &mut File, cutoff: u64) -> io::Result<()> {
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let cut = expired(&data, cutoff);
    if cut == 0 {
        return Ok(());
    }
    // Writing before truncating loses less if either step fails.
    file.rewind()?;
    file.write_all(&data[cut..])?;
    file.set_len((data.len() - cut) as u64)
}

/// Whether standard error appends to `file`, as it does under the LaunchAgent.
#[cfg(unix)]
fn is_stderr(file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: F_GETFL only reads descriptor 2's status flags.
    let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: stat is a writable, correctly sized buffer for fstat to fill.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: fstat only reads descriptor 2's metadata into stat.
    if unsafe { libc::fstat(libc::STDERR_FILENO, &mut stat) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let metadata = file.metadata()?;
    Ok(flags & libc::O_APPEND != 0
        && stat.st_dev as u64 == metadata.dev()
        && stat.st_ino as u64 == metadata.ino())
}

fn trim(path: &Path, now: u64) -> io::Result<()> {
    // Hold standard error so that no log line of this process lands mid-rewrite and is lost.
    let _stderr = io::stderr().lock();
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    #[cfg(unix)]
    if !is_stderr(&file)? {
        return Ok(());
    }
    rewrite(&mut file, now.saturating_sub(MAX_AGE))
}

/// Whether a trim is due at `now`, 24 hours of wall-clock time after the `last` one.
fn due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| now.abs_diff(last) >= DAY)
}

/// Drops lines older than 180 days from the log at `path` if 24 hours of wall-clock time have
/// passed since the `last` trim, or if there has been none.
pub(crate) async fn trim_if_due(path: &Path, last: &mut Option<u64>) {
    let now = unix_now();
    if !due(*last, now) {
        return;
    }
    *last = Some(now);
    let file = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || trim(&file, now))
        .await
        .unwrap_or_else(|err| Err(io::Error::other(err)));
    if let Err(err) = result {
        eprintln!(
            "{} herdvia: cannot remove old lines from {}: {err}",
            stamp(),
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn formats_known_times() {
        for (secs, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_704_067_199, "2023-12-31T23:59:59Z"),
            (1_704_067_200, "2024-01-01T00:00:00Z"),
            (1_709_164_800, "2024-02-29T00:00:00Z"),
            (4_107_456_000, "2100-02-28T00:00:00Z"),
            (4_107_542_400, "2100-03-01T00:00:00Z"),
            (1_794_665_823, "2026-11-14T14:17:03Z"),
            (253_402_300_799, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(timestamp(secs), text);
            assert_eq!(parse(format!("{text} herdvia: x").as_bytes()), Some(secs));
        }
    }

    #[test]
    fn parses_only_a_stamp_followed_by_a_space() {
        assert_eq!(
            parse(b"2026-11-14T14:17:03Z herdvia: connected x\n"),
            Some(1_794_665_823)
        );
        for line in [
            &b""[..],
            b"herdvia: connected x\n",
            b"2026-11-14T14:17:03Z",
            b"2026-11-14T14:17:03Zherdvia\n",
            b"2026-11-14 14:17:03Z herdvia\n",
            b"2026-11-14T14:17:03+ herdvia\n",
            b"2026-13-01T00:00:00Z herdvia\n",
            b"2026-02-30T00:00:00Z herdvia\n",
            b"2026-11-14T24:00:00Z herdvia\n",
            b"2026-11-14T14:60:00Z herdvia\n",
            b"2026-11-14T14:17:60Z herdvia\n",
            b"1969-12-31T23:59:59Z herdvia\n",
            b"+026-11-14T14:17:03Z herdvia\n",
            b"2026-+1-14T14:17:03Z herdvia\n",
            b" 2026-11-14T14:17:03Z herdvia\n",
            "2026-11-14T14:17:0３ herdvia\n".as_bytes(),
        ] {
            assert_eq!(parse(line), None, "{:?}", String::from_utf8_lossy(line));
        }
    }

    const CUTOFF: u64 = 1_794_665_823;

    fn at(secs: u64, text: &str) -> String {
        format!("{} herdvia: {text}\n", timestamp(secs))
    }

    /// The text that remains after dropping the expired start.
    fn kept(data: &str) -> &str {
        &data[expired(data.as_bytes(), CUTOFF)..]
    }

    #[test]
    fn keeps_everything_when_nothing_has_expired() {
        assert_eq!(kept(""), "");
        assert_eq!(kept("plain\nlines\n"), "plain\nlines\n");
        let data = format!("old\n{}{}", at(CUTOFF, "a"), at(CUTOFF + 1, "b"));
        assert_eq!(kept(&data), data);
    }

    #[test]
    fn drops_through_the_last_expired_stamp_when_all_have_expired() {
        let data = format!(
            "{}continued\n{}trailing\n",
            at(CUTOFF - 2, "a"),
            at(CUTOFF - 1, "b")
        );
        assert_eq!(kept(&data), "trailing\n");
        assert_eq!(kept(&at(CUTOFF - 1, "a")), "");
    }

    #[test]
    fn unstamped_lines_before_the_first_stamp_go_with_it() {
        let data = format!("from an older version\n{}", at(CUTOFF - 1, "a"));
        assert_eq!(kept(&data), "");
        let data = format!("from an older version\n{}", at(CUTOFF, "a"));
        assert_eq!(kept(&data), data);
    }

    #[test]
    fn keeps_unstamped_lines_between_the_last_expired_and_first_kept_stamp() {
        let data = format!(
            "{}continued\n{}",
            at(CUTOFF - 1, "old"),
            at(CUTOFF + 1, "new")
        );
        assert_eq!(kept(&data), format!("continued\n{}", at(CUTOFF + 1, "new")));
    }

    #[test]
    fn keeps_old_stamps_after_the_first_kept_stamp() {
        // The clock went back: everything from the first kept line on stays.
        let data = format!(
            "{}{}{}",
            at(CUTOFF - 5, "old"),
            at(CUTOFF + 5, "new"),
            at(CUTOFF - 9, "clock went back")
        );
        assert_eq!(
            kept(&data),
            format!(
                "{}{}",
                at(CUTOFF + 5, "new"),
                at(CUTOFF - 9, "clock went back")
            )
        );
    }

    #[test]
    fn handles_a_last_line_without_a_newline() {
        let old = at(CUTOFF - 1, "old");
        assert_eq!(kept(old.trim_end()), "");
        let new = at(CUTOFF + 1, "new");
        let data = format!("{old}{}", new.trim_end());
        assert_eq!(kept(&data), new.trim_end());
        assert_eq!(kept(&format!("{old}partial")), "partial");
    }

    #[test]
    fn rewrites_in_place_so_open_append_handles_write_at_the_new_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let new = at(CUTOFF + 1, "new");
        fs::write(&path, format!("{}{new}", at(CUTOFF - 1, "old"))).unwrap();
        let mut append = OpenOptions::new().append(true).open(&path).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        rewrite(&mut file, CUTOFF).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), new);
        append.write_all(b"after\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{new}after\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
    }

    #[test]
    fn does_not_write_when_nothing_has_expired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let data = format!("plain\n{}", at(CUTOFF, "new"));
        fs::write(&path, &data).unwrap();
        // A read-only handle fails any write.
        rewrite(&mut File::open(&path).unwrap(), CUTOFF).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), data);
    }

    #[test]
    fn trims_after_24_hours_of_wall_clock_time() {
        assert!(due(None, CUTOFF));
        assert!(!due(Some(CUTOFF), CUTOFF + DAY - 1));
        assert!(due(Some(CUTOFF), CUTOFF + DAY));
        // A clock set back by a day or more does not stop the trims.
        assert!(!due(Some(CUTOFF), CUTOFF - DAY + 1));
        assert!(due(Some(CUTOFF), CUTOFF - DAY));
    }

    #[cfg(unix)]
    #[test]
    fn leaves_a_file_that_is_not_standard_error_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let data = at(1, "old");
        fs::write(&path, &data).unwrap();
        trim(&path, CUTOFF).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), data);
        trim(&dir.path().join("missing.log"), CUTOFF).unwrap();
    }
}
