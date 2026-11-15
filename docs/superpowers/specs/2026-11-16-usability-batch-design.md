# herdvia Usability Batch Design (status reasons, register, peers, CLI hints, help)

Date: 2026-11-16. Status: draft for approval.

## Intent and Why

The user runs Herdr across 3–4 of their own PCs (macmini, win1, win2, laptop) over herdvia. Daily friction comes from:

- **Not knowing why a peer is down.** `herdvia status` prints only `disconnected - -`, `doctor` can only say "its agent is off, or it has not allowed this PC", and Herdr shows `herdvia: peer did not connect within 5 seconds` (the user's real `win1` entry shows exactly this today).
- **Partial failures with no cheap way back.** If `connect` paired but `hc machine add` failed, rerunning `connect` is refused ("already registered; revoke it first"), so the only way back is to throw away a good pairing and re-pair.
- **Small CLI gaps** that make the tool feel hostile: no offline way to see the peer list, `allow` silently replacing a peer's key, `revoke` leaving the Herdr machine behind without saying so, a `doctor` key message that contradicts the README, and no `--help`.

- **`revoke` may not reach the running agent, silently.** `revoke` edits the `peers` file first and then asks the agent to reload. If that request fails or hangs, the file is changed but nobody knows whether the running agent still admits the revoked peer, and rerunning `revoke` fails ("no peer named …") before it ever reaches the agent.

This batch fixes those eight items, chosen by the user from the improvement-options review: **A1, B1, C1, B2-b, C2-b, C3-a, D2, B3**.

## Goals

1. **A1 – Reasons for being disconnected.** `status`, `doctor`, and the `via` error that Herdr shows tell apart: rejected by the peer (with the peer's reason), could not reach (timed out / dial error), waiting for the peer to connect (inbound-only), and when the next retry happens. `herdvia agent reload` (goal 8) also serves as "retry every connection now".
2. **B1 – `herdvia register <name>`.** Re-run only the Herdr registration for an already-paired outbound peer, without touching keys or permissions.
3. **C1 – `herdvia peers`.** Show the saved peer list without a running agent.
4. **B2-b – `revoke` follow-up hints.** After a successful revoke, tell the user what is left to clean up.
5. **C2-b – `allow` key-change notice.** When `allow` replaces a peer's EndpointId, say so with the old and new fingerprints.
6. **C3-a – Consistent key-recovery guidance.** `doctor`'s unreadable-key message matches the README Security model.
7. **D2 – Help.** `herdvia --help` / `-h` / `help` and `herdvia <command> --help` print help to stdout and exit 0.
8. **B3 – Confirmed reloads.** Every one-shot local CONTROL request has an overall time limit; when `allow` / `revoke` / `accept` saved the file but the running agent did not confirm the reload, the CLI says exactly that and fails; `herdvia agent reload` re-applies the file on demand.

## Non-Goals

- Changing `allow` semantics (no `--replace` requirement; that was C2-a, not chosen).
- Automatically running `hc machine rm` on revoke (B2-a, not chosen).
- Any change to the network protocol (`herdvia/1`, `herdvia/pair/1`) or the exact connect-line allowlist.
- B4, A2, C3-b.
- Making file edit + reload one atomic agent-side operation, or having the agent watch the `peers` file (B3 alternatives, not chosen).

## Anti-Goals (would make this a failure even if it works)

- Breaking compatibility between a new CLI and an old running agent, or an old `doctor` and a new agent. The existing 5-column `status` reply must stay byte-for-byte the same shape.
- Presenting a guess as a fact: a timeout must not be reported as "the PC is off"; a reason the remote did not send must be shown as unknown.
- Leaking unsanitized remote-chosen text (close reasons) into terminals or logs.
- Any read-only command (`peers`, `status`, `doctor`, `--help`) creating the data folder, `secret.key`, or repairing permissions.
- Weakening any existing refusal (name/key conflict checks, root/elevation refusal).

## Constraints

- Rust, existing crates only. macOS + Windows supported, Linux builds.
- Error convention stays: one `herdvia: ...` line on stderr, exit 255. Exit 1 keeps its existing meaning for pairing/registration outcomes.
- Local IPC CONTROL payloads are an allow-list in `agent.rs`. An agent from before this batch (v0.1.4 and earlier) checks CONTROL payloads in its frame layer and drops the connection on an unknown one without any reply frame, so the client sees `agent closed without EXIT`, a reset, or a broken pipe. From this batch on, the frame layer only bounds the length and the agent answers an unknown payload with ERROR `unknown control request`.
- A separate Claude session (Herdr terminal `t71`) currently has ~2,400 lines of uncommitted changes in the main working folder (pairing confirmation, data-dir parent checks, doctor timeouts, README updates) under review. This work must not touch the main working folder until that lands.

## Approaches Considered

### Where the detailed status comes from (A1)

| Approach | Trade-off | Decision |
|---|---|---|
| **A. New CONTROL payload `status-detail`** returning a versioned line format; CLI and doctor fall back to plain `status` when the agent rejects it (an older agent drops the connection; a newer one answers `unknown control request`) | Old/new mixes keep working; one extra payload in the allow-list | **Chosen** |
| B. Add columns to the existing `status` reply | Simplest, but the old `doctor` warns on anything other than exactly 5 fields | Rejected |
| C. Only log reasons | Smallest, but the user must find OS-specific log files and still cannot see retry timing | Rejected |

### How "retry now" works (A1, B3)

| Approach | Trade-off | Decision |
|---|---|---|
| **A. One command, `herdvia agent reload`, that sends the existing `reload` CONTROL** (reload already re-reads peers and wakes every outbound worker with zero delay). It serves both B3's "re-apply the file" and A1's "retry now" | Works with old agents; wakes all peers, not one; one command instead of two that do the same thing | **Chosen** (2–4 PCs, waking all is harmless) |
| B. Separate `herdvia retry` alias next to `agent reload` | Friendlier word for A1, but two commands with identical effect | Rejected |
| C. New `retry <name>` payload | Per-peer, but needs a parameterized CONTROL and old-agent fallback | Rejected |

### Making reload failures visible (B3)

| Approach | Trade-off | Decision |
|---|---|---|
| **A. Time limit on every one-shot CONTROL request + an honest "saved but not confirmed" failure + `agent reload`** | Small, local, no protocol change; user re-applies by hand | **Chosen (user picked B3-a)** |
| B. Agent performs file edit and reload in one request | Consistent when the agent runs, but a second path is still needed for "agent off", and a lost reply is still ambiguous | Rejected |
| C. Agent watches the `peers` file | Fewer missed reloads, but per-OS watchers and read-failure policy; CLI still cannot confirm | Rejected |

### Re-running Herdr registration (B1)

| Approach | Trade-off | Decision |
|---|---|---|
| **A. New `herdvia register <name>`** | Clear meaning, reuses `pair::hc`; one more command | **Chosen (user picked B1-a)** |
| B. Reinterpret `connect <existing>` as registration repair | Fewer commands but mixes pairing with registration | Rejected |
| C. Print the manual `hc machine add` line only | Smallest; user still runs it by hand | Rejected |

### Workspace isolation (process)

| Approach | Trade-off | Decision |
|---|---|---|
| **A. Git worktree outside the repo** (`/Users/jupi/Repos/herdvia-worktrees/usability`, branch `usability-batch`) whose first commit is a snapshot of the main folder's current uncommitted state; at the end, rebase only this batch's commits onto whatever `t71` finally commits (`git rebase --onto <t71-final> <snapshot> usability-batch`) | Never touches the main folder; builds on the in-flight pairing changes, so the rebase conflicts only where `t71`'s review fixes overlap | **Chosen** |
| B. Worktree from clean `HEAD` | Larger rebase conflicts later in `doctor.rs`, `pair/cli.rs`, `pair/hc.rs`, README | Rejected |
| C. Wait for `t71` to commit, then work in place | No conflicts, but blocks for an unknown time | Rejected |

## Design

### A1. Disconnection reasons

**Agent state (in memory only).** `Network` keeps, per EndpointId, a small record:

- `last_failure`: one of
  - `Rejected { code: u64, reason: String }`: the remote closed the connection with an application close (for example `403 peer not allowed`, `403 inbound permission required`, `403 peer permissions changed`).
  - `Unreachable { detail: String }`: `endpoint.connect` failed (timeout or other dial error).
  - `Closed { detail: String }`: an established connection ended for another reason (idle timeout, reset, local close).
- `at`: when it happened (`Instant`).
- `next_retry`: for peers with an outbound worker, the `Instant` the worker will dial next (set before it sleeps, cleared when it dials).

Remote text is passed through the existing `log` sanitizer and truncated (same limits as log lines) before it is stored. Records for peers no longer in `peers` are dropped on reload. Nothing is persisted.

**Where it is captured.** In `outbound` (dial error → `Unreachable`; set `next_retry` before sleeping) and at the end of `serve_connection` from `conn.close_reason()`. *(My call:)* only an application close by the remote with **code 403** counts as `Rejected`; every other close, including the `0 session closed` a remote agent sends when it stops or restarts (network.rs `Registration` drop), counts as `Closed`. A successful connection clears `last_failure` for that peer.

**Hints by 403 reason.** `peer not allowed` and `inbound permission required` → "on <name> run herdvia pending, then herdvia accept" (or "allow this PC with --inbound" for the latter). `peer permissions changed` → "<name> changed this PC's permissions (it may have revoked this PC); check herdvia peers on <name>". Any other 403 text → shown as-is with no hint.

**CONTROL `status-detail`.** New payload. Reply: a header line `herdvia-status-detail 1`, then one line per peer:
`<name> <in|out|both> <state> <path> <rtt> <reason-kind> <retry-secs> <reason-text…>`
where `state` is `connected` / `disconnected` / `waiting` (inbound-only and not connected), `reason-kind` is `-`, `rejected`, `unreachable`, or `closed`, `retry-secs` is `-` or whole seconds until `next_retry`, and `reason-text` is the sanitized remainder of the line (may contain spaces, may be empty). The exact text layout is the builder's to refine; it must be parseable by the CLI and future-proofed by the version header.

**`herdvia status` output.** Prints the same 5-column table as today. Below it, for each peer that is not connected, one human line, for example:

```
win1: rejected by the peer (403 peer not allowed); on win1 run herdvia pending, then herdvia accept. Retrying in 12s.
home: could not reach it (timed out); its agent may be off or offline. Retrying in 18s.
laptop: waiting for laptop to connect (this PC only accepts it).
```

If the agent rejects `status-detail` (see Constraints for how an older agent does that), print the table only plus one stderr note: `herdvia: note: restart the agent (herdvia agent install) to see why peers are disconnected`.

**`doctor`.** The per-peer `warn` line uses the same wording when detail is available, and keeps today's wording otherwise. `doctor` still parses the plain 5-column `status` for the table logic; it requests `status-detail` separately with the same timeout `doctor` uses for `status`.

**`via` error.** When `connection_for` times out waiting for a peer, the error becomes `peer did not connect within 5 seconds: <short reason>` when a `last_failure` exists (for example `…: rejected by the peer (403 peer not allowed)`), so Herdr's machine list shows the cause. Without a record, the text stays as today.

**Retry now.** The status hints say `run herdvia agent reload to retry now` where a retry is pending (see B3 for the command).

### B1. `herdvia register <name>`

1. Load `peers` (no key creation). If no peer has that name, or it lacks outbound permission → 255 with `no outbound peer named <name>; pair it with herdvia connect first`.
2. Build the `--via` value exactly as `connect` does today (reuse `pair::hc`).
3. If `hc` is found, run `hc machine ls --json` (same 15-second limit). If a machine with that name exists (real shape: `{"name":"win1","target":"/Users/jupi/.local/bin/herdvia via win1","via":true,...}`):
   - `via` is `true` and `target` equals the target this executable would register (current executable path + ` via <name>`, compared the way `hc` prints it) → print `<name> is already registered in Herdr` and exit 0;
   - `via` is `true`, `target` ends with ` via <name>`, and its program's file name is `herdvia` / `herdvia.exe` but the path differs (old install location, moved binary, cargo build path) → exit 1 with `Herdr's <name> uses another herdvia executable (<old path>); remove it with hc machine rm <name>, then run herdvia register <name>` *(my call: a stale path must not look healthy, and automatic removal is a non-goal)*;
   - otherwise → exit 1 with `Herdr already has a machine named <name> that does not use herdvia; rename or remove it (hc machine rename / hc machine rm), then run herdvia register <name>`.
   - If `hc machine ls` fails or its JSON cannot be read, skip this check and continue.
4. Run `hc machine add --via … --name <name>` as `connect` does. Success → exit 0. Missing `hc`, failure, or timeout → exit 1 and print the exact manual command (same text as `connect`'s fallback). A timeout says the outcome is unknown rather than "not registered".
5. Does not require the agent to be connected first; it does not wait on `status` (unlike `connect`).
6. `connect`'s refusal for an already-registered outbound name (`peers.rs` `check_connect_target`) adds: `; to re-run only the Herdr registration, run herdvia register <name>`.

### C1. `herdvia peers`

- Reads `peers` with the same safety checks `load` already applies; never creates the folder or key, never repairs permissions. A missing file prints `no peers registered`.
- Output: header `name permission fingerprint`, one row per peer (`in` / `out` / `both`), then a legend line: `in = may open sessions on this PC; out = this PC may connect to it`.
- Works whether or not the agent runs; states nothing about live connections (that is `status`).

### B2-b. `revoke` follow-up hints

After the existing `revoked <name> <id>` line (stdout unchanged), print to stderr:

- if the revoked peer had outbound permission: `herdvia: note: Herdr may still list <name>; remove it with: hc machine rm <name>`
- always: `herdvia: note: on that PC, also revoke this PC (herdvia revoke <the name it uses for this PC>)`

The existing duplicate-key warning stays and comes first.

### C2-b. `allow` key-change notice

`peers::allow` returns the replaced entry when the same name previously had a different EndpointId. The CLI then prints to stderr, after the normal stdout line:
`herdvia: warning: <name> now points to a different key (fingerprint <old> -> <new>)`.
Same-key permission changes print nothing extra. Behavior and exit code are unchanged.

### C3-a. Unreadable-key guidance in `doctor`

Replace "restore it from a backup …" with wording consistent with the README Security model: fix the file's permissions or ownership if that is the cause; otherwise remove it, run `herdvia id` to create a new identity, and pair again with every peer, because restoring a copied or backed-up key is not recommended. Keep it one line.

### D2. Help

- `herdvia --help`, `herdvia -h`, `herdvia help` → stdout: one-paragraph description, the command list with one line each, and a pointer to `herdvia <command> --help`; exit 0.
- `herdvia <command> --help` (and `-h`) for every public command → its usage line, two or three sentences, and one example; exit 0. `via` gets a one-liner saying it is internal.
- Parse errors keep today's behavior (usage on stderr, exit 255). No `--help` path touches the data folder or agent.

### B3. Confirmed reloads

Builds on `t71`'s change to `control::reload_if_running` (a refused connection is retried for up to 1 second before it counts as "no agent").

- **Overall time limit.** `control::request` (used by `reload`, `status`, `status-detail`, `pending`) is wrapped in a single deadline covering connect, write, and the full reply: **5 seconds** (my call; `doctor` keeps its own shorter limit). On expiry it returns a `TimedOut` error naming the request: `the agent did not answer the <request> request within 5 seconds`.
- **"No agent" stays a success for file edits.** As today, a missing socket/pipe (or a refusal that lasts the 1-second window) means no agent is running; the file change alone is the complete result and the CLI prints its usual line.
- **Saved but not confirmed.** For `allow`, `revoke`, and `accept`: if the file was changed and the reload then fails for any other reason (timeout, protocol error, ERROR frame such as an unreadable `peers` file), the CLI does **not** print its success line and exits 255 with one line:
  `herdvia: <revoked|saved> <name> in the peers file, but the running agent did not confirm it (<cause>); run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)`.
  For `revoke`, the wording stresses the risk: `… the running agent may still admit <name> until it reloads …`.
- **`herdvia agent reload`.** New subcommand. Sends `reload`; on success prints `reloaded: peers re-read; waiting connections retry now` and exits 0. If no agent is running → 255 with the usual "agent is not running" message (unlike `allow`, here a missing agent is an error, because the user explicitly asked to reach it). Works with old agents (the payload already exists).
- **Old CLI / new agent and new CLI / old agent.** No payload changes for B3, so every mix behaves as today except for the new messages.
- *(Builder calls, accepted:)* when a reload is not confirmed, the warnings that describe the saved file still print (revoke's duplicate-key warning, allow's key-change warning from C2-b), but B2-b's cleanup notes do not. The 5-second limit applies per request, inside the 1-second retry-on-refused loop. `listen` / `connect` / `via` open their own IPC connections and stay untimed.
- **Docs.** README Commands table, Security model (`revoke` paragraph), and Troubleshooting add `agent reload` and the "not confirmed" case; DESIGN.md's `allow`/`revoke` section describes the time limit and the new failure.

### Documentation

README (English) Commands table and Troubleshooting gain `peers`, `register`, `agent reload`, and the status reason lines. The three translations and DESIGN.md (Korean) get the matching changes. If `t71`'s README edits are still uncommitted at implementation time, documentation changes are made last and kept in a separate commit to ease the rebase.

## Testing

- Unit: `status-detail` formatting and parsing round-trip; reason capture from a remote application close vs. dial error; `next_retry` countdown; sanitization of hostile close reasons; `allow` returning the replaced entry; `register`'s `hc machine ls` JSON matching (herdvia target / foreign machine / unreadable JSON); help parsing for every command.
- B3: a fake agent socket that accepts but never answers → `revoke` exits 255 within ~5 s with the "not confirmed" line and the peer is gone from the file; a fake agent that answers ERROR → same; no agent → `revoke` succeeds as today; `agent reload` against no agent → 255; `control::request` deadline covers a stalled reply mid-stream.
- Integration (existing relay-free harness in `tests/`): two agents where B has not allowed A → A's `status` shows `rejected` with the 403 reason; `agent reload` wakes a backed-off worker; `peers` works with no agent and creates nothing; `register` with a fake `hc` on `PATH` (already registered / foreign / add succeeds / add fails); new CLI against an agent that rejects `status-detail` falls back cleanly.
- CI lists of exact test names (`.github/workflows/ci.yml`) are updated for new tests.

## End State of This Work *(my call)*

- The snapshot commit of the main folder's uncommitted state (including untracked `src/ipc/dir_security.rs`) is taken **once**, at the start. Later edits by `t71` are not pulled in.
- This plan ends with reviewed, passing commits on `usability-batch` in the worktree (local checks + Codex review).
- Landing is a separate step after `t71` commits: `git rebase --onto <t71-final> <snapshot> usability-batch`, rerun all checks, then merge into `main` **only after the user confirms**. If `t71` never commits, the branch stays as is and the user decides.
- No version bump, tag, push, or release.

## Left to the Builder

- Exact struct and function names, file placement (a new `src/register.rs` vs. inside `pair/hc.rs`), and the precise human wording within the shapes above.
- Exact `status-detail` line encoding, as long as it is versioned and parseable.
- How `register`'s fake-`hc` tests are wired, reusing the pattern in `tests/pair.rs`.

## Related Work

- `t71`'s uncommitted change retries `reload` for up to 1 second when a running macOS agent's socket backlog is full. B3 here builds on top of it.
