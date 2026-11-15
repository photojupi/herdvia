//! The agent's `status-detail` reply and its wording, shared by the agent, `herdvia status`, and
//! `doctor`.
//!
//! The reply is a version line, [`HEADER`], then one line per peer:
//! `<name> <in|out|both> <state> <path> <rtt> <reason-kind> <retry-secs> <reason-text…>`.
//! The first seven fields hold no spaces; the reason text is the rest of the line and may be empty
//! (then the line ends after `<retry-secs>`). The plain `status` reply stays the 5-column
//! [`table`], so an old `doctor` keeps working with a new agent.

/// The first line of a `status-detail` reply. A reply with another first line cannot be parsed,
/// and callers fall back to the plain `status` reply.
pub const HEADER: &str = "herdvia-status-detail 1";

/// Whether a peer is connected. `Waiting` is an inbound-only peer that is not connected: this PC
/// never dials it, so there is no failure to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connected,
    Disconnected,
    Waiting,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Waiting => "waiting",
        }
    }
}

/// Why the last outbound connection failed or ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The peer closed the connection with code 403.
    Rejected,
    /// Dialing failed (timed out or another dial error).
    Unreachable,
    /// An established connection ended for another reason.
    Closed,
    /// A kind this version does not know, from a newer agent: shown without a hint.
    Unknown(String),
}

impl Kind {
    fn as_str(&self) -> &str {
        match self {
            Self::Rejected => "rejected",
            Self::Unreachable => "unreachable",
            Self::Closed => "closed",
            Self::Unknown(kind) => kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub kind: Kind,
    /// Already sanitized: one line with no control characters.
    pub text: String,
}

impl Reason {
    /// `text` may come from the remote, so it is sanitized here, before it is stored.
    pub fn new(kind: Kind, text: &str) -> Self {
        Self {
            kind,
            text: crate::log::sanitize(text),
        }
    }
}

/// One peer in a `status-detail` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// `in`, `out`, or `both`.
    pub permission: String,
    pub state: State,
    /// `direct`, `relay`, `unknown`, or `-` when not connected.
    pub path: String,
    /// For example `1.25ms`, or `-`.
    pub rtt: String,
    pub reason: Option<Reason>,
    /// Whole seconds until the outbound worker dials again, if it is waiting to.
    pub retry_secs: Option<u64>,
}

/// The full `status-detail` reply.
pub fn format(rows: &[Row]) -> String {
    let mut text = format!("{HEADER}\n");
    for row in rows {
        text.push_str(&format!(
            "{} {} {} {} {} {} {}",
            row.name,
            row.permission,
            row.state.as_str(),
            row.path,
            row.rtt,
            row.reason.as_ref().map_or("-", |r| r.kind.as_str()),
            row.retry_secs.map_or("-".to_owned(), |s| s.to_string()),
        ));
        if let Some(reason) = row.reason.as_ref().filter(|r| !r.text.is_empty()) {
            text.push(' ');
            text.push_str(&reason.text);
        }
        text.push('\n');
    }
    text
}

/// Parses a `status-detail` reply. `None` for another header version or a line it cannot read.
pub fn parse(reply: &str) -> Option<Vec<Row>> {
    let mut lines = reply.lines();
    if lines.next()? != HEADER {
        return None;
    }
    lines
        .map(|line| {
            let mut fields = line.splitn(8, ' ');
            let mut next = || fields.next().filter(|f| !f.is_empty());
            let (name, permission, state, path, rtt, kind, retry) = (
                next()?,
                next()?,
                next()?,
                next()?,
                next()?,
                next()?,
                next()?,
            );
            let text = fields.next().unwrap_or("");
            let state = match state {
                "connected" => State::Connected,
                "disconnected" => State::Disconnected,
                "waiting" => State::Waiting,
                _ => return None,
            };
            let kind = match kind {
                "-" => None,
                "rejected" => Some(Kind::Rejected),
                "unreachable" => Some(Kind::Unreachable),
                "closed" => Some(Kind::Closed),
                other => Some(Kind::Unknown(other.to_owned())),
            };
            Some(Row {
                name: name.to_owned(),
                permission: permission.to_owned(),
                state,
                path: path.to_owned(),
                rtt: rtt.to_owned(),
                reason: kind.map(|kind| Reason {
                    kind,
                    text: text.to_owned(),
                }),
                retry_secs: match retry {
                    "-" => None,
                    secs => Some(secs.parse().ok()?),
                },
            })
        })
        .collect()
}

/// The plain `status` reply's 5-column table: a waiting peer shows as `disconnected`.
pub fn table(rows: &[Row]) -> String {
    let mut text = String::from("name permission connection path rtt\n");
    for row in rows {
        let state = match row.state {
            State::Connected => "connected",
            State::Disconnected | State::Waiting => "disconnected",
        };
        text.push_str(&format!(
            "{} {} {state} {} {}\n",
            row.name, row.permission, row.path, row.rtt
        ));
    }
    text
}

/// `prefix (text)`, or `prefix` alone when there is no text.
fn with_text(prefix: &str, text: &str) -> String {
    if text.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} ({text})")
    }
}

/// The reason without a hint, for example `rejected by the peer (403 peer not allowed)`. This is
/// what the `via` error adds after `peer did not connect within 5 seconds: `.
pub fn short_reason(reason: &Reason) -> String {
    match &reason.kind {
        Kind::Rejected => with_text("rejected by the peer", &reason.text),
        Kind::Unreachable => with_text("could not reach it", &reason.text),
        Kind::Closed => with_text("the connection closed", &reason.text),
        Kind::Unknown(_) => with_text("not connected", &reason.text),
    }
}

/// One line for a peer that is not connected, `None` for a connected one. It states only what the
/// agent saw: no record means "not connected yet", never a guessed cause.
pub fn human(row: &Row) -> Option<String> {
    let name = &row.name;
    let mut line = match (row.state, &row.reason) {
        (State::Connected, _) => return None,
        (State::Waiting, _) => {
            format!("{name}: waiting for {name} to connect (this PC only accepts it)")
        }
        (State::Disconnected, None) => format!("{name}: not connected yet"),
        (State::Disconnected, Some(reason)) => {
            let mut line = format!("{name}: {}", short_reason(reason));
            match (&reason.kind, reason.text.as_str()) {
                (Kind::Rejected, "403 peer not allowed") => line.push_str(&format!(
                    "; on {name} run herdvia pending, then herdvia accept"
                )),
                (Kind::Rejected, "403 inbound permission required") => line.push_str(&format!(
                    "; on {name} allow this PC with --inbound or --both (herdvia pending, then herdvia accept)"
                )),
                (Kind::Rejected, "403 peer permissions changed") => line.push_str(&format!(
                    "; {name} changed this PC's permissions (it may have revoked this PC); check herdvia peers on {name}"
                )),
                (Kind::Unreachable, _) => line.push_str("; its agent may be off or offline"),
                _ => {}
            }
            line
        }
    };
    line.push('.');
    if let Some(secs) = row.retry_secs {
        line.push_str(&format!(" Retrying in {secs}s."));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, state: State, reason: Option<Reason>, retry_secs: Option<u64>) -> Row {
        Row {
            name: name.into(),
            permission: "out".into(),
            state,
            path: "-".into(),
            rtt: "-".into(),
            reason,
            retry_secs,
        }
    }

    #[test]
    fn format_and_parse_round_trip() {
        let rows = vec![
            Row {
                name: "home".into(),
                permission: "both".into(),
                state: State::Connected,
                path: "direct".into(),
                rtt: "1.25ms".into(),
                reason: None,
                retry_secs: None,
            },
            row(
                "win1",
                State::Disconnected,
                Some(Reason::new(Kind::Rejected, "403 peer not allowed")),
                Some(12),
            ),
            // A remote-chosen reason that tries to fake a second row and colour the terminal.
            row(
                "win2",
                State::Disconnected,
                Some(Reason::new(
                    Kind::Rejected,
                    "403 x\nhome out connected direct 1ms - -\u{1b}[31m  ",
                )),
                None,
            ),
            row(
                "laptop",
                State::Disconnected,
                Some(Reason::new(Kind::Closed, "")),
                Some(0),
            ),
            row("tab", State::Waiting, None, None),
        ];
        let text = format(&rows);
        assert_eq!(text.lines().count(), rows.len() + 1, "{text}");
        assert!(!text.contains('\u{1b}'));
        assert!(
            text.starts_with("herdvia-status-detail 1\nhome both connected direct 1.25ms - -\n")
        );
        assert!(text.contains("\nwin1 out disconnected - - rejected 12 403 peer not allowed\n"));
        assert!(text.contains("\nlaptop out disconnected - - closed 0\n"));
        assert_eq!(parse(&text), Some(rows));
    }

    #[test]
    fn parse_rejects_another_version_and_unreadable_lines() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("herdvia-status-detail 2\n"), None);
        assert_eq!(parse("name permission connection path rtt\n"), None);
        assert_eq!(parse("herdvia-status-detail 1\n"), Some(vec![]));
        for line in [
            "a out disconnected - - -",
            "a out sleeping - - - -",
            "a out disconnected - - - soon",
            "a out disconnected  - - - -",
        ] {
            assert_eq!(parse(&format!("{HEADER}\n{line}\n")), None, "{line}");
        }
    }

    #[test]
    fn parse_keeps_unknown_reason_kinds_and_shows_them_without_a_hint() {
        let rows = parse(&format!(
            "{HEADER}\na out disconnected - - novel 3 some text\n"
        ))
        .unwrap();
        let reason = rows[0].reason.as_ref().unwrap();
        assert_eq!(reason.kind, Kind::Unknown("novel".into()));
        assert_eq!(reason.text, "some text");
        assert_eq!(
            human(&rows[0]).unwrap(),
            "a: not connected (some text). Retrying in 3s."
        );
    }

    #[test]
    fn table_matches_the_plain_status_reply() {
        let rows = vec![
            Row {
                name: "home".into(),
                permission: "both".into(),
                state: State::Connected,
                path: "relay".into(),
                rtt: "12.50ms".into(),
                reason: None,
                retry_secs: None,
            },
            row(
                "win1",
                State::Disconnected,
                Some(Reason::new(Kind::Unreachable, "timed out")),
                Some(4),
            ),
            row("tab", State::Waiting, None, None),
        ];
        assert_eq!(
            table(&rows),
            "name permission connection path rtt\nhome both connected relay 12.50ms\nwin1 out disconnected - -\ntab out disconnected - -\n"
        );
        assert_eq!(table(&[]), "name permission connection path rtt\n");
    }

    #[test]
    fn human_lines_say_what_is_known_and_hint_at_known_rejections() {
        let rejected = |text: &str| {
            human(&row(
                "win1",
                State::Disconnected,
                Some(Reason::new(Kind::Rejected, text)),
                Some(12),
            ))
            .unwrap()
        };
        assert_eq!(
            rejected("403 peer not allowed"),
            "win1: rejected by the peer (403 peer not allowed); on win1 run herdvia pending, then herdvia accept. Retrying in 12s."
        );
        assert_eq!(
            rejected("403 inbound permission required"),
            "win1: rejected by the peer (403 inbound permission required); on win1 allow this PC with --inbound or --both (herdvia pending, then herdvia accept). Retrying in 12s."
        );
        assert_eq!(
            rejected("403 peer permissions changed"),
            "win1: rejected by the peer (403 peer permissions changed); win1 changed this PC's permissions (it may have revoked this PC); check herdvia peers on win1. Retrying in 12s."
        );
        assert_eq!(
            rejected("403 go away"),
            "win1: rejected by the peer (403 go away). Retrying in 12s."
        );
        assert_eq!(
            human(&row(
                "home",
                State::Disconnected,
                Some(Reason::new(Kind::Unreachable, "timed out")),
                Some(18),
            ))
            .unwrap(),
            "home: could not reach it (timed out); its agent may be off or offline. Retrying in 18s."
        );
        assert_eq!(
            human(&row(
                "home",
                State::Disconnected,
                Some(Reason::new(Kind::Closed, "reset by peer")),
                None,
            ))
            .unwrap(),
            "home: the connection closed (reset by peer)."
        );
        assert_eq!(
            human(&row("laptop", State::Waiting, None, None)).unwrap(),
            "laptop: waiting for laptop to connect (this PC only accepts it)."
        );
        assert_eq!(
            human(&row("new", State::Disconnected, None, Some(1))).unwrap(),
            "new: not connected yet. Retrying in 1s."
        );
        assert_eq!(human(&row("up", State::Connected, None, None)), None);
    }

    #[test]
    fn short_reason_has_no_hint_and_skips_empty_text() {
        assert_eq!(
            short_reason(&Reason::new(Kind::Rejected, "403 peer not allowed")),
            "rejected by the peer (403 peer not allowed)"
        );
        assert_eq!(
            short_reason(&Reason::new(Kind::Closed, "")),
            "the connection closed"
        );
    }
}
