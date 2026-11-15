use crate::{
    control,
    discover::{self, BeaconInfo},
    frame::{self, CONTROL, ERROR, EXIT, STDIN, STDOUT},
    ipc,
    pair::{
        codec::decode_invite,
        hc::{self, HcResult},
    },
    peers,
};
use data_encoding::HEXLOWER;
use iroh::EndpointId;
use std::{
    io::{self, BufRead, Write},
    net::SocketAddr,
    path::Path,
    time::Duration,
};

fn parse_yes_no(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")
}

#[derive(Debug, PartialEq, Eq)]
enum PeerChoice {
    Peer(usize),
    Invite,
    /// A blank line, as Enter alone gives.
    Cancel,
}

fn parse_peer_selection(input: &str, count: usize) -> Option<PeerChoice> {
    if input.trim().is_empty() {
        return Some(PeerChoice::Cancel);
    }
    let n: usize = input.trim().parse().ok()?;
    if n == 0 {
        Some(PeerChoice::Invite)
    } else if n <= count {
        Some(PeerChoice::Peer(n - 1))
    } else {
        None
    }
}

fn peer_is_connected(status: &str, name: &str) -> bool {
    status.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(name) && fields.nth(1) == Some("connected")
    })
}

async fn ensure_agent_running(dir: &Path) -> io::Result<()> {
    match tokio::time::timeout(Duration::from_secs(3), control::request(dir, "status")).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err))
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(io::Error::new(
                err.kind(),
                "agent is not running; start it with 'herdvia agent install' (or 'herdvia agent' in a terminal)",
            ))
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the local agent did not respond within 3 seconds; restart it with 'herdvia agent install' (or restart 'herdvia agent')",
        )),
    }
}

async fn check_older_agent(dir: &Path, fallback_msg: &'static str) -> io::Error {
    if tokio::time::timeout(Duration::from_secs(2), control::request(dir, "status"))
        .await
        .is_ok_and(|r| r.is_ok())
    {
        io::Error::other(
            "the running agent is older than this herdvia executable; restart it with 'herdvia agent install' (or restart 'herdvia agent')",
        )
    } else {
        frame::invalid(fallback_msg)
    }
}

async fn read_stdin_line() -> io::Result<Option<String>> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let bytes = io::stdin().lock().read_line(&mut line)?;
        if bytes == 0 { Ok(None) } else { Ok(Some(line)) }
    })
    .await
    .map_err(io::Error::other)?
}

fn decode_hex_16(s: &str) -> io::Result<[u8; 16]> {
    if s.len() != 32 {
        return Err(frame::invalid("invalid listen_id length"));
    }
    let bytes = HEXLOWER
        .decode(s.to_ascii_lowercase().as_bytes())
        .map_err(|_| frame::invalid("invalid listen_id hex"))?;
    bytes
        .try_into()
        .map_err(|_| frame::invalid("invalid listen_id"))
}

pub async fn run_listen(explicit_name: Option<String>) -> io::Result<i32> {
    let dir = ipc::data_dir()?;
    ensure_agent_running(&dir).await?;

    if let Some(warn) = crate::doctor::core_preflight_warning().await {
        let _ = writeln!(io::stderr(), "herdvia: warning: {warn}");
    }

    // Split, so that a frame read can go on while the answer to the prompt is written.
    let (mut reader, mut writer) = tokio::io::split(ipc::connect(&dir).await?);
    if frame::write(&mut writer, CONTROL, b"listen").await.is_err()
        || frame::write(
            &mut writer,
            STDIN,
            explicit_name.as_deref().unwrap_or("").as_bytes(),
        )
        .await
        .is_err()
    {
        return Err(check_older_agent(&dir, "failed to start listen session").await);
    }

    let first = match frame::read(&mut reader).await {
        Ok(Some(f)) => f,
        Ok(None) | Err(_) => {
            return Err(check_older_agent(&dir, "agent closed listen session unexpectedly").await);
        }
    };

    if first.tag == ERROR {
        return Err(io::Error::other(
            String::from_utf8_lossy(&first.payload).into_owned(),
        ));
    }
    if first.tag != STDOUT {
        return Err(frame::invalid("unexpected response from agent"));
    }

    let ready_text = String::from_utf8_lossy(&first.payload);
    let parts: Vec<_> = ready_text.split_whitespace().collect();
    if parts.len() != 6 || parts[0] != "READY" {
        return Err(frame::invalid("malformed READY line from agent"));
    }
    let endpoint_id: EndpointId = parts[1]
        .parse()
        .map_err(|_| frame::invalid("invalid EndpointId from agent"))?;
    let quic_port: u16 = parts[2]
        .parse()
        .map_err(|_| frame::invalid("invalid QUIC port from agent"))?;
    let listen_id = decode_hex_16(parts[3])?;
    let invite_token = parts[4];
    let hostname = parts[5].to_owned();

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let adv_handle = match discover::bind_discovery_socket(discover::DEFAULT_PORT) {
        Ok(sock) => Some(tokio::spawn(discover::advertise_on(
            sock,
            discover::default_targets(discover::DEFAULT_PORT),
            BeaconInfo {
                listen_id,
                endpoint_id,
                quic_port,
                hostname,
            },
            stop_rx,
        ))),
        Err(err) => {
            let _ = writeln!(
                io::stderr(),
                "herdvia: note: LAN auto-discovery unavailable ({err}); use the invite token below"
            );
            None
        }
    };

    let _ = writeln!(
        io::stdout(),
        "Waiting for a connection (expires in 5 minutes; press Ctrl-C to cancel)...\nThis PC's fingerprint: {}\n\n  Same local network: run on the Herdr PC:\n    herdvia connect <name>\n  and choose the entry with this fingerprint.\n\n  Different network:  run on the Herdr PC:\n    herdvia connect <name> {invite_token}\n",
        peers::fingerprint(endpoint_id)
    );
    let _ = io::stdout().flush();

    let next = frame::read(&mut reader)
        .await?
        .ok_or_else(|| frame::invalid("agent closed listen session"))?;
    let _ = stop_tx.send(true);
    if let Some(h) = adv_handle {
        let _ = h.await;
    }

    match next.tag {
        ERROR => Err(io::Error::other(
            String::from_utf8_lossy(&next.payload).into_owned(),
        )),
        STDOUT => {
            let line = String::from_utf8_lossy(&next.payload);
            if line.trim() == "TIMEOUT" {
                let _ = frame::read(&mut reader).await;
                let _ = writeln!(
                    io::stderr(),
                    "herdvia: listen timed out after 5 minutes without a connection"
                );
                return Ok(1);
            }
            let p: Vec<_> = line.split_whitespace().collect();
            if p.len() != 5 || p[0] != "PROMPT" {
                return Err(frame::invalid("unexpected prompt from agent"));
            }
            let (peer_name, caller_fp, sas) = (p[1], p[3], p[4]);
            let _ = write!(
                io::stdout(),
                "Incoming pairing request:\n  Peer name:         {peer_name}\n  Peer fingerprint:  {caller_fp}\n  Verification code: {sas}\n\nCheck that the other PC shows {sas}. Approving lets that PC run commands as this user (--inbound).\nAllow? [y/N]: "
            );
            let _ = io::stdout().flush();

            // The frame read is kept after the answer, not started again: it may have read
            // part of a frame already.
            let outcome = {
                let read = frame::read(&mut reader);
                tokio::pin!(read);
                tokio::select! {
                    input = read_stdin_line() => {
                        let approved = input?.as_deref().is_some_and(parse_yes_no);
                        let decision = if approved { b"approve".as_slice() } else { b"reject".as_slice() };
                        frame::write(&mut writer, STDIN, decision).await?;
                        read.await?
                    }
                    agent_early = &mut read => agent_early?,
                }
            };
            handle_listen_outcome(&mut reader, outcome).await
        }
        _ => Err(frame::invalid("unexpected frame during listen")),
    }
}

async fn handle_listen_outcome<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    outcome: Option<frame::Frame>,
) -> io::Result<i32> {
    let f = outcome.ok_or_else(|| frame::invalid("agent closed before completion"))?;
    match f.tag {
        ERROR => Err(io::Error::other(
            String::from_utf8_lossy(&f.payload).into_owned(),
        )),
        STDOUT => {
            let text = String::from_utf8_lossy(&f.payload);
            let _ = frame::read(stream).await;
            if let Some(rest) = text.strip_prefix("APPROVED ") {
                let mut parts = rest.split_whitespace();
                let saved_name = parts.next().unwrap_or("peer");
                let perm = parts.nth(1).unwrap_or("in");
                let _ = writeln!(io::stdout(), "Paired with {saved_name} ({perm}).");
                Ok(0)
            } else if text.trim() == "TIMEOUT" {
                let _ = writeln!(io::stderr(), "herdvia: listen timed out after 5 minutes");
                Ok(1)
            } else {
                let _ = writeln!(io::stderr(), "herdvia: pairing declined");
                Ok(1)
            }
        }
        EXIT => {
            let code = f
                .payload
                .as_slice()
                .try_into()
                .map(i32::from_be_bytes)
                .unwrap_or(1);
            if code != 0 {
                let _ = writeln!(io::stderr(), "herdvia: pairing declined");
            }
            Ok(if code == 0 { 0 } else { 1 })
        }
        _ => Err(frame::invalid("unexpected completion frame")),
    }
}

pub async fn run_connect(name: String, invite: Option<String>) -> io::Result<i32> {
    let dir = ipc::data_dir()?;

    let (target_id, mode, token, direct_addr): (EndpointId, &str, [u8; 16], Option<SocketAddr>) =
        if let Some(ref raw_invite) = invite {
            let (id, secret) = decode_invite(raw_invite)?;
            peers::check_connect_target(&dir, &name, Some(id))?;
            ensure_agent_running(&dir).await?;
            (id, "invite", secret, None)
        } else {
            peers::check_connect_target(&dir, &name, None)?;
            ensure_agent_running(&dir).await?;
            let self_id = peers::load_key(&dir)?.public();

            let _ = writeln!(io::stdout(), "Searching local network for a waiting PC...");
            let _ = io::stdout().flush();

            let listen_sock = discover::bind_discovery_socket(discover::DEFAULT_PORT).ok();
            let found = discover::scan_on(
                listen_sock,
                discover::default_targets(discover::DEFAULT_PORT),
                self_id,
                Duration::from_secs(2),
            )
            .await?;

            // Beacons are not authenticated: anyone on the LAN can announce any key. Always let the
            // person pick, by the fingerprint the target PC's listen screen shows.
            let selected_lan = match found.len() {
                0 => None,
                count => {
                    let _ = writeln!(
                        io::stdout(),
                        "Waiting PCs found on the local network.\nChoose the PC whose fingerprint matches the one 'herdvia listen' shows on the target PC:"
                    );
                    for (i, p) in found.iter().enumerate() {
                        let _ = writeln!(
                            io::stdout(),
                            "  {}) {} (fingerprint {})",
                            i + 1,
                            p.info.hostname,
                            peers::fingerprint(p.info.endpoint_id)
                        );
                    }
                    let _ = writeln!(io::stdout(), "  0) Paste an invite token instead");
                    let range = if count == 1 {
                        "1".to_owned()
                    } else {
                        format!("1-{count}")
                    };
                    let _ = write!(io::stdout(), "Select [{range}, or 0]: ");
                    let _ = io::stdout().flush();

                    let Some(line) = read_stdin_line().await? else {
                        let _ = writeln!(io::stderr(), "herdvia: pairing cancelled");
                        return Ok(1);
                    };
                    match parse_peer_selection(&line, count) {
                        Some(PeerChoice::Peer(idx)) => Some(found[idx].clone()),
                        Some(PeerChoice::Invite) => None,
                        Some(PeerChoice::Cancel) => {
                            let _ = writeln!(io::stderr(), "herdvia: pairing cancelled");
                            return Ok(1);
                        }
                        None => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "invalid selection",
                            ));
                        }
                    }
                }
            };

            if let Some(peer) = selected_lan {
                peers::check_connect_target(&dir, &name, Some(peer.info.endpoint_id))?;
                (
                    peer.info.endpoint_id,
                    "lan",
                    peer.info.listen_id,
                    peer.direct_addr(),
                )
            } else {
                if found.is_empty() {
                    let _ = writeln!(io::stdout(), "No waiting PC found on the local network.");
                }
                let _ = write!(io::stdout(), "Paste the invite token from the other PC: ");
                let _ = io::stdout().flush();

                let Some(line) = read_stdin_line().await? else {
                    let _ = writeln!(io::stderr(), "herdvia: pairing cancelled");
                    return Ok(1);
                };
                if line.trim().is_empty() {
                    let _ = writeln!(io::stderr(), "herdvia: pairing cancelled");
                    return Ok(1);
                }
                let (id, secret) = decode_invite(&line)?;
                peers::check_connect_target(&dir, &name, Some(id))?;
                (id, "invite", secret, None)
            }
        };

    // Split, so that a frame read can go on while the answer to the prompt is written.
    let (mut reader, mut writer) = tokio::io::split(ipc::connect(&dir).await?);
    let req = match direct_addr {
        Some(addr) => format!(
            "{name} {target_id} {mode} {} {addr}",
            HEXLOWER.encode(&token)
        ),
        None => format!("{name} {target_id} {mode} {}", HEXLOWER.encode(&token)),
    };
    if frame::write(&mut writer, CONTROL, b"connect")
        .await
        .is_err()
        || frame::write(&mut writer, STDIN, req.as_bytes())
            .await
            .is_err()
    {
        return Err(check_older_agent(&dir, "failed to start connect session").await);
    }

    let mut saw_any = false;
    let mut confirmed = false;
    let mut early = None;
    loop {
        let read = match early.take() {
            Some(read) => read,
            None => frame::read(&mut reader).await,
        };
        let f = match read {
            Ok(Some(f)) => {
                saw_any = true;
                f
            }
            Ok(None) | Err(_) if !saw_any => {
                return Err(
                    check_older_agent(&dir, "agent closed connect session unexpectedly").await,
                );
            }
            Ok(None) => return Err(frame::invalid("agent closed connect session without EXIT")),
            Err(err) => return Err(err),
        };

        match f.tag {
            ERROR => {
                let msg = String::from_utf8_lossy(&f.payload).into_owned();
                // An agent from before the confirmation step fails like this on the confirmation
                // frame, before saving anything.
                if confirmed && msg == "unexpected frame during connect" {
                    return Err(io::Error::other(
                        "the running agent is older than this herdvia executable; restart it with 'herdvia agent install' (or restart 'herdvia agent') and pair again",
                    ));
                }
                return Err(io::Error::other(msg));
            }
            STDOUT => {
                let text = String::from_utf8_lossy(&f.payload);
                if let Some(rest) = text.strip_prefix("SAS ") {
                    let parts: Vec<_> = rest.split_whitespace().collect();
                    if parts.len() != 3 {
                        return Err(frame::invalid("malformed SAS line from agent"));
                    }
                    let (target_name, target_fp, sas) = (parts[0], parts[1], parts[2]);
                    let _ = write!(
                        io::stdout(),
                        "\nConnected to {target_name} (fingerprint {target_fp}).\n  Verification code: {sas}\n\nCheck that the other PC shows {sas}. If it shows a different code, answer n: you may be connected to another PC.\nSame code on both PCs? [y/N]: "
                    );
                    let _ = io::stdout().flush();

                    // The frame read is kept after the answer, not started again: it may have
                    // read part of a frame already.
                    let read = frame::read(&mut reader);
                    tokio::pin!(read);
                    early = Some(tokio::select! {
                        input = read_stdin_line() => {
                            if !input?.as_deref().is_some_and(parse_yes_no) {
                                let _ = frame::write(&mut writer, STDIN, b"reject").await;
                                let _ = writeln!(io::stderr(), "herdvia: pairing cancelled");
                                return Ok(1);
                            }
                            frame::write(&mut writer, STDIN, b"approve").await?;
                            confirmed = true;
                            let _ = writeln!(io::stdout(), "Waiting for approval on the other PC...");
                            let _ = io::stdout().flush();
                            read.await
                        }
                        agent_early = &mut read => agent_early,
                    });
                } else if let Some(rest) = text.strip_prefix("REJECTED") {
                    let _ = frame::read(&mut reader).await;
                    let reason = rest.trim();
                    if reason.is_empty() {
                        let _ = writeln!(io::stderr(), "herdvia: pairing rejected");
                    } else {
                        let _ = writeln!(io::stderr(), "herdvia: pairing rejected: {reason}");
                    }
                    return Ok(1);
                } else if text.starts_with("APPROVED ") {
                    if !confirmed {
                        // Only an agent from before the confirmation step approves on its own.
                        return Err(io::Error::other(format!(
                            "the running agent is older than this herdvia executable and saved '{name}' without your confirmation; run 'herdvia revoke {name}', restart the agent with 'herdvia agent install' (or restart 'herdvia agent'), and pair again"
                        )));
                    }
                    let _ = frame::read(&mut reader).await;
                    break;
                }
            }
            _ => return Err(frame::invalid("unexpected frame during connect")),
        }
    }

    // Wait briefly so the new herdvia/1 outbound session is ready when hc probes the machine.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(status) = control::request(&dir, "status").await
            && peer_is_connected(&status, &name)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let exe = std::env::current_exe()?;
    match hc::register_machine(&exe, &name).await {
        HcResult::Added => {
            let _ = writeln!(
                io::stdout(),
                "Paired with {name} and registered machine '{name}' in Herdr."
            );
            Ok(0)
        }
        HcResult::MissingHc { manual_cmd } => {
            let _ = writeln!(io::stdout(), "Paired with {name} in herdvia.");
            let _ = writeln!(
                io::stderr(),
                "herdvia: warning: 'hc' was not found, so '{name}' was not added to Herdr yet. After installing Herdr, run:\n  {manual_cmd}"
            );
            Ok(1)
        }
        HcResult::Failed { reason, manual_cmd } => {
            let _ = writeln!(io::stdout(), "Paired with {name} in herdvia.");
            let _ = writeln!(
                io::stderr(),
                "herdvia: warning: failed to register '{name}' in Herdr ({reason}). To register manually, run:\n  {manual_cmd}"
            );
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yes_no_accepts_only_y_or_yes_and_rejects_empty_or_other() {
        for no in ["", "\n", "\r\n", " ", "n", "N", "no", "ye", "yep", "1"] {
            assert!(!parse_yes_no(no), "{no:?}");
        }
        for yes in ["y", "Y", "yes", "YES", " Yes \r\n", "  y\n"] {
            assert!(parse_yes_no(yes), "{yes:?}");
        }
    }

    #[test]
    fn parse_peer_selection_handles_single_choice_numbers_and_zero_for_invite() {
        assert_eq!(parse_peer_selection("0\n", 3), Some(PeerChoice::Invite));
        assert_eq!(
            parse_peer_selection(" 1 \r\n", 3),
            Some(PeerChoice::Peer(0))
        );
        assert_eq!(parse_peer_selection("3", 3), Some(PeerChoice::Peer(2)));
        assert_eq!(parse_peer_selection("4", 3), None);
        assert_eq!(parse_peer_selection("abc", 3), None);
        assert_eq!(parse_peer_selection("-1", 3), None);
    }

    #[test]
    fn parse_peer_selection_reads_a_blank_line_as_cancel() {
        for blank in ["", "\n", "\r\n", "  \n", "\t"] {
            assert_eq!(
                parse_peer_selection(blank, 3),
                Some(PeerChoice::Cancel),
                "{blank:?}"
            );
        }
    }
}
