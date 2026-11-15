# `herdvia listen` and `herdvia connect` Pairing Design

## Intent and Why

Today, after installing `herdvia` on two PCs, a first-time user must run five to six separate commands across both machines (`herdvia id`, `herdvia allow ... --outbound`, `herdvia pending`, `herdvia accept ... --inbound`, `hc machine add --via ... --name ...`), copy a 64-character `EndpointId`, type a 16-character hex fingerprint, reason about `--inbound` versus `--outbound` packet directions, wait up to 30 seconds for retry backoff, and get shell/argument quoting right for `hc machine add` (especially on Windows).

The goal is to make first-time P2P setup feel as intuitive as Bluetooth Secure Simple Pairing while preserving `herdvia`'s existing security boundaries:

1. On the target PC to be controlled: run **`herdvia listen`**.
2. On the controller PC running Herdr: run **`herdvia connect <name>`**.
3. On the same local network, `connect` discovers the waiting PC automatically without copying any key or token; across different routers, firewalls, or locations, `connect` accepts a single one-time invite token printed by `listen`.
4. Both screens display the same 6-digit verification code (Short Authentication String, SAS), the person approves `[y/N]` on the target PC, both `peers` files update immediately with zero retry-backoff delay, and the controller PC registers the machine in Herdr (`hc machine add`) automatically using `herdvia`'s exact executable path.

## Goals, Non-Goals, and Anti-Goals

### Goals

- Reduce first-time pairing and Herdr registration to two role-named commands (`herdvia listen` and `herdvia connect <name>`) with no `--inbound`/`--outbound` flags.
- Support zero-copy automatic discovery when both PCs are on the same LAN/Wi-Fi and UDP broadcast/multicast reaches between them.
- Seamlessly fall back to a single-line, single-use invite token (`hvi1-...`) when PCs are on different networks, isolated guest Wi-Fi, double-NAT subnets, or blocked by local firewall rules.
- Maintain mutual cryptographic verification via TLS 1.3 session binding plus a commit-reveal 6-digit SAS and explicit human `[y/N]` confirmation on the target PC before granting `--inbound` access.
- Eliminate the up-to-30-second post-approval connection wait by authorizing and reloading the target agent before returning approval to the controller.
- Run `hc machine add --via <quoted-current-exe-via-name> --name <name>` automatically on the controller PC so `PATH` propagation delays and Windows/Unix quoting rules do not trip newcomers.
- Perform fast preflight checks (`agent` reachability and, on `listen`, `herdr-core` availability) before waiting, printing actionable one-line remediation when something is missing.
- Keep all existing commands (`agent`, `via`, `id`, `allow`, `revoke`, `status`, `pending`, `accept`, `doctor`, `--version`) and wire framing for `herdvia/1` 100% backward-compatible.

### Non-Goals

- Physical Bluetooth / BLE radio transport (`CoreBluetooth` / WinRT BLE).
- Short 6–8 digit remote rendezvous codes across the internet (which would require operating or abusing an external rendezvous lookup slot plus a PAKE dependency).
- Terminal ASCII QR code rendering in v1 (builder call: kept out to keep terminal output compact in split panes and avoid extra code, since single-line `hvi1-...` tokens select cleanly on double-click; can be added later if desired).
- Installing `herdr-core` automatically on either PC.
- Modifying `herdvia via` (which `hc` invokes non-interactively as a raw byte pipe).

### Anti-Goals

- Never weaken `herdvia/1` or allow an unauthenticated or pairing-in-progress peer to reach `exec::serve` / `herdr-core connect`.
- Never trust LAN discovery names, hostnames, or a leaked invite token alone without interactive human `[y/N]` confirmation on the target PC backed by SAS verification.
- Never default to `--both` permissions just for convenience; `connect` grants `out` on the controller (preserving `both` if `in` was already granted to that `EndpointId`) and `listen` grants `in` on the target (preserving `both` if `out` was already granted to that `EndpointId`).
- Never silently overwrite an existing peer name that points to a different `EndpointId`.
- Never add new third-party crates outside the existing `Cargo.lock` dependency graph (`about.toml` license pinning, `cargo audit --deny warnings`, and Linux-to-`x86_64-pc-windows-gnu` cross-builds must continue to pass without new external dependencies).

## Constraints

- **Single binary & existing lockfile:** Built in Rust 2024 (`Cargo.toml`). `blake3` (`1.8.7`), `data-encoding` (`2.11.1`), and `socket2` (`0.6.5`) are already resolved in `Cargo.lock` and audited/clarified in `about.toml`; declaring them as direct dependencies of `herdvia` introduces zero new external packages.
- **Single owner of `secret.key`:** Only the running `herdvia agent` binds `secret.key` on an `iroh::Endpoint`. Both `herdvia listen` and `herdvia connect` talk to their local `herdvia agent` over the existing authenticated user-isolated local IPC (`agent.sock` on Unix, per-SID named pipe on Windows) and fail immediately with a clear message if the local agent is not running.
- **Cross-platform:** Must compile cleanly and pass unit tests on macOS/Unix and Windows (`x86_64-pc-windows-gnu` cross-compile and `windows-latest` MSVC unit tests).
- **No test backdoors:** Product code must not add special test-only flags or env vars. Local `hc` lookup follows the same `$PATH` and `$HOME` / `%USERPROFILE%` conventions already used by `doctor.rs`, allowing integration tests with isolated `HOME` and `PATH` directories to test `hc machine add` cleanly without touching real user state.

## Approaches Considered

1. **Guided wrapper over existing `id` / `allow` / `pending` / `accept`:** Wraps the six commands into interactive prompts without changing the network layer. Lowest effort, but keeps 64-char copy-paste on LAN, manual fingerprint typing, pending-list flooding exposure, and up-to-30-second backoff waits.
2. **Dedicated pairing ALPN (`herdvia/pair/1`) + hybrid LAN UDP discovery + one-time invite token + commit-reveal 6-digit SAS + auto `hc machine add` (Chosen):** Gives zero-copy Bluetooth-like pairing on the same LAN, one-line copy-paste across routers/firewalls, zero retry wait, and automatic Herdr registration using zero new crates outside `Cargo.lock`.
3. **Global short-code rendezvous + PAKE (Magic Wormhole style):** Allows typing a short code across different networks without clipboard sharing, but requires an external rendezvous lookup mapping (or ephemeral Pkarr slot management with squatting/rate-limit risks) plus PAKE cryptography and strict global attempt budgets. Rejected as disproportionate complexity for `herdvia`.

## Detailed Design

### 1. CLI Surface

| Command | Behavior |
|---|---|
| `herdvia listen` | Starts a single-use pairing listener (expires after 5 minutes or `Ctrl-C`). Performs preflight checks, advertises on LAN UDP, prints a single-use `hvi1-...` invite token, displays the 6-digit SAS when a peer connects, prompts `[y/N]` to grant inbound access, saves the peer using a sanitized/disambiguated name derived from the caller's hostname (or reuses the existing name if that `EndpointId` is already in `peers`), reloads the agent, and exits `0` on approval (`1` on decline/timeout, `255` on usage/IPC error). |
| `herdvia listen <name>` | Same as `herdvia listen`, but uses `<name>` (`peers::valid_name`) as the local peer alias on the target PC instead of deriving it from the caller's hostname. Builder call: `<name>` syntax is validated before listening; under the `peers` file lock upon approval, `<name>` is accepted if unused or already bound to the *same* `EndpointId`, and refused if bound to a *different* `EndpointId`. |
| `herdvia connect <name>` | Validates `<name>` and checks existing `peers` early (builder call: if `<name>` already exists with `outbound` access and no explicit `<invite>` was passed, fails fast before dialing with `"peer <name> is already registered; revoke it first or choose another name"` so a newcomer never discovers a name conflict after walking to the other PC). Discovers active `listen` peers on LAN UDP (~2 seconds). If one is found, dials it; if multiple are found, prompts the user to pick one (or enter an invite token); if none is found, prompts on stdin to paste the `hvi1-...` invite token. Displays the 6-digit SAS while waiting for approval on the target PC, saves `<name>` (`out`, or `both` if already `in`), waits briefly for `herdvia/1` readiness, and runs `hc machine add`. |
| `herdvia connect <name> <invite>` | Skips LAN discovery and immediately dials the target encoded in `<invite>` (`hvi1-...`), then runs the identical SAS display, `peers` update, and `hc machine add` flow. |

Usage string in `src/main.rs` is updated to include `listen [<name>]` and `connect <name> [<invite>]`.

### 2. Preflight Checks

- **Both `listen` and `connect`:** Query the local agent (`doctor` control request).
  - If the agent is not running: exit `255` with `herdvia: agent is not running; start it with 'herdvia agent install' (or 'herdvia agent' in a terminal)`.
  - If the running agent is from an older version that rejects the pairing control request: exit `255` with `herdvia: the running agent is older than this herdvia executable; restart it with 'herdvia agent install' (or restart 'herdvia agent')`.
- **`listen` `herdr-core` check (non-blocking warning to `stderr`):** Reuses `doctor.rs`'s `herdr-core` path/version check. If `herdr-core` is missing or only on the interactive `PATH` rather than the LaunchAgent path on macOS, `listen` prints a clear `herdvia: warning: ...` line with the exact install or `ln -s` command before waiting, without blocking pairing itself.

### 3. Invite Token Format (`hvi1-...`)

- Prefix: `hvi1-` (case-insensitive when parsed; printed lowercase).
- Payload: 48 raw bytes = `target_endpoint_id (32 bytes) || invite_secret (16 bytes CSPRNG)`.
- Encoding: RFC 4648 lowercase Base32 without padding (`data_encoding::BASE32_NOPAD` lowercased, 77 `[a-z2-7]` characters). Total length: **82 ASCII characters** (`hvi1-<77 chars>`), containing no whitespace or URL-special characters.
- Properties:
  - Contains no IP addresses (zero IP leakage if pasted into chat; `iroh` resolves `target_endpoint_id` through its normal `N0` discovery).
  - Single-use and ephemeral: `invite_secret` lives only in the `agent`'s memory for the lifetime of that `listen` session (max 5 minutes) and is never written to disk, logs, DNS, or LAN packets.

### 4. LAN Discovery Protocol (UDP, Active Only During Pairing)

- Runs inside the interactive `herdvia listen` and `herdvia connect` CLI processes (builder call: running UDP discovery in the foreground terminal process rather than the background `launchd` LaunchAgent avoids macOS 15+ background LaunchAgent Local Network restrictions when the terminal app already has local network access, and keeps the background agent completely silent on LAN).
- Default UDP port: `48721` (bound with `SO_REUSEADDR`, and `SO_REUSEPORT` on Unix where supported, so multiple local processes never fail with `AddrInUse`; internal library functions accept the port as a parameter so unit/integration tests use ephemeral loopback ports `127.0.0.1:0` deterministically).
- Multicast group (IPv4): `239.255.77.86:48721` plus subnet/limited broadcast `255.255.255.255:48721` (`SO_BROADCAST`).
- **Two-way discovery (survives one-sided firewall filtering):**
  - While waiting, `listen` multicasts/broadcasts a compact beacon every `500ms` AND listens for `DISCOVER` queries on port `48721` to reply directly via unicast UDP to the querier's source address.
  - During its `2s` scan window, `connect` binds an ephemeral UDP socket + listener on `48721`, sends `DISCOVER` queries (`b"HDVD1" || query_nonce[16]`) at `0ms`, `400ms`, and `900ms`, and collects unique beacons by `EndpointId`, ignoring beacons whose `EndpointId` equals its own `EndpointId` (preventing self-discovery while allowing two distinct `HERDVIA_DIR` instances on the same machine to discover each other).
- **Beacon wire format (`BEACON`, max ~120 bytes):**
  - Magic: `b"HDVB1"` (5 bytes)
  - `listen_id`: `[u8; 16]` (public session ID for this `listen` run — *not* `invite_secret`)
  - `endpoint_id`: `[u8; 32]` (`id_B`)
  - `quic_port`: `u16` big-endian (`0` if unknown; `agent` reports its bound IPv4/IPv6 QUIC UDP port(s) to `listen` when starting the session)
  - `hostname_len`: `u8` (`0..=32`) + `hostname`: UTF-8 sanitized ASCII label (`[A-Za-z0-9._-]`)
- When `connect` selects a LAN beacon received from UDP source IP `src_ip` with `quic_port > 0`, it passes `SocketAddr::new(src_ip, quic_port)` along with `id_B` and `listen_id` to its local `agent`. The `agent` dials `EndpointAddr::new(id_B).with_ip_addr(direct_addr)` (falling back to relay/DNS lookup simultaneously if configured), giving sub-50ms LAN pairing even without internet access.

### 5. Dedicated Pairing ALPN (`herdvia/pair/1`) and Cryptographic Protocol

#### Separation from `herdvia/1`

- `Network::bind` registers `vec![ALPN.to_vec(), PAIR_ALPN.to_vec()]` where `PAIR_ALPN = b"herdvia/pair/1"`.
- In `Policy::after_handshake`:
  - `conn.alpn() == ALPN` (`herdvia/1`): unchanged (`permitted(&peers, remote_id, server)`).
  - `conn.alpn() == PAIR_ALPN`:
    - `Side::Server`: accepted **only** if an active `ListenState` exists in `Network`, has not expired (5-minute deadline), and active in-flight `PAIR_ALPN` server connections `< 4`. Rejected `PAIR_ALPN` connections are **not** added to `pending`.
    - `Side::Client`: accepted **only** if an active outbound `ConnectState` in `Network` is currently dialing `conn.remote_id()`.
- Incoming `PAIR_ALPN` connections in `agent.rs` dispatch to `pair::serve_incoming`, never `network.serve_connection` or `exec::serve`.

#### Local IPC Coordination (`CONTROL` `"listen"` and `"connect"`)

- `frame::validate_text` adds `b"listen"` (6 bytes) and `b"connect"` (7 bytes) to the allowed `CONTROL` payloads (matching the existing `6..=7` length check in `frame.rs`). `exec::serve` on `herdvia/1` continues to reject all non-`OPEN` first frames.
- Only one `listen` session may be active per agent at a time; starting a second `herdvia listen` while one is active returns an `ERROR` frame (`a listen session is already active`).
- Dropping/closing the local IPC stream (`Ctrl-C` or terminal exit) immediately cancels the active `ListenState` / `ConnectState` and closes any open `PAIR_ALPN` connection.

#### Wire Handshake on `herdvia/pair/1` (Single Bidirectional QUIC Stream, 15s Step Timeouts, 60s Approval Timeout)

Both endpoints first extract a 32-byte TLS 1.3 session binding from the mutually authenticated QUIC connection:

```rust
conn.export_keying_material(&mut tls_binding, b"herdvia-pair-v1", &[])
```

All pairing messages are small length-prefixed frames (`tag: u8`, `len: u16 BE`, `len <= 256` bytes):

1. **`PAIR_HELLO` (tag `1`, A → B):**
   - `version`: `1u8`
   - `auth_kind`: `1u8` (Invite) or `2u8` (LAN `listen_id`)
   - `auth_proof`: `[u8; 32]`
     - If `auth_kind == 1`: `blake3::keyed_hash(blake3::derive_key("herdvia-pair-invite-v1", &invite_secret), &tls_binding)`
     - If `auth_kind == 2`: `blake3::keyed_hash(blake3::derive_key("herdvia-pair-lan-v1", &listen_id), &tls_binding)`
   - `commit_a`: `[u8; 32]` = `blake3::hash("herdvia-pair-commit-a-v1" || tls_binding || id_A || id_B || nonce_A)` where `nonce_A` is 32 CSPRNG bytes.
   - `caller_name`: UTF-8 sanitized hostname (`1..=32` bytes of `[A-Za-z0-9._-]`).
2. **Verification & Rate Limiting on B:**
   - B checks `version == 1` and verifies `auth_proof` in constant time (`blake3::Hash` equality uses `constant_time_eq`).
   - Invalid proofs increment `failed_attempts` on `ListenState`. After **5** failed proofs in a `listen` session, B aborts the `listen` session (`too many invalid pairing attempts; run herdvia listen again`) to bound online guessing.
   - Only **one** verified caller may hold the interactive approval prompt at a time; a second simultaneous caller receives `PAIR_REJECT` (`busy`).
3. **`PAIR_CHALLENGE` (tag `2`, B → A):**
   - `nonce_b`: `[u8; 32]` (32 CSPRNG bytes, generated *after* receiving `commit_a`).
   - `target_name`: UTF-8 sanitized hostname of B (`1..=32` bytes).
4. **`PAIR_REVEAL` (tag `3`, A → B):**
   - `nonce_a`: `[u8; 32]`.
5. **Commit Verification & Unbiased 6-Digit SAS Derivation (Both Sides):**
   - B recomputes `commit_a`; if it does not match, B closes the connection immediately.
   - Both sides compute:
     `digest = blake3::hash("herdvia-pair-sas-v1" || tls_binding || id_A || id_B || nonce_A || nonce_B)`
     Using a BLAKE3 XOF reader seeded with that domain-separated input, draw `u32::from_le_bytes` until `v < 4_294_000_000` (`4294 * 1_000_000`) and take `code = v % 1_000_000`, formatted as `"{:03}-{:03}", code / 1000, code % 1000`.
   - Neither party nor an active MITM can bias or grind the 6-digit SAS: `tls_binding`, `id_A`, and `id_B` are fixed by the TLS 1.3 handshake before `commit_a` is sent, `nonce_B` is chosen without knowledge of `nonce_A`, and `nonce_A` is locked by `commit_a`.
6. **Human Confirmation & Atomic Order of Operations (`PAIR_DECISION`, tag `4` / `5`, B → A):**
   - A prints the target's display name, fingerprint, and `Verification code: NNN-NNN`, then waits (up to 60s) for B's decision.
   - B resolves the local peer name to save for `id_A`:
     - If `id_A` already exists in B's `peers` under `existing_name`: reuse `existing_name` (if `listen <name>` was explicitly given and `name != existing_name`, fail cleanly with `EndpointId ... is already registered as peer <existing_name>`). Permission becomes `both` if previously `out`, else `in`.
     - Else if `listen <name>` was given: use `<name>` (refusing if `<name>` is already taken by another `EndpointId`).
     - Else: use `caller_name`, or `<caller_name_truncated>-<first4_hex_of_id_A>` (or further hex digits) if `caller_name` is already taken by another `EndpointId`.
   - B prompts interactively (builder call: interactive progress and prompts go to `stdout` with flush; errors go to `stderr` prefixed with `herdvia:`):
     - Clearly states: approving grants `--inbound` remote command execution as this user account, shows peer name + fingerprint + `Verification code: NNN-NNN`, and reads one line from `stdin` (`y` / `yes` approves; anything else or EOF/60s timeout declines).
   - If approved:
     1. B writes `peers` (under the file lock) and calls `network.reload()` **first**.
     2. B sends `PAIR_APPROVED` (tag `4`) and waits briefly (up to 200ms / stream finish) so the frame is flushed before closing.
     3. A receives `PAIR_APPROVED`, writes B into A's `peers` under `<name>` (`out`, or `both` if `id_B` was already `in` under `<name>`), calls `network.reload()`, and waits up to 5 seconds for `status` to show B `connected` on `herdvia/1`.

### 6. Automatic `hc machine add` on Controller (Machine A)

After A's `peers` file and running agent are updated:

1. **Build OS-safe `--via` string (`via_command(exe: &Path, peer_name: &str)`):**
   - **Unix (`sh` rules):** `<sh_quoted_exe> via <peer_name>` (single-quoting `exe` if it contains anything outside `[A-Za-z0-9._/:@%+=,-]`, escaping `'` as `'\''`, identical to `scripts/install.sh`).
   - **Windows (`CommandLineToArgvW` rules):** `<argv_quoted_exe> via <peer_name>` (wrapping `exe` in `"..."` with standard Windows backslash/quote escaping when `exe` contains whitespace or `"`, otherwise bare `exe`).
2. **Locate `hc`:**
   - Search `PATH` first (`hc` on Unix, `hc.exe` on Windows).
   - If not on `PATH`: check `$HOME/.local/bin/hc` on Unix, or `%USERPROFILE%\AppData\Local\Programs\herdr-core\hc.exe` on Windows.
3. **Invoke `hc machine add --via <via_cmd> --name <name>`:**
   - Run with `Stdio::null()` for `stdin` (so `hc` never hangs on interactive `--install` prompts), capturing `stdout`/`stderr`, with a 15-second timeout.
   - **If `hc` succeeds (exit `0`):** Print completion line confirming both `herdvia` pairing and Herdr machine `<name>` registration, exit `0`.
   - **If `hc` is not found or exits non-zero:** Do **not** roll back `peers` (pairing succeeded!). Print that `herdvia` pairing with `<name>` succeeded, report why Herdr registration was not completed (missing `hc` or `hc`'s error message), print the exact ready-to-run `hc machine add --via ... --name <name>` command, and exit `1` (distinct from `255` usage/protocol failure, just as `doctor` uses `1`).

### 7. Peer Name & Conflict Rules on `connect <name>`

Before dialing on A, `connect` inspects A's existing `peers` file:
- If `<name>` is already registered to a peer:
  - If `<invite>` was supplied and its `target_endpoint_id` matches that peer's `id`, allow re-running (idempotent recovery, e.g. if a previous `hc machine add` failed or B revoked A and is re-pairing).
  - If `<name>` already has `outbound` permission and no `<invite>` was supplied, or if the discovered/invited `EndpointId` differs from `<name>`'s existing `EndpointId`, fail with a clear message telling the user to `herdvia revoke <name>` or pick another `<name>`.
  - If `<id_B>` is already registered on A under a *different* peer name `<other>`, fail with `EndpointId <id_B> is already registered as peer <other>; revoke it first or use that name` (matching `peers.rs`).

### 8. Logging, Docs, Install Scripts, and CI Updates

- **Agent logging (`src/log.rs`):** Pairing events on `herdvia/pair/1` log sanitized one-line lifecycle events (`pair approved: <name> (fingerprint <fp>)`, `pair rejected`, invalid attempt rate-limited under `Kind::Unauthenticated`) without ever logging `invite_secret`.
- **Install scripts (`scripts/install.sh`, `scripts/install.ps1`) & CI (`ci.yml`):**
  - Update the `Next steps` output at the end of `install.sh` and `install.ps1` to lead with the simple two-command workflow (`herdvia listen` on the remote PC, `herdvia connect <name>` on the Herdr PC), while keeping or updating any `ci.yml` smoke checks (`ci.yml` lines 220–238 check the printed commands from `install.sh` — update `ci.yml` alongside `install.sh` so CI exercises the newly printed commands too!).
- **Documentation:** Update `DESIGN.md`, `README.md`, `docs/README.ko.md`, `docs/README.ja.md`, and `docs/README.zh-CN.md` so the Quick Start leads with `herdvia listen` + `herdvia connect <name>`, while documenting `id` / `allow` / `pending` / `accept` as manual/non-interactive peer management commands.

## Left to the Builder

- Exact module layout (`src/pair.rs` and `src/discover.rs`, or submodules under `src/pair/`).
- Exact wording of interactive terminal status lines (keeping them concise, plain English in the binary to match every existing `herdvia` CLI message, and documented in English/Korean/Japanese/Chinese READMEs).
- Exact internal wire framing between the CLI (`listen` / `connect`) and the local `agent` over the `CONTROL` IPC stream.
