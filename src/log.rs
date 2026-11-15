//! The agent's log lines start with their UTC time, which lets the agent drop old ones.
//! Every line the agent writes while running goes through `line`, which keeps each one to a
//! single line of limited length and limits how fast lines are written.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{LazyLock, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_AGE: u64 = 180 * 24 * 60 * 60;
const DAY: u64 = 24 * 60 * 60;
/// How often the agent checks the log's size and whether a trim by age is due. Tokio's timers
/// stop while a Mac sleeps, so it checks the wall clock every hour instead of waiting 24 hours.
pub(crate) const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
/// A log longer than this is cut down to the whole lines in its last `KEEP` bytes.
const MAX_SIZE: u64 = 10 * 1024 * 1024;
const KEEP: u64 = 5 * 1024 * 1024;
/// Longer text is cut to this many characters after escaping.
pub(crate) const MAX_CHARS: usize = 300;

/// Which rate limit a log line counts against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Lines that a remote nobody authorized can cause: rejected peers and failed incoming
    /// handshakes. Up to 30 at once, then one every 10 seconds.
    Unauthenticated,
    /// Every other line. Up to 60 at once, then one every second.
    Other,
}

/// A token bucket that counts the lines it turns away.
#[derive(Debug)]
struct Bucket {
    burst: u32,
    every: Duration,
    tokens: u32,
    filled: Instant,
    suppressed: u64,
}

impl Bucket {
    fn new(burst: u32, every: Duration, now: Instant) -> Self {
        Self {
            burst,
            every,
            tokens: burst,
            filled: now,
            suppressed: 0,
        }
    }

    /// Takes a token at `now`: how many lines were suppressed since the last one written, or
    /// `None` when the bucket is empty and this line is suppressed too.
    fn admit(&mut self, now: Instant) -> Option<u64> {
        let earned = now.saturating_duration_since(self.filled).as_nanos() / self.every.as_nanos();
        if earned > 0 {
            if u128::from(self.tokens) + earned >= u128::from(self.burst) {
                self.tokens = self.burst;
                self.filled = now;
            } else {
                // Less than burst, so it fits in a u32.
                self.tokens += earned as u32;
                self.filled += self.every * earned as u32;
            }
        }
        if self.tokens == 0 {
            self.suppressed += 1;
            return None;
        }
        self.tokens -= 1;
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Separate buckets, so that unauthenticated remotes cannot crowd out the other lines.
#[derive(Debug)]
struct Limiter {
    unauthenticated: Bucket,
    other: Bucket,
}

impl Limiter {
    fn new(now: Instant) -> Self {
        Self {
            unauthenticated: Bucket::new(30, Duration::from_secs(10), now),
            other: Bucket::new(60, Duration::from_secs(1), now),
        }
    }

    fn admit(&mut self, kind: Kind, now: Instant) -> Option<u64> {
        match kind {
            Kind::Unauthenticated => self.unauthenticated.admit(now),
            Kind::Other => self.other.admit(now),
        }
    }
}

static LIMITER: LazyLock<Mutex<Limiter>> =
    LazyLock::new(|| Mutex::new(Limiter::new(Instant::now())));

/// The line that reports `count` suppressed lines of `kind`.
fn summary(kind: Kind, count: u64) -> String {
    let lines = if count == 1 { "line" } else { "lines" };
    match kind {
        Kind::Unauthenticated => {
            format!("suppressed {count} log {lines} about unauthenticated connections")
        }
        Kind::Other => format!("suppressed {count} other log {lines}"),
    }
}

/// `text` with control characters such as newlines and ESC, the Unicode line and paragraph
/// separators and the bidirectional controls escaped, cut to 300 characters with the original
/// length in bytes noted. Text that a remote chose cannot fake or flood lines, or reorder what a
/// log viewer shows.
pub(crate) fn sanitize(text: &str) -> String {
    let mut clean = String::new();
    for c in text.chars() {
        if c.is_control()
            || matches!(
                c,
                '\u{2028}'
                    | '\u{2029}'
                    | '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            clean.extend(c.escape_default());
        } else {
            clean.push(c);
        }
    }
    if let Some((end, _)) = clean.char_indices().nth(MAX_CHARS) {
        clean.truncate(end);
        clean.push_str(&format!("... [truncated, {} bytes]", text.len()));
    }
    clean
}

/// Writes `<UTC time> herdvia: <text>` to standard error as one line, unless `kind` is over its
/// rate limit. The first line after suppressed ones is preceded by a line that counts them.
pub(crate) fn line(kind: Kind, text: &str) {
    // The limiter's lock is released before standard error is locked: a trim holds standard
    // error while it rewrites the log.
    let admitted = LIMITER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .admit(kind, Instant::now());
    let Some(suppressed) = admitted else {
        return;
    };
    write_line(&mut io::stderr().lock(), kind, suppressed, text);
}

/// Writes the line, preceded by the summary of `suppressed` lines if there are any, with one
/// write. A failed write is ignored: a full disk or a closed log must not stop a session.
fn write_line(out: &mut impl Write, kind: Kind, suppressed: u64, text: &str) {
    let stamp = stamp();
    let mut lines = String::new();
    if suppressed > 0 {
        lines.push_str(&format!("{stamp} herdvia: {}\n", summary(kind, suppressed)));
    }
    lines.push_str(&format!("{stamp} herdvia: {}\n", sanitize(text)));
    let _ = out.write_all(lines.as_bytes());
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The current UTC time for the start of a log line, such as `2026-11-14T14:17:03Z`.
fn stamp() -> String {
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

/// Cuts `file` down in place to the whole lines in its last `keep` bytes when it is longer than
/// `max`, reading only those bytes. Leaves the file alone otherwise.
fn shrink(file: &mut File, max: u64, keep: u64) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len <= max {
        return Ok(());
    }
    // One byte more shows whether the kept bytes start a line; a partial first line is dropped.
    file.seek(SeekFrom::Start(len.saturating_sub(keep + 1)))?;
    let mut data = Vec::new();
    (&*file).take(keep + 1).read_to_end(&mut data)?;
    let cut = data
        .iter()
        .position(|&b| b == b'\n')
        .map_or(data.len(), |i| i + 1);
    file.rewind()?;
    file.write_all(&data[cut..])?;
    file.set_len((data.len() - cut) as u64)
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

/// Whether standard error writes to `file`, as it does under `agent --log`. A link or other
/// reparse point at the log's name is not.
#[cfg(windows)]
fn is_stderr(file: &File) -> io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(false);
    }
    same_file(io::stderr(), file)
}

/// Whether `handle` and `file` are open on the same file. A console, a pipe, or no handle
/// at all has no file index and is not.
#[cfg(windows)]
fn same_file(handle: impl std::os::windows::io::AsHandle, file: &File) -> io::Result<bool> {
    Ok(crate::ipc::file_id(handle).ok() == Some(crate::ipc::file_id(file)?))
}

/// Opens the log for reading and writing; on Windows the entry at its name itself, not the
/// target of a link.
fn open_log(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(
        &mut options,
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    );
    options.open(path)
}

fn trim(path: &Path, now: u64, expire: bool) -> io::Result<()> {
    // Hold standard error so that no log line of this process lands mid-rewrite and is lost.
    let _stderr = io::stderr().lock();
    let mut file = match open_log(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    if !is_stderr(&file)? {
        return Ok(());
    }
    // Cutting the size first bounds what the trim by age reads.
    shrink(&mut file, MAX_SIZE, KEEP)?;
    if expire {
        file.rewind()?;
        rewrite(&mut file, now.saturating_sub(MAX_AGE))?;
    }
    Ok(())
}

/// Whether a trim is due at `now`, 24 hours of wall-clock time after the `last` one.
fn due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| now.abs_diff(last) >= DAY)
}

/// Cuts the log at `path` down to its last 5 MiB if it is over 10 MiB. Then drops lines older
/// than 180 days if 24 hours of wall-clock time have passed since the `last` such trim, or if
/// there has been none. A failed trim is tried again at the next check.
pub(crate) async fn trim_if_due(path: &Path, last: &mut Option<u64>) {
    let now = unix_now();
    let expire = due(*last, now);
    let file = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || trim(&file, now, expire))
        .await
        .unwrap_or_else(|err| Err(io::Error::other(err)));
    match result {
        Ok(()) if expire => *last = Some(now),
        Ok(()) => {}
        Err(err) => line(
            Kind::Other,
            &format!("cannot remove old lines from {}: {err}", path.display()),
        ),
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

    #[test]
    fn sanitize_escapes_control_characters_into_one_line() {
        assert_eq!(sanitize("plain text, ünïcode"), "plain text, ünïcode");
        assert_eq!(
            sanitize("a\nb\r\n2026-11-14T14:17:03Z herdvia: fake\u{1b}[31m\0\u{7f}\u{85}"),
            r"a\nb\r\n2026-11-14T14:17:03Z herdvia: fake\u{1b}[31m\u{0}\u{7f}\u{85}"
        );
        assert!(!sanitize(&"\n\u{1b}".repeat(500)).contains(['\n', '\u{1b}']));
        assert_eq!(sanitize("a\u{2028}b\u{2029}c"), r"a\u{2028}b\u{2029}c");
        let bidi = [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        assert_eq!(
            sanitize(&String::from_iter(bidi)),
            r"\u{61c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}"
        );
        // Neighbours of the escaped ranges stay.
        assert_eq!(
            sanitize("\u{061b}\u{061d}\u{200d}\u{2027}\u{202f}\u{2065}\u{206a}"),
            "\u{061b}\u{061d}\u{200d}\u{2027}\u{202f}\u{2065}\u{206a}"
        );
    }

    #[test]
    fn sanitize_cuts_after_300_characters_and_notes_the_original_bytes() {
        let exact = "é".repeat(MAX_CHARS);
        assert_eq!(sanitize(&exact), exact);
        let long = format!("{exact}x");
        assert_eq!(
            sanitize(&long),
            format!("{exact}... [truncated, {} bytes]", long.len())
        );
        // Escapes count toward the limit.
        let escaped = sanitize(&"\n".repeat(200));
        assert_eq!(
            escaped,
            format!("{}... [truncated, 200 bytes]", r"\n".repeat(150))
        );
    }

    fn admitted(bucket: &mut Bucket, now: Instant, count: usize) -> Vec<Option<u64>> {
        (0..count).map(|_| bucket.admit(now)).collect()
    }

    #[test]
    fn a_bucket_allows_a_burst_then_refills_one_per_period() {
        let start = Instant::now();
        let mut bucket = Bucket::new(3, Duration::from_secs(10), start);
        assert_eq!(admitted(&mut bucket, start, 3), [Some(0); 3]);
        assert_eq!(admitted(&mut bucket, start, 2), [None; 2]);
        assert_eq!(bucket.admit(start + Duration::from_secs(9)), None);
        // The next line written reports the three suppressed ones, and the count starts over.
        assert_eq!(bucket.admit(start + Duration::from_secs(10)), Some(3));
        assert_eq!(bucket.admit(start + Duration::from_secs(19)), None);
        assert_eq!(bucket.admit(start + Duration::from_secs(20)), Some(1));
        // Two periods earn two lines, and the part of a period already waited counts.
        let later = start + Duration::from_secs(45);
        assert_eq!(admitted(&mut bucket, later, 3), [Some(0), Some(0), None]);
        assert_eq!(bucket.admit(start + Duration::from_secs(50)), Some(1));
    }

    #[test]
    fn a_bucket_holds_no_more_than_its_burst() {
        let start = Instant::now();
        let mut bucket = Bucket::new(3, Duration::from_secs(1), start);
        let later = start + Duration::from_secs(3600);
        assert_eq!(
            admitted(&mut bucket, later, 4),
            [Some(0), Some(0), Some(0), None]
        );
    }

    #[test]
    fn unauthenticated_lines_cannot_crowd_out_the_others() {
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        let unauthenticated: Vec<_> = (0..100)
            .map(|_| limiter.admit(Kind::Unauthenticated, start))
            .collect();
        assert_eq!(unauthenticated.iter().flatten().count(), 30);
        let other: Vec<_> = (0..100)
            .map(|_| limiter.admit(Kind::Other, start))
            .collect();
        assert_eq!(other.iter().flatten().count(), 60);
        assert_eq!(
            limiter.admit(Kind::Other, start + Duration::from_secs(1)),
            Some(40)
        );
        assert_eq!(
            limiter.admit(Kind::Unauthenticated, start + Duration::from_secs(9)),
            None
        );
        assert_eq!(
            limiter.admit(Kind::Unauthenticated, start + Duration::from_secs(10)),
            Some(71)
        );
    }

    #[test]
    fn summaries_name_the_kind_and_count() {
        assert_eq!(
            summary(Kind::Unauthenticated, 71),
            "suppressed 71 log lines about unauthenticated connections"
        );
        assert_eq!(summary(Kind::Other, 1), "suppressed 1 other log line");
    }

    /// Records each write it is given, or fails every write.
    struct Writes {
        calls: Vec<Vec<u8>>,
        fail: bool,
    }

    impl Write for Writes {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.calls.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_the_summary_and_the_line_with_one_write() {
        let mut out = Writes {
            calls: Vec::new(),
            fail: false,
        };
        write_line(&mut out, Kind::Other, 2, "a\nb");
        let [call] = &out.calls[..] else {
            panic!("{} writes", out.calls.len());
        };
        let text = String::from_utf8(call.clone()).unwrap();
        let lines: Vec<_> = text.lines().map(|line| &line[21..]).collect();
        assert_eq!(
            lines,
            ["herdvia: suppressed 2 other log lines", r"herdvia: a\nb"]
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn a_failed_write_is_ignored() {
        let mut out = Writes {
            calls: Vec::new(),
            fail: true,
        };
        write_line(&mut out, Kind::Other, 1, "session ended");
    }

    fn numbered(count: usize) -> String {
        (0..count).map(|i| format!("line {i:03}\n")).collect()
    }

    fn read_write(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn does_not_write_a_log_within_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let data = numbered(10);
        fs::write(&path, &data).unwrap();
        // A read-only handle fails any write.
        shrink(&mut File::open(&path).unwrap(), data.len() as u64, 9).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), data);
    }

    #[test]
    fn shrinks_an_oversized_log_in_place_at_a_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        // 25 lines of 9 bytes.
        let data = numbered(25);
        fs::write(&path, &data).unwrap();
        let mut append = OpenOptions::new().append(true).open(&path).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };
        // The last 50 bytes start inside line 19, which goes.
        shrink(&mut read_write(&path), 100, 50).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), &data[9 * 20..]);
        append.write_all(b"after\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}after\n", &data[9 * 20..])
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
    }

    #[test]
    fn keeps_a_line_that_starts_exactly_at_the_kept_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let data = numbered(25);
        fs::write(&path, &data).unwrap();
        shrink(&mut read_write(&path), 100, 45).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), &data[9 * 20..]);
        // Without a line boundary in the kept bytes nothing is left.
        fs::write(&path, "x".repeat(200)).unwrap();
        shrink(&mut read_write(&path), 100, 50).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    /// Reading all of this sparse file would need 64 GiB of memory.
    #[cfg(unix)]
    #[test]
    fn reads_only_the_end_of_a_huge_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        fs::write(&path, b"").unwrap();
        let mut file = read_write(&path);
        file.set_len(64 << 30).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        let tail = format!("\n{}", numbered(3));
        file.write_all(tail.as_bytes()).unwrap();
        shrink(&mut file, MAX_SIZE, KEEP).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), numbered(3));
    }

    #[test]
    fn leaves_a_file_that_is_not_standard_error_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let data = at(1, "old");
        fs::write(&path, &data).unwrap();
        trim(&path, CUTOFF, true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), data);
        trim(&dir.path().join("missing.log"), CUTOFF, true).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn compares_open_files_by_volume_and_file_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdvia.log");
        let other = dir.path().join("other.log");
        fs::write(&path, b"").unwrap();
        fs::write(&other, b"").unwrap();
        // As agent --log opens standard error, and as trim opens the log.
        let append = OpenOptions::new().append(true).open(&path).unwrap();
        let log = open_log(&path).unwrap();
        assert!(same_file(&append, &log).unwrap());
        assert!(!same_file(&append, &open_log(&other).unwrap()).unwrap());
        // Nor is a pipe, as standard error may be.
        let (reader, _writer) = io::pipe().unwrap();
        assert!(!same_file(&reader, &log).unwrap());
    }

    #[tokio::test]
    async fn a_failed_trim_by_age_is_tried_again_at_the_next_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut last = None;
        // A directory cannot be opened for writing.
        trim_if_due(dir.path(), &mut last).await;
        assert_eq!(last, None);
        // A missing log needs no trim, which counts as done.
        trim_if_due(&dir.path().join("herdvia.log"), &mut last).await;
        let done = last.unwrap();
        assert!(done.abs_diff(unix_now()) < 60);
        trim_if_due(dir.path(), &mut last).await;
        assert_eq!(last, Some(done));
    }
}
