# `herdvia listen` and `herdvia connect` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `herdvia listen` and `herdvia connect` so first-time users can pair two PCs over LAN auto-discovery or a single-use invite token with 6-digit commit-reveal SAS confirmation and automatic `hc machine add` registration.

**Architecture:** Only the running `herdvia agent` binds `secret.key` and registers a dedicated `herdvia/pair/1` ALPN isolated from `herdvia/1` command execution. Interactive `herdvia listen` and `herdvia connect` processes perform foreground UDP LAN discovery (avoiding background LaunchAgent LAN blocks), coordinate handshake state with their local agent over authenticated `CONTROL` IPC streams, verify a TLS-bound commit-reveal 6-digit SAS with human `[y/N]` approval on the target PC, update `peers` atomically, and register the peer in Herdr using `herdvia`'s exact executable path.

**Tech Stack:** Rust 2024 (`herdvia`), `iroh` (`~1.4.0`), `tokio` (`1`), `blake3` (`1.8`), `data-encoding` (`2.11`), `socket2` (`0.6`).

**Spec:** `docs/superpowers/specs/2026-11-15-listen-connect-pairing-design.md`

## Global Constraints

- Zero new packages outside the existing `Cargo.lock` dependency graph (`blake3 1.8.7`, `data-encoding 2.11.1`, `socket2 0.6.5` are already locked and clarified in `about.toml`); always verify with `--locked`.
- Write all Git commit messages in English (both subject and body; zero Hangul characters per `AGENTS.md`).
- Never allow `herdvia/pair/1` connections to reach `Network::serve_connection` or `exec::serve`, and never weaken `herdvia/1` authorization or wire framing.
- Never default to `--both`; preserve existing reverse permissions by upgrading `in` + `out` to `both` only when the same `EndpointId` already holds the opposite direction.
- Must pass `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, and compile cleanly for both Unix and Windows (`x86_64-pc-windows-gnu` / `cfg(windows)`).
- Product code must contain zero test-only flags or environment-variable backdoors.

## Review Focus

1. **Invite pasted with surrounding whitespace or uppercase letters** (`"  HVI1-... \r\n"` from chat or PowerShell) — `decode_invite` trims ASCII whitespace and accepts case-insensitive prefix and Base32 digits. (Tested in Task 1.)
2. **Caller OS hostname with `.local`, spaces, non-ASCII characters, or `> 32` bytes** (`"지민의 MacBook Pro (2).local"`) — `sanitize_hostname` produces a valid `1..=32` ASCII peer name accepted by `peers::valid_name`, falling back to `peer-<first8hex>` when no ASCII characters remain. (Tested in Task 1.)
3. **Bidirectional pairing when the reverse direction already exists** (e.g. B already allowed A as `out`, then B runs `listen` and approves A; or A already allowed B as `in` under `<name>`, then A runs `connect <name>`) — permission upgrades to `both` instead of clobbering the earlier direction or failing on duplicate `EndpointId`. (Tested in Task 2.)
4. **Executable path containing spaces and single quotes on Unix or spaces on Windows** (`/tmp/it's home/.local/bin/herdvia`, `C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe`) — `via_command` quotes the path so `sh` (Unix) and `CommandLineToArgvW` (Windows) invoke the exact binary with `via <name>`. (Tested in Task 5.)
5. **Empty Enter or EOF (`Ctrl-D`) at `herdvia listen`'s `[y/N]` prompt** — treated as safe rejection (`N`), sends `PAIR_REJECTED`, leaves `peers` untouched, and exits `1` without hanging. (Tested in Task 6.)

---

### Task 1: Dependencies, Invite Token, Commit-Reveal SAS, and Pairing Wire Codec

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock` (adds direct edges from `herdvia` to already-locked `blake3`, `data-encoding`, `socket2` only)
- Modify: `src/lib.rs`
- Create: `src/pair.rs`
- Create: `src/pair/codec.rs`

**Interfaces:**
- Produces in `crate::pair::codec`:
  - `pub const PAIR_ALPN: &[u8] = b"herdvia/pair/1";`
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum AuthKind { Invite = 1, Lan = 2 }`
  - `pub fn random_bytes<const N: usize>() -> [u8; N]` (derived via `iroh::SecretKey::generate().to_bytes()` + `blake3` XOF when `N > 32`)
  - `pub fn encode_invite(target: EndpointId, secret: &[u8; 16]) -> String`
  - `pub fn decode_invite(input: &str) -> io::Result<(EndpointId, [u8; 16])>`
  - `pub fn auth_proof(kind: AuthKind, token: &[u8; 16], tls_binding: &[u8; 32]) -> [u8; 32]`
  - `pub fn verify_auth_proof(kind: AuthKind, token: &[u8; 16], tls_binding: &[u8; 32], proof: &[u8; 32]) -> bool`
  - `pub fn compute_commit_a(tls_binding: &[u8; 32], id_a: EndpointId, id_b: EndpointId, nonce_a: &[u8; 32]) -> [u8; 32]`
  - `pub fn verify_commit_a(tls_binding: &[u8; 32], id_a: EndpointId, id_b: EndpointId, nonce_a: &[u8; 32], expected: &[u8; 32]) -> bool`
  - `pub fn derive_sas(tls_binding: &[u8; 32], id_a: EndpointId, id_b: EndpointId, nonce_a: &[u8; 32], nonce_b: &[u8; 32]) -> String`
  - `pub fn sanitize_hostname(raw: &str, fallback: EndpointId) -> String`
  - `#[derive(Debug, PartialEq, Eq)] pub enum PairMsg { Hello { auth_kind: AuthKind, auth_proof: [u8; 32], commit_a: [u8; 32], caller_name: String }, Challenge { nonce_b: [u8; 32], target_name: String }, Reveal { nonce_a: [u8; 32] }, Approved, Rejected { reason: String } }`
  - `pub async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut W, msg: &PairMsg) -> io::Result<()>`
  - `pub async fn read_msg<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<PairMsg>`

- [ ] **Step 1: Add direct dependencies to `Cargo.toml` and verify lockfile**

Add `blake3 = "1.8"`, `data-encoding = "2.11"`, and `socket2 = "0.6"` under `[dependencies]` in `Cargo.toml`. Run `cargo metadata --offline --format-version 1 > /dev/null` so `Cargo.lock` links `herdvia` to the existing cached versions (`1.8.7`, `2.11.1`, `0.6.5`) offline without downloading or adding any crate. Verify `git diff Cargo.lock` changes only the `herdvia` package's `dependencies` list.

- [ ] **Step 2: Write failing unit tests in `src/pair/codec.rs`**

Create `src/pair.rs` (`pub mod codec;`) and register `pub mod pair;` in `src/lib.rs`. Add unit tests in `src/pair/codec.rs` covering:
- `invite_roundtrips_82_chars_and_accepts_trimmed_case_insensitive_input`: encodes `(id, secret)` to an 82-char lowercase `hvi1-...` string; decodes `"  " + &encoded.to_uppercase() + "\r\n"` back to `(id, secret)`; rejects wrong prefix, wrong length, invalid Base32 characters, and tampered bytes.
- `auth_proof_binds_kind_token_and_tls_binding`: `verify_auth_proof` succeeds for matching `(kind, token, tls_binding)` and fails if `AuthKind` flips (`Invite` vs `Lan`), a bit in `token` flips, or a bit in `tls_binding` flips.
- `commit_and_sas_are_deterministic_and_sensitive_to_every_input`: `verify_commit_a` catches 1-bit changes in `tls_binding`, `id_a`, `id_b`, or `nonce_a`; `derive_sas` returns `NNN-NNN` (`len == 7`, ASCII digits with `-` at index 3) and changes when any of the 5 inputs changes or `id_a`/`id_b` swap roles.
- `sanitize_hostname_strips_local_suffix_non_ascii_and_bounds_length`: `"지민의 MacBook Pro (2).local"` -> `"MacBook-Pro-2"` (valid per `peers::valid_name`), `"한글만.local"` -> `"peer-<first8hex>"`, 60-char ASCII name truncates to 32 chars.
- `pair_messages_roundtrip_and_reject_oversized_or_invalid_frames`: every `PairMsg` variant round-trips via `write_msg` / `read_msg`; header with `len > 256`, unknown tag, or invalid `caller_name` fails with `InvalidData`.

- [ ] **Step 3: Run tests to verify failure**

Run: `cargo test --locked --lib pair::codec`
Expected: FAIL (unimplemented functions/types).

- [ ] **Step 4: Implement `src/pair/codec.rs`**

Use `data_encoding::BASE32_NOPAD` (lowercased on encode, `.trim()` + `.to_ascii_uppercase()` after stripping case-insensitive `hvi1-` prefix on decode). Use `blake3::Hash` equality for constant-time proof and commit comparisons. Match the spec's exact domain-separated hashes: `blake3::derive_key("herdvia-pair-invite-v1", secret)` / `blake3::derive_key("herdvia-pair-lan-v1", listen_id)` with `blake3::keyed_hash(&key, tls_binding)` for `auth_proof`; `blake3::Hasher::new()` fed `b"herdvia-pair-commit-a-v1" || tls_binding || id_a || id_b || nonce_a` for `compute_commit_a`; and `blake3::Hasher::new()` fed `b"herdvia-pair-sas-v1" || tls_binding || id_a || id_b || nonce_a || nonce_b` + `.finalize_xof()`, rejection-sampling little-endian `u32`s `< 4_294_000_000` (`% 1_000_000`) formatted as `"{:03}-{:03}"`, for `derive_sas`. Validate `peers::valid_name` when decoding `caller_name` and `target_name` in `read_msg`.

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test --locked --lib pair::codec`
Expected: PASS (all 5 unit tests pass).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/pair.rs src/pair/codec.rs
git commit -m "Add pairing invite token, commit-reveal SAS, and wire codec"
```

---

### Task 2: Peer Alias Resolution, Conflict Guards, and Bidirectional Permission Upgrades

**Files:**
- Modify: `src/peers.rs`

**Interfaces:**
- Produces in `crate::peers`:
  - `pub fn check_connect_target(dir: &Path, name: &str, known_target: Option<EndpointId>) -> io::Result<()>`
  - `pub fn resolve_inbound_peer(peers: &[Peer], explicit_name: Option<&str>, caller_name: &str, id: EndpointId) -> io::Result<Peer>` (pure helper used to preview the name before prompting and inside `grant_inbound`)
  - `pub fn grant_inbound(dir: &Path, explicit_name: Option<&str>, caller_name: &str, id: EndpointId) -> io::Result<Peer>`
  - `pub fn grant_outbound(dir: &Path, name: &str, id: EndpointId) -> io::Result<Peer>`

- [ ] **Step 1: Write failing unit tests in `src/peers.rs`**

Add unit tests in `src/peers.rs::tests`:
- `grant_inbound_disambiguates_colliding_caller_names_and_stays_within_32_chars`: when `"macbook"` is already taken by `id_1`, `grant_inbound(dir, None, "macbook", id_2)` saves `"macbook-<hex4_of_id2>"` with `Permission::In`; also test a 32-char `caller_name` collision so the disambiguated name remains `<= 32` chars and valid.
- `grant_inbound_and_outbound_upgrade_existing_reverse_permission_to_both`:
  - If `b` already has `Peer { name: "laptop", id: id_a, permission: Out }`, `grant_inbound(dir, None, "other-host", id_a)` and `grant_inbound(dir, Some("laptop"), "other-host", id_a)` both reuse `"laptop"` and upgrade permission to `Both`; `grant_inbound(dir, Some("wrong"), "other-host", id_a)` fails with `AlreadyExists` naming `"laptop"`; and `grant_inbound(dir, Some("laptop"), "other-host", id_c)` fails with `AlreadyExists` because `"laptop"` belongs to `id_a`.
  - If `a` already has `Peer { name: "home", id: id_b, permission: In }`, `check_connect_target(dir, "home", None)` succeeds and `grant_outbound(dir, "home", id_b)` upgrades `"home"` to `Both`.
- `check_connect_target_fails_fast_on_existing_outbound_or_conflicting_id`:
  - When `"home"` already has `Out` or `Both` for `id_1`: `check_connect_target(dir, "home", None)` fails (`AlreadyExists`), `check_connect_target(dir, "home", Some(id_1))` succeeds (idempotent re-run with matching invite), and `check_connect_target(dir, "home", Some(id_2))` fails (`AlreadyExists`).
  - When `id_1` is registered as `"office"`, `check_connect_target(dir, "home", Some(id_1))` and `grant_outbound(dir, "home", id_1)` fail (`AlreadyExists` naming `"office"`).

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test --locked --lib peers::tests`
Expected: FAIL (missing functions).

- [ ] **Step 3: Implement `check_connect_target`, `resolve_inbound_peer`, `grant_inbound`, and `grant_outbound` in `src/peers.rs`**

Run `grant_inbound` and `grant_outbound` inside `peers::update` (holding the `secret.key` file lock). For auto-disambiguation when `explicit_name` is `None` and `caller_name` is taken by another `EndpointId`, try suffixes of `id.to_string()` of length `4`, `8`, `12`, `16` (`<truncated_caller>-<hex>`) until unused.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --locked --lib peers::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/peers.rs
git commit -m "Add peer name resolution and bidirectional permission helpers for pairing"
```

---

### Task 3: Foreground UDP LAN Discovery (`src/discover.rs`)

**Files:**
- Create: `src/discover.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::peers::valid_name`, `crate::pair::codec::random_bytes`, `iroh::EndpointId`
- Produces in `crate::discover`:
  - `pub const DEFAULT_PORT: u16 = 48721;`
  - `pub const MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 86);`
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct BeaconInfo { pub listen_id: [u8; 16], pub endpoint_id: EndpointId, pub quic_port: u16, pub hostname: String }`
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub struct DiscoveredPeer { pub info: BeaconInfo, pub source_ip: IpAddr }` with `pub fn direct_addr(&self) -> Option<SocketAddr>` (`quic_port != 0` -> `Some(SocketAddr::new(self.source_ip, self.info.quic_port))`)
  - `pub fn encode_beacon(info: &BeaconInfo) -> Vec<u8>` and `pub fn decode_beacon(buf: &[u8]) -> Option<BeaconInfo>`
  - `pub fn encode_query(nonce: &[u8; 16]) -> [u8; 21]` and `pub fn is_query(buf: &[u8]) -> bool`
  - `pub fn default_targets(port: u16) -> Vec<SocketAddr>` (`239.255.77.86:port` and `255.255.255.255:port`)
  - `pub fn bind_discovery_socket(port: u16) -> io::Result<tokio::net::UdpSocket>` (with `socket2` `reuse_address(true)`, `reuse_port(true)` on `cfg(unix)`, `broadcast(true)`, and best-effort `join_multicast_v4`)
  - `pub async fn advertise_on(socket: tokio::net::UdpSocket, targets: Vec<SocketAddr>, info: BeaconInfo, stop: tokio::sync::watch::Receiver<bool>) -> io::Result<()>`
  - `pub async fn scan_on(listen_socket: Option<tokio::net::UdpSocket>, targets: Vec<SocketAddr>, self_id: EndpointId, duration: Duration) -> io::Result<Vec<DiscoveredPeer>>`

- [ ] **Step 1: Write failing unit tests in `src/discover.rs`**

Register `pub mod discover;` in `src/lib.rs` and write tests in `src/discover.rs::tests`:
- `beacon_and_query_packets_roundtrip_and_ignore_garbage`: tests `HDVB1` (`listen_id[16] || endpoint_id[32] || quic_port_be[2] || name_len[1] || hostname`) and `HDVD1` (`nonce[16]`), rejecting truncated packets, trailing bytes, invalid Ed25519 keys, and invalid hostnames (`!peers::valid_name`).
- `scan_discovers_listener_over_loopback_and_filters_self_id`: binds `listener_sock` on `127.0.0.1:0`, spawns `advertise_on` for `id_b` (and a second advertiser for `self_id`), runs `scan_on` targeting `listener_sock.local_addr()` for `300ms`, and asserts `scan_on` returns only `id_b` with `direct_addr() == Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345))` in `< 350ms`.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test --locked --lib discover::tests`
Expected: FAIL.

- [ ] **Step 3: Implement `src/discover.rs`**

Use `socket2::{Domain, Protocol, Socket, Type}` to configure `reuse_address(true)`, `#[cfg(unix)]` `SO_REUSEPORT` (ignoring `ENOPROTOOPT`), `broadcast(true)`, nonblocking mode, and best-effort `join_multicast_v4(&MULTICAST_V4, &Ipv4Addr::UNSPECIFIED)`. In `advertise_on`, send `encode_beacon(&info)` to each target immediately and every `500ms` (ignoring per-target `send_to` errors when e.g. broadcast is unreachable on an offline host) AND reply directly via `socket.send_to(&beacon_bytes, peer_addr)` whenever `is_query(pkt)` arrives. In `scan_on`, open an ephemeral query socket (`0.0.0.0:0` with `broadcast(true)`), send `HDVD1` queries to `targets` at `0ms`, `400ms`, and `900ms`, read replies from both the ephemeral query socket (receives unicast replies!) and `listen_socket` (if bound, receives multicast/broadcast beacons!), deduplicate by `EndpointId` (excluding `self_id`), and return once `duration` elapses.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --locked --lib discover::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/discover.rs
git commit -m "Add UDP LAN discovery beacon and scanner for pairing"
```

---

### Task 4: Agent Pairing Gate, `herdvia/pair/1` Session Handler, and Local IPC Wiring

**Files:**
- Modify: `src/frame.rs`
- Modify: `src/network.rs`
- Modify: `src/agent.rs`
- Modify: `src/pair.rs`
- Create: `src/pair/session.rs`

**Interfaces:**
- Consumes: Task 1 (`pair::codec`), Task 2 (`peers::resolve_inbound_peer`, `peers::grant_inbound`, `peers::grant_outbound`), `frame`, `network::Network`, `ipc`
- Produces:
  - In `src/frame.rs`: `CONTROL` accepts `b"listen"` and `b"connect"` in addition to `b"reload"`, `b"status"`, `b"pending"`, `b"doctor"`.
  - In `src/network.rs`:
    - `Network` registers `PAIR_ALPN` alongside `ALPN`, exposes `pub fn dir(&self) -> &Path`, `pub fn quic_port(&self) -> u16`, `pub fn set_addr_hint(&self, id: EndpointId, addr: SocketAddr)` (used by `Network::outbound` when building `EndpointAddr` so post-pairing `herdvia/1` connects immediately over LAN even offline without relay/DNS), and `pub(crate) pairing: Arc<crate::pair::session::PairingGate>`.
    - `Policy::after_handshake` routes `PAIR_ALPN` exclusively through `self.pairing.allow_handshake(conn.side(), conn.remote_id())` and never records `PAIR_ALPN` rejections in `pending`.
  - In `src/pair::session`:
    - Local IPC protocol over `CONTROL` `"listen"` stream:
      - CLI sends 1 `STDIN` config line: `explicit_name` (empty or valid peer name).
      - Agent replies with `STDOUT` line: `READY <endpoint_id> <quic_port> <listen_id_hex> <invite_token> <hostname>\n`.
      - When a verified caller completes commit-reveal, agent sends `STDOUT` line: `PROMPT <local_peer_name> <caller_id> <caller_fingerprint> <sas>\n` and waits up to 60s for 1 `STDIN` line (`"approve"` or `"reject"`).
      - On `"approve"`, agent runs `peers::grant_inbound` + `network.reload()`, sends `PairMsg::Approved` to caller, writes `STDOUT` `APPROVED <local_peer_name> <caller_id> <permission>\n` + `EXIT 0`.
      - On `"reject"`, timeout, or 5 failed `auth_proof`s, agent writes `ERROR <reason>` (or `STDOUT` `REJECTED\n` + `EXIT 1` for user rejection/timeout).
    - Local IPC protocol over `CONTROL` `"connect"` stream:
      - CLI sends 1 `STDIN` request line: `<local_peer_name> <target_id> <invite|lan> <token_hex_32> [<direct_socket_addr>]`.
      - Agent dials `target_id` (adding `direct_socket_addr` to `EndpointAddr` if provided) with `PAIR_ALPN` (15s connect/handshake timeout), sends `STDOUT` `SAS <target_name> <target_fingerprint> <sas>\n` once commit-reveal finishes, waits up to 65s for `PairMsg::Approved` / `PairMsg::Rejected`, on approval runs `peers::grant_outbound` + `network.reload()`, and writes `STDOUT` `APPROVED <target_id> <permission>\n` + `EXIT 0` (or `STDOUT` `REJECTED <reason>\n` + `EXIT 1` if remote declined, or `ERROR <reason>` on transport/auth error).

- [ ] **Step 1: Write failing unit/in-process tests in `src/frame.rs` and `src/pair/session.rs`**

1. Update `accepts_only_reload_status_pending_and_doctor_controls` in `src/frame.rs` (renaming or adding test) to assert `b"listen"` and `b"connect"` are accepted `CONTROL` payloads while invalid payloads are still rejected, and confirm `exec::serve` still rejects `CONTROL` on `herdvia/1`.
2. In `src/pair/session.rs::tests`, write async tests using two loopback `iroh::Endpoint`s (`presets::Minimal`) or `Network` instances in `tempfile::tempdir()`s:
   - `pairing_over_lan_token_computes_matching_sas_grants_permissions_and_never_touches_pending`: runs `listen` IPC and `connect` IPC over `tokio::io::duplex`, verifies both sides emit the exact same `sas`, approves on B, verifies B's `peers` has A as `in`, A's `peers` has B as `out`, and B's `pending` is empty.
   - `five_wrong_proofs_abort_listen_session_and_unpaired_pair_alpn_is_rejected`: verifies `PAIR_ALPN` is rejected with `403` when no `listen` session is active; when active, 5 wrong `auth_proof` attempts terminate the `listen` session with an error.
   - `dropping_local_ipc_cancels_active_listen_immediately`: dropping the local duplex stream immediately clears `PairingGate` so a subsequent `PAIR_ALPN` dial is rejected.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test --locked --lib frame::tests pair::session::tests`
Expected: FAIL.

- [ ] **Step 3: Implement `PairingGate`, `src/pair/session.rs`, and wire `src/frame.rs`, `src/network.rs`, `src/agent.rs`**

Key details:
- Extract TLS binding via `conn.export_keying_material(&mut tls_binding, b"herdvia-pair-v1", &[]).map_err(|_| frame::invalid("TLS exporter failed"))?`.
- Limit `ListenSession` lifetime to `Duration::from_secs(300)`, concurrent server `PAIR_ALPN` streams to `4`, active interactive prompt to `1` (`tokio::sync::Mutex::try_lock`), step read/write timeouts to `15s`, and human approval wait to `60s`.
- Log sanitized lifecycle lines via `log::line` (`Kind::Unauthenticated` for invalid pairing proofs/handshakes, `Kind::Other` for approved/declined pairing; never log tokens).
- In `agent.rs`, watch local IPC disconnect via `ipc::watch_disconnect(&stream)` on `"listen"` and `"connect"` just like `Request::Open` does!

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --locked --lib`
Expected: PASS (all existing + new unit tests pass).

- [ ] **Step 5: Commit**

```bash
git add src/frame.rs src/network.rs src/agent.rs src/pair.rs src/pair/session.rs
git commit -m "Implement herdvia/pair/1 agent session handler and local IPC control"
```

---

### Task 5: Herdr `hc machine add` Integration, OS Quoting, and `herdr-core` Preflight Helper

**Files:**
- Modify: `src/doctor.rs`
- Modify: `src/pair.rs`
- Create: `src/pair/hc.rs`

**Interfaces:**
- Produces:
  - In `src/doctor.rs`: `pub(crate) async fn core_preflight_warning() -> Option<String>` (runs `ipc::home_dir` + `find_core` + `core_version_of` + `core_check`; returns `Some(check.text)` if `check.level != Level::Ok`, else `None`).
  - In `src/pair::hc`:
    - `pub fn sh_via_command(exe: &Path, peer: &str) -> String`
    - `pub fn windows_via_command(exe: &Path, peer: &str) -> String`
    - `pub fn via_command_for_current_os(exe: &Path, peer: &str) -> String`
    - `pub fn manual_hc_add_line(exe: &Path, peer: &str) -> String` (ready-to-paste shell/PowerShell `hc machine add --via ... --name <peer>` command)
    - `#[derive(Debug, PartialEq, Eq)] pub enum HcResult { Added, MissingHc { manual_cmd: String }, Failed { reason: String, manual_cmd: String } }`
    - `pub async fn register_machine(exe: &Path, peer: &str) -> HcResult`

- [ ] **Step 1: Write failing unit tests in `src/pair/hc.rs`**

Add unit tests in `src/pair/hc.rs::tests`:
- `sh_via_command_quotes_spaces_and_single_quotes_for_posix_sh`: tests `/Users/you/.local/bin/herdvia` -> `/Users/you/.local/bin/herdvia via home` and `/tmp/it's home/.local/bin/herdvia` -> `'/tmp/it'\''s home/.local/bin/herdvia' via home`. On `#[cfg(unix)]`, executes `sh -c "set -- <via_cmd>; printf '%s\n%s\n%s' \"$1\" \"$2\" \"$3\""` and asserts `$1` equals the exact path, `$2 == "via"`, `$3 == "home"`.
- `windows_via_command_quotes_spaces_and_backslashes_for_command_line_to_argv_w`: tests `C:\Prog\herdvia.exe` -> `C:\Prog\herdvia.exe via home`, `C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe` -> `"C:\Users\Jane Doe\AppData\Local\Programs\herdvia\herdvia.exe" via home`, and trailing-backslash/quote edge cases per Win32 `CommandLineToArgvW` rules.
- `manual_hc_add_line_is_pasteable`: verifies `manual_hc_add_line` produces valid quoting for the target platform.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test --locked --lib pair::hc::tests`
Expected: FAIL.

- [ ] **Step 3: Implement `core_preflight_warning` in `src/doctor.rs` and `src/pair/hc.rs`**

In `src/pair/hc.rs`, locate `hc` on `env::var_os("PATH")` first (`hc` on Unix with executable bit check; `hc.exe` on Windows), falling back to `ipc::home_dir()` + `.local/bin/hc` (Unix) or `AppData\Local\Programs\herdr-core\hc.exe` (Windows). Run `tokio::process::Command::new(hc_path).args(["machine", "add", "--via", &via_cmd, "--name", peer]).stdin(Stdio::null())` with a 15-second timeout; sanitize stderr/stdout first non-empty line for `HcResult::Failed`.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --locked --lib pair::hc::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/doctor.rs src/pair.rs src/pair/hc.rs
git commit -m "Add OS-safe hc machine add runner and herdr-core preflight helper"
```

---

### Task 6: Interactive `herdvia listen` & `herdvia connect` CLI Commands (`src/pair/cli.rs`, `src/main.rs`)

**Files:**
- Create: `src/pair/cli.rs`
- Modify: `src/pair.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: Tasks 1–5 (`pair::codec`, `peers::check_connect_target`, `discover`, `pair::session`, `pair::hc`, `doctor::core_preflight_warning`)
- Produces:
  - In `src/pair/cli.rs`:
    - `pub async fn run_listen(explicit_name: Option<String>) -> io::Result<i32>`
    - `pub async fn run_connect(name: String, invite: Option<String>) -> io::Result<i32>`
  - In `src/main.rs`:
    - `Command::Listen(Option<String>)` (`herdvia listen` and `herdvia listen <name>`)
    - `Command::Connect { name: String, invite: Option<String> }` (`herdvia connect <name>` and `herdvia connect <name> <invite>`)
    - Updated usage string including `listen [<name>]` and `connect <name> [<invite>]`.

- [ ] **Step 1: Write failing CLI parser and prompt tests in `src/main.rs` and `src/pair/cli.rs`**

1. In `src/main.rs::tests`, add `parses_listen_and_connect_commands`:
   - `["listen"]` -> `Listen(None)`
   - `["listen", "laptop"]` -> `Listen(Some("laptop"))`
   - `["listen", "bad/name"]` and `["listen", "a", "b"]` -> `Err`
   - `["connect", "home"]` -> `Connect { name: "home", invite: None }`
   - `["connect", "home", "hvi1-abc"]` -> `Connect { name: "home", invite: Some("hvi1-abc") }`
   - `["connect"]`, `["connect", "bad/name"]`, `["connect", "a", "b", "c"]` -> `Err`
2. In `src/pair/cli.rs::tests`, add pure unit tests for:
   - `parse_yes_no_accepts_only_y_or_yes_and_rejects_empty_or_other`: `""`, `"\n"`, `"n"`, `"no"`, `"ye"` -> `false`; `"y"`, `"Y"`, `" yes \r\n"` -> `true`.
   - `parse_peer_selection_handles_single_choice_numbers_and_zero_for_invite`: tests user selection parsing when multiple LAN peers are discovered.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test --locked --bins`
Expected: FAIL.

- [ ] **Step 3: Implement `src/pair/cli.rs` and wire into `src/main.rs`**

Key behavior to pin:
- **Preflight agent check (both commands):** Connect to local IPC with `"doctor"` (3s timeout). If `NotFound` / `ConnectionRefused`: return `Err` (`agent is not running; start it with 'herdvia agent install' (or 'herdvia agent' in a terminal)` -> exits `255`). If `"listen"` / `"connect"` `CONTROL` fails because the running agent does not recognize it while `"doctor"` or `"status"` succeeds: return `Err` (`the running agent is older than this herdvia executable; restart it with 'herdvia agent install' (or restart 'herdvia agent')` -> exits `255`).
- **`run_listen`:**
  1. Run `doctor::core_preflight_warning().await`; if `Some(warn)`, write `herdvia: warning: {warn}\n` to `stderr`.
  2. Open `"listen"` IPC stream, receive `READY <id> <quic_port> <listen_id> <invite> <hostname>`.
  3. Spawn foreground UDP advertiser (`discover::advertise_on` on `DEFAULT_PORT`, ignoring bind error with a stderr note if UDP port is blocked so invite pairing still works!).
  4. Print concise instructions to `stdout` (flushed): waiting message (5m), same-network command (`herdvia connect <name>`), and different-network invite token (`hvi1-...`).
  5. When `PROMPT <peer_name> <caller_id> <fp> <sas>` arrives: stop LAN advertiser, print incoming peer name, fingerprint, `Verification code: <sas>`, and prompt `Check that the other PC shows <sas>. Approving lets that PC run commands as this user (--inbound). Allow? [y/N]: ` (flushed). Read one line from `stdin` on a blocking thread; empty line or EOF sends `"reject"`.
  6. Print completion line on `APPROVED` and return `Ok(0)`; on decline or timeout print status and return `Ok(1)`.
- **`run_connect`:**
  1. Decode `<invite>` early if supplied (invalid invite -> `Err` -> `255`), then call `peers::check_connect_target(&dir, &name, invite_target)`.
  2. Query local agent `"doctor"` + `peers::load_key(&dir)` (read-only / existing key via `id` from agent or `load_key`).
  3. If no `<invite>` was passed: run `discover::scan_on` (`DEFAULT_PORT`, `2s`).
     - 1 peer found: announce discovered peer and select it.
     - `> 1` peers found: list numbered choices (`1..=N`) plus `0) Paste an invite token instead`, read selection from `stdin` (EOF -> return `Ok(1)`).
     - 0 peers found (or `0` chosen): print `No waiting PC found on the local network.` and prompt `Paste the invite token from the other PC: `; read line from `stdin` (EOF/empty -> return `Ok(1)`), decode invite, and re-verify `peers::check_connect_target(&dir, &name, Some(target_id))`.
  4. Open `"connect"` IPC stream, print `Verification code: <sas>` + `Waiting for approval on the other PC...` as soon as `SAS` arrives.
  5. On `REJECTED`: print message to `stderr` and return `Ok(1)`.
  6. On `APPROVED`: poll `control::request(&dir, "status")` every `100ms` up to `5s` until `<name>` shows `connected` (best-effort so `hc` connects on first try), then call `hc::register_machine(&current_exe, &name)`:
     - `HcResult::Added`: print completion line and return `Ok(0)`.
     - `HcResult::MissingHc { manual_cmd }` / `HcResult::Failed { reason, manual_cmd }`: print that `herdvia` pairing succeeded, print why `hc machine add` was not completed to `stderr` along with `manual_cmd`, and return `Ok(1)`.

- [ ] **Step 4: Run unit and binary tests to verify pass**

Run: `cargo test --locked --lib --bins`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/pair.rs src/pair/cli.rs src/main.rs
git commit -m "Add interactive herdvia listen and connect CLI commands"
```

---

### Task 7: End-to-End Integration Tests, Install Scripts, CI Checks, and Documentation

**Files:**
- Create: `tests/pair.rs`
- Modify: `tests/common/mod.rs` (only if a small helper for spawning `herdvia` with piped stdin/stdout is helpful)
- Modify: `scripts/install.sh`
- Modify: `scripts/install.ps1`
- Modify: `.github/workflows/ci.yml`
- Modify: `DESIGN.md`
- Modify: `README.md`
- Modify: `docs/README.ko.md`
- Modify: `docs/README.ja.md`
- Modify: `docs/README.zh-CN.md`

**Interfaces:**
- Consumes: Tasks 1–6 (`herdvia listen`, `herdvia connect`)
- Produces: End-to-end binary tests (both offline/loopback for PR CI and relay-backed for `integration.yml`), updated installer `Next steps` + `ci.yml` assertions, and updated design + 4-language READMEs.

- [ ] **Step 1: Write end-to-end integration tests in `tests/pair.rs`**

Create `#![cfg(unix)]` `tests/pair.rs` using `mod common;` with:
- `listen_and_connect_fail_without_running_agent`: runs `herdvia listen` and `herdvia connect home` without an agent -> exit `255` with single `herdvia:` stderr line.
- `lan_discovery_pairs_two_local_agents_registers_hc_and_passes_traffic` *(no public relay needed — uses LAN UDP discovery + direct `127.0.0.1:<quic_port>` `EndpointAddr` + fake `hc` script in Node A's `PATH`!)*:
  - Starts Node B (`start_agent`) and spawns `herdvia listen` with piped stdin/stdout.
  - Installs a tiny executable `hc` script in Node A's `bin` dir that records `$@` to `@ROOT@/hc_args` and runs `"$4"` (or `sh -c "$4 'sh -c ...'"`) to verify the `--via` command actually works!
  - Starts Node A (`start_agent`) and runs `herdvia connect home`.
  - Reads the `Verification code: NNN-NNN` printed by both A and B, asserts **both codes are identical**, writes `"y\n"` to B's `listen` stdin, and asserts both `listen` and `connect` exit `0` within a few seconds (nowhere near 30s!).
  - Asserts A's `peers` has `home <b_id> out`, B's `peers` has `<sanitized_host> <a_id> in`, `@ROOT@/hc_args` recorded `machine add --via <exe> via home --name home`, and `a.run_via("home", &session_line("cat"), b"paired")` echoes `b"paired"` with exit `0`!
- `declining_or_eof_on_listen_leaves_peers_empty_and_exits_one`: repeats LAN connect, closes B's stdin (`EOF`) at `[y/N]`, asserts both B and A exit `1` and both `peers` files remain empty.
- `connect_refuses_duplicate_peer_name_before_dialing`: pre-populates `home` with `Out` on A, runs `herdvia connect home`, asserts immediate exit `255` without needing B.
- `invite_token_pairs_two_agents_over_relay`: spawns `herdvia listen laptop` on B, parses the printed `hvi1-...` line from B's stdout, runs `herdvia connect home <invite>` on A (skipping LAN scan), verifies matching SAS on both stdout streams, sends `"y\n"` to B, and verifies end-to-end `run_via` traffic! (This test uses n0 lookup, so omit it from `ci.yml`'s offline `--exact` list so `integration.yml` runs it while `ci.yml` runs the 4 offline tests above!)

- [ ] **Step 2: Run integration tests locally to verify pass**

Run: `cargo test --locked --test pair`
Expected: PASS (all 5 integration tests pass, including real n0 relay invite test and sub-3-second offline LAN pairing test).

- [ ] **Step 3: Update `scripts/install.sh`, `scripts/install.ps1`, and `.github/workflows/ci.yml`**

1. Update the `Next steps` block at the bottom of `scripts/install.sh` and `scripts/install.ps1` so newcomers see the simple two-command workflow first:
   - Remote PC (to accept connections here): `<run> listen`
   - Controller PC (where Herdr runs, to pair and add to Herdr): `<run> connect <name>`
   - Keep a one-line pointer to the README for manual `allow` / `pending` / `accept` commands.
2. In `.github/workflows/ci.yml`:
   - Add the 4 offline `pair` tests (`listen_and_connect_fail_without_running_agent`, `lan_discovery_pairs_two_local_agents_registers_hc_and_passes_traffic`, `declining_or_eof_on_listen_leaves_peers_empty_and_exits_one`, `connect_refuses_duplicate_peer_name_before_dialing`) to the `Integration tests (no connection)` step.
   - Update lines 220–238 of `install-sh` job to verify the newly printed `<run> listen` and `<run> connect <name>` lines from `install.sh` (running `<run> listen` without an agent and checking exit `255` and `^herdvia: `, just like `pending` did previously).

- [ ] **Step 4: Update `DESIGN.md` and all four READMEs (`README.md`, `docs/README.ko.md`, `docs/README.ja.md`, `docs/README.zh-CN.md`)**

- Document `listen` / `connect`, `herdvia/pair/1`, `hvi1-` invite format, LAN UDP discovery, commit-reveal 6-digit SAS, and automatic `hc machine add` in `DESIGN.md` (in Korean, matching `DESIGN.md`'s existing style).
- Update Quick Start, Commands table, Troubleshooting, and Security Model in `README.md`, `docs/README.ko.md`, `docs/README.ja.md`, and `docs/README.zh-CN.md` so the newcomer Quick Start is the simple 2-step `herdvia listen` + `herdvia connect <name>` flow, while preserving manual `id` / `allow` / `pending` / `accept` documentation in a dedicated manual-pairing subsection.

- [ ] **Step 5: Run full local verification suite**

Run:
- `cargo fmt --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked --lib --bins`
- `cargo test --locked --test doctor --test peers --test pending --test transport --test pair`
Expected: PASS with zero warnings.

- [ ] **Step 6: Commit**

```bash
git add tests/ scripts/install.sh scripts/install.ps1 .github/workflows/ci.yml DESIGN.md README.md docs/README.ko.md docs/README.ja.md docs/README.zh-CN.md
git commit -m "Add end-to-end pairing tests and update install scripts and documentation"
```
