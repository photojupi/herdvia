# Usability Batch Implementation Plan

Spec: `docs/superpowers/specs/2026-11-16-usability-batch-design.md` (read it first; it wins over this
plan wherever they disagree, except for the "Plan decisions" below, which settle points the spec
left to the builder).

Worktree: `/Users/jupi/Repos/herdvia-worktrees/usability`, branch `usability-batch`.
Base: snapshot commit `3a9fe05` (a copy of another session's in-flight work). This batch is later
rebased onto that session's final commits, so keep edits to existing lines small and local.

## Rules for every task

- Work only inside the worktree. Never read-modify, stage, commit, or stash anything under
  `/Users/jupi/Repos/herdvia/` (the main folder; another session owns it). Use absolute paths and
  `cd /Users/jupi/Repos/herdvia-worktrees/usability` in every shell command.
- Existing crates only. Do not add a dependency (`serde_json` is not in the tree).
- Error convention: one `herdvia: ...` line on stderr, exit 255. Exit 1 only for
  pairing/registration outcomes. Notes and warnings are `herdvia: note: ...` /
  `herdvia: warning: ...` on stderr.
- Read-only commands (`peers`, `status`, `doctor`, `--help`, `register`'s peer lookup,
  `agent reload`) must not create the data folder or `secret.key` and must not repair permissions,
  on Unix and on Windows. Check what `ipc::private_file` / `ipc::data_dir` do in
  `src/ipc/windows.rs` before relying on them; `doctor.rs` has read-only openers if needed.
- Text a remote chose (close reasons, error strings that embed them) goes through
  `log::sanitize` before it is stored, sent over IPC, or printed.
- Code must build on macOS, Linux, and Windows. `cfg(unix)`-only tests are fine where a fake agent
  socket is needed (as in `src/control.rs` and `tests/doctor.rs`).
- Match the existing style: short doc comments that say why, no new abstractions for one use, no
  drive-by edits. Every changed line must trace to the task.
- Tests first where practical. Each task ends with all of these passing from the worktree:
  `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --locked --lib --bins`, and the "Integration tests (no connection)" step of
  `.github/workflows/ci.yml` (copy its exact commands). Add each new integration test that needs
  no public relay to that step's exact-name list. Integration tests that need the n0 relay are not
  added there.
- One commit per task (plus fix-up commits from review). Commit messages in English only, no
  Hangul, imperative subject. End each message with these two lines:
  `Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_018ZAsEVYNwWRoQoPKr63rcE`
- Do not edit `README.md`, `docs/README.*.md`, or `DESIGN.md` in tasks 1–6. Task 7 does all
  documentation. If an existing test asserts README text that a code change invalidates, say so in
  the report instead of editing the README.

## Plan decisions (settle what the spec left open)

1. **Failure records are for the outbound direction only.** `Network` records a failure when
   `endpoint.connect` fails and when a connection this side dialed (`Side::Client`) ends. Inbound
   connections ending do not touch the record, so a `both` peer's "rejected" reason is not
   overwritten by the other direction closing. Inbound-only peers have no record; their state is
   `waiting`.
2. **Classification.** A remote application close with code 403 is `rejected` (reason text
   `403 <sanitized reason>`), whether it surfaces as an error from `endpoint.connect` or as
   `conn.close_reason()` after the handshake. A dial error without such a close is `unreachable`.
   Every other end of an established outbound connection is `closed`. Check the iroh 1.4 error
   types in `~/.cargo/registry` instead of guessing their shape.
3. **Record lifetime.** A record is written only while the id still has outbound permission
   (checked under the record lock), is cleared when an outbound connection is registered, and is
   dropped on reload for ids without outbound permission. Lock order: `connections` before the
   record map, never the reverse.
4. **One request for `herdvia status`.** The CLI asks `status-detail` first and renders the
   5-column table from it (byte-identical to the `status` reply; a unit test compares the two).
   Only when the agent rejects `status-detail` does it ask plain `status` and print the restart
   note. Rejected means an ERROR `unknown control request` (agents from this batch on) or the
   connection ending without a reply: `agent closed without EXIT`, `ConnectionReset`,
   `ConnectionAborted`, `BrokenPipe`, or `UnexpectedEof` (v0.1.4 and earlier drop the connection
   in the frame layer on a CONTROL payload they do not know; check what the Windows pipe
   surfaces). Any other error (`TimedOut`, `NotFound`, `ConnectionRefused`, other ERROR text) is
   the command's error, as today.
5. **`herdvia status` stdout.** The table, then, if any peer is not connected, one empty line and
   one line per such peer. If any of them has a pending retry, a final line
   `Run herdvia agent reload to retry now.` The old-agent note and the relay note go to stderr.
6. **A new module `src/status.rs`** (`pub mod status`) holds the `status-detail` row type, its
   formatter and parser, the table renderer, and the human wording, so `network.rs`, `main.rs`,
   and `doctor.rs` share one definition.
7. **`control::reload`** returns whether an agent confirmed (`Ok(true)`) or none is running
   (`Ok(false)`); `reload_if_running` keeps its signature on top of it. `herdvia agent reload`
   uses it, so it also gets the 1-second retry-on-refused.
8. **`register` reads `hc machine ls --json` with a small hand-written reader** in `pair/hc.rs`
   that extracts `name`, `target`, and `via` of each object in `machines`. Anything it cannot read
   means "skip the check and continue", never a crash and never a false "already registered".
9. **Help arms come first in `parse`.** `--help` and `-h` are valid peer names today, so
   `listen --help` currently means "listen as --help". After D2 they mean help.

## Tasks (in this order, one after another)

### Task 1 — B3: confirmed reloads

Files: `src/control.rs`, `src/main.rs`, tests.

- `control::request`: one 5-second deadline over connect, write, and the full reply. On expiry:
  `io::ErrorKind::TimedOut`, `the agent did not answer the <request> request within 5 seconds`.
  Check that the Windows `ipc::connect` cannot block past the deadline (a synchronous wait inside
  it would defeat `tokio::time::timeout`); report what you find.
- `control::reload` / `reload_if_running` per decision 7. The 5-second limit applies per request
  inside the 1-second retry-on-refused loop.
- `allow` / `revoke` / `accept` in `main.rs`: when the file was changed and the reload fails for a
  reason other than "no agent", do not print the success line; print the warnings that describe
  the saved file (revoke's duplicate-key warning), then fail with exit 255 and one line:
  - allow/accept: `saved <name> in the peers file, but the running agent did not confirm it (<cause>); run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)`
  - revoke: `revoked <name> in the peers file, but the running agent did not confirm it (<cause>), so it may still admit <name> until it reloads; run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)`
- `herdvia agent reload`: success prints `reloaded: peers re-read; waiting connections retry now`,
  exit 0. No agent: 255 with the same "agent is not running" wording `pair/cli.rs` uses. It must
  not create the data folder or key.
- Tests (spec "Testing", B3 bullet): fake agent that accepts and never answers → `revoke` exits
  255 in about 5 s with the "not confirmed" line and the peer is gone from the file; fake agent
  answering ERROR → same; no agent → `revoke` succeeds as today; `agent reload` with no agent →
  255 and nothing created; the `request` deadline covers a reply that stalls mid-stream.

### Task 2 — C1, C2-b, B2-b, C3-a: peers list and CLI notes

Files: `src/peers.rs`, `src/main.rs`, `src/doctor.rs`, `tests/peers.rs`, `tests/doctor.rs`.

- C1 `herdvia peers`: header `name permission fingerprint`, one row per peer, then
  `in = may open sessions on this PC; out = this PC may connect to it`. No peers (or no file, or
  no data folder): `no peers registered`. Same safety refusals as `peers::load`; creates nothing.
- C2-b: `peers::allow` returns the replaced entry when the name had a different EndpointId. The
  CLI prints, after its stdout line, `herdvia: warning: <name> now points to a different key (fingerprint <old> -> <new>)`.
  With an unconfirmed reload (Task 1) this warning still prints before the error.
- B2-b: after `revoked <name> <id>` and the duplicate-key warning, on stderr:
  outbound peers get `herdvia: note: Herdr may still list <name>; remove it with: hc machine rm <name>`;
  always `herdvia: note: on that PC, also revoke this PC (herdvia revoke <the name it uses for this PC>)`.
  Not printed when the reload was not confirmed.
- C3-a: `doctor`'s unreadable-key line, one line, consistent with the README Security model: fix
  the file's permissions or ownership if that is the cause; otherwise remove it, run `herdvia id`
  to create a new identity, and pair again with every peer (restoring a copied or backed-up key is
  not recommended).

### Task 3 — A1 agent side: failure records and `status-detail`

Files: new `src/status.rs`, `src/lib.rs`, `src/network.rs`, `src/agent.rs`.

- `src/status.rs` per decision 6. Reply format: header `herdvia-status-detail 1`, then per peer
  `<name> <in|out|both> <state> <path> <rtt> <reason-kind> <retry-secs> <reason-text…>`;
  `state` is `connected` / `disconnected` / `waiting`; `reason-kind` is `-`, `rejected`,
  `unreachable`, `closed`; `retry-secs` is `-` or whole seconds; `reason-text` is the rest of the
  line and may be empty. The parser rejects an unknown header version (returns "cannot parse",
  callers fall back) and tolerates unknown reason kinds by showing the text without a hint.
- Human wording (spec A1): rejected → `<name>: rejected by the peer (<text>)` plus the hint for the
  three known 403 reasons; unreachable → `<name>: could not reach it (<text>); its agent may be off or offline`;
  closed → `<name>: the connection closed (<text>)`; waiting →
  `<name>: waiting for <name> to connect (this PC only accepts it)`; not connected with no record
  → say only what is known (for example `<name>: not connected yet`), never a guessed cause.
  `Retrying in <n>s.` is appended when a retry is pending. A short form of the reason (no hint) is
  exposed for the `via` error.
- `Network`: records per decisions 1–3; `next_retry` set before the worker sleeps and cleared when
  it dials; `status_detail()`; `status()` output unchanged byte for byte.
- `via` error: on the 5-second timeout in `forward`, when a record exists for that peer the text
  becomes `peer did not connect within 5 seconds: <short reason>`; otherwise unchanged. The whole
  line must still fit `log::sanitize`'s limit without double-escaping the stored reason.
- `agent.rs`: `status-detail` joins the CONTROL allow-list.
- Tests: format/parse round trip (including hostile reason text and an empty reason); table
  rendered from detail equals `status()`; two local `Network`s where B has not allowed A → A's
  detail shows `rejected` with `403 peer not allowed` and a retry countdown (model it on
  `reload_wakes_sleeping_outbound_worker_immediately`); a dial to a dead address → `unreachable`;
  a record is dropped when the peer loses outbound permission; the `via` timeout text.

### Task 4 — A1 CLI side: `status` and `doctor`

Files: `src/main.rs`, `src/doctor.rs`, tests.

- `herdvia status` per decisions 4 and 5. Old agent: table only, plus
  `herdvia: note: restart the agent (herdvia agent install) to see why peers are disconnected`.
- `doctor`: still drives its table logic from the plain `status` reply; asks `status-detail`
  separately with `AGENT_TIMEOUT`; when it parses, the per-peer `warn` lines use the Task 3
  wording, otherwise today's wording. A stuck agent is not asked again.
- Tests: fake agent that answers `status-detail` with rejected / unreachable / waiting rows →
  `herdvia status` stdout and `doctor` lines; fake agent that answers `unknown control request` →
  table plus the note, exit 0; fake old agent that reads only the 5-byte header of the 13-byte
  `status-detail` CONTROL frame and closes (as v0.1.4's frame check does), then answers plain
  `status` → same; integration with two real local agents if the existing harness can
  do it without the relay.

### Task 5 — B1: `herdvia register <name>`

Files: `src/pair/hc.rs`, `src/pair/cli.rs`, `src/peers.rs`, `src/main.rs`, `tests/pair.rs`.

- Spec section B1, steps 1–6, with decision 8. Reuse `via_command_for_current_os`,
  `manual_hc_add_line`, `find_hc`, and the 15-second limit; share the spawn/timeout/kill code
  between `hc machine ls` and `hc machine add` instead of copying it.
- A timeout of `hc machine add` says the outcome is unknown. `connect`'s own output is unchanged.
- `check_connect_target`'s "already registered" refusal gains
  `; to re-run only the Herdr registration, run herdvia register <name>`.
- Tests with the fake `hc` from `tests/pair.rs`: already registered (exit 0, `hc machine add` not
  run), stale herdvia path (exit 1, `hc machine rm` hint), foreign machine (exit 1), `ls` output
  unreadable → add runs, add succeeds, add fails (exit 1 with the manual line), no `hc`, unknown
  or inbound-only name (255). Unit tests for the JSON reader: escapes in strings, nested objects,
  missing fields, truncated input, a `machines` entry that only looks like a match inside a string.

### Task 6 — D2: help

Files: new `src/help.rs` (a module of the binary), `src/main.rs`, tests.

- `herdvia --help` / `-h` / `help` and `herdvia <command> --help|-h` for every public command
  (`agent`, `listen`, `connect`, `register`, `peers`, `id`, `allow`, `revoke`, `status`, `pending`,
  `accept`, `doctor`; `via` gets one line saying it is internal). stdout, exit 0, no data folder
  or agent access. `agent`'s help covers `install`, `uninstall`, `reload` (and `--log` on Windows).
- Parse errors keep today's behavior; the usage line lists the new commands.
- Tests: every command's help parses and prints its usage line and an example; `--help` with a
  missing data folder creates nothing; `listen --help` no longer starts listening.

### Task 7 — Documentation

`README.md`, `docs/README.{ko,ja,zh-CN}.md`, `DESIGN.md`, in one separate commit: Commands table,
Troubleshooting, Security model (`revoke`), the `allow`/`revoke` section of DESIGN.md.

### Task 8 — Whole-branch review and Windows cross-check

Review of `3a9fe05..usability-batch` by a different model, and the Windows cross-build that CI runs.
