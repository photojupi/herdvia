# herdvia

A transport for attaching PCs behind different routers to Herdr as remote machines, without an SSH server or a VPN.

`hc machine add --via` normally takes a command such as `ssh`. herdvia is a drop-in for that command: it carries the bytes of an `hc` session between two of your machines over [iroh](https://www.iroh.computer) (peer-to-peer QUIC with NAT traversal, falling back to a relay server when a direct path is not possible). It does not interpret the Herdr protocol; it only moves bytes.

> **Unofficial.** herdvia is a personal project. It is not affiliated with, endorsed by, or supported by the Herdr / herdr-core project. It is experimental, and its commands, file formats, and wire protocol may change in incompatible ways.

## Requirements

- `hc` (herdr-core) on both machines. Install herdr-core on the remote machine yourself; herdvia does not install or update it.
- On the receiving side, herdvia runs only the `herdr-core connect` line that `hc` sends (with or without a session name), and only when it matches that line character for character. Every other command is rejected. This was verified with **hc 0.1.21**. If a future `hc` changes that line, herdvia will reject it until herdvia is updated.
- On the remote machine, herdvia looks for herdr-core where `hc`'s line looks for it:
  - macOS/Unix: `herdr-core` on the agent's `PATH`, or `~/.local/bin/herdr-core`. When the agent is started by `herdvia agent install` on macOS, its `PATH` is only `~/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin`, so a herdr-core installed elsewhere (for example by Homebrew) is not found; install or link it at `~/.local/bin/herdr-core`.
  - Windows: `%USERPROFILE%\AppData\Local\Programs\herdr-core\herdr-core.exe`

## Install

### Prebuilt binaries

Download from [GitHub Releases](https://github.com/photojupi/herdvia/releases):

- `herdvia-macos-arm64.zip` (Apple Silicon)
- `herdvia-windows-x64.zip`

Each archive contains the executable, this README, the license files (`LICENSE-MIT`, `LICENSE-APACHE`), and `THIRD-PARTY-LICENSES.html`. Put the executable in a stable location, for example `~/.local/bin/herdvia` on macOS or `%LOCALAPPDATA%\Programs\herdvia\herdvia.exe` on Windows. Automatic startup records the path of the executable you run it from, so do not leave it in a temporary or download folder.

The commands in this README assume that folder is on your `PATH`. If it is not, add it (and reopen your terminal and Herdr), or run the executable by its full path, for example in PowerShell: `& "$env:LOCALAPPDATA\Programs\herdvia\herdvia.exe" id`.

The binaries are not signed or notarized.

- **macOS:** Gatekeeper may refuse to run the file. Remove the quarantine attribute:
  ```sh
  xattr -d com.apple.quarantine ~/.local/bin/herdvia
  ```
- **Windows:** SmartScreen may show a warning the first time you run it.

### From source

Requires Rust 1.91 or later (required by the iroh dependency).

```sh
cargo install --locked --git https://github.com/photojupi/herdvia
```

## Quick start

Two machines: **A** dials (you run Herdr / `hc` here) and **B** is the machine you want to reach.

**1. Show each machine's identity.** Run on both A and B:

```sh
herdvia id
```

The first line is the machine's EndpointId (its public key). The second line is a short fingerprint: the first 16 characters of the EndpointId in groups of four. The secret key is created on first use.

**2. Allow each other.** Peers are never trusted automatically; each side must list the other.

On B, let A run sessions here:

```sh
herdvia allow A <EndpointId of A> --inbound
```

On A, let this machine dial B:

```sh
herdvia allow B <EndpointId of B> --outbound
```

- `--inbound`: that peer may open sessions **on this machine**.
- `--outbound`: this machine may connect **to that peer**.
- `--both`: both directions.

The name (`A`, `B`) is a local label: 1–32 characters of `A-Z a-z 0-9 . _ -`. Running `allow` again with the same name replaces the entry.

**3. Verify the fingerprints over a different channel.** For each EndpointId you pasted, check that its first 16 characters match the fingerprint that `herdvia id` prints on the other machine. Compare them in person or over a channel other than the one you used to copy the ID.

**4. Run the agent on both machines.** Either in the foreground:

```sh
herdvia agent
```

or register it to start automatically at login and start it now:

```sh
herdvia agent install
```

See [Automatic startup](#automatic-startup) for platform details.

**5. Add B to Herdr on A.**

```sh
hc machine add --via 'herdvia via B' --name B
```

`B` after `via` is the peer name you gave to `herdvia allow` on A. `--name` is the name Herdr shows; it can differ. `hc` appends its remote line to this command as one extra argument. If `hc` cannot find `herdvia` on its `PATH`, use the full path to the executable. On Windows, `hc` runs `--via` as a program and arguments without a shell, so quote the path if it contains spaces.

**6. Check the connection.** On A:

```sh
herdvia status
```

```text
name permission connection path rtt
B out connected direct 4.12ms
```

`path` is `direct` or `relay`. `status` requires the local agent to be running.

## Commands

| Command | Description |
|---|---|
| `herdvia id` | Print this machine's EndpointId and fingerprint. Creates the secret key if missing. |
| `herdvia allow <name> <EndpointId> (--inbound \| --outbound \| --both)` | Add a peer or replace its entry. A running agent applies the change immediately; otherwise it is applied when the agent starts. |
| `herdvia revoke <name>` | Remove a peer. A running agent closes connections to it immediately. |
| `herdvia status` | Show each peer's name, permission, connection state, direct/relay path, and round-trip time. Requires a running agent. |
| `herdvia agent` | Run the agent in the foreground. Stop it with Ctrl-C. |
| `herdvia agent install` | Register automatic startup at login and start the agent (macOS, Windows). |
| `herdvia agent uninstall` | Stop the automatically started agent and remove the registration (on Windows, any agent using the default data folder is stopped). Keeps the data folder and log. |
| `herdvia via <peer> <line>` | Called by `hc`; not meant to be run by hand. Requires a running local agent. |

On errors, herdvia prints one line starting with `herdvia:` to standard error and exits with code 255.

## Automatic startup

### macOS

`herdvia agent install` creates a per-user LaunchAgent (label `io.github.photojupi.herdvia.agent`) in `~/Library/LaunchAgents/` and starts it with `launchctl bootstrap`. launchd starts the agent at login and restarts it if it exits. Output goes to `~/Library/Logs/herdvia.log`.

- It refuses to install while `HERDVIA_DIR` is set; automatic startup always uses the default data folder.
- It refuses while an agent you started by hand is running. Stop that agent first.
- Running `install` again replaces the registration and restarts the agent with the current executable path.
- It warns, but still installs, if the executable is in a temporary folder or a `target` folder.

### Windows

`herdvia agent install` adds an entry to the current user's `Run` registry key (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) that starts `conhost.exe --headless "<path to herdvia.exe>" agent --log` at sign-in, without a console window. It then starts the agent the same way immediately. No administrator rights are needed, and none are used: no service or scheduled task is created.

- **Run it from a normal PowerShell window.** `install` refuses to run from an elevated (administrator) PowerShell.
- It refuses while `HERDVIA_DIR` is set.
- It refuses while an agent is running and automatic startup is not registered yet. Stop that agent first.
- The executable path must not contain `%`, and the whole registered command line must fit in 259 UTF-16 code units.
- If automatic startup is already registered, `install` stops any agent running on the default data folder, including one you started by hand, and starts the new one.
- If the agent exits, Windows does not restart it; it starts again at the next sign-in.
- Output goes to `%LOCALAPPDATA%\herdvia\herdvia.log`. If the agent does not come up within 5 seconds, `install` reports an error and keeps the registration.

### Linux

Not available. `agent install` and `agent uninstall` report that they are unsupported. Run `herdvia agent` yourself.

## Security model

- **A peer you allow with `--inbound` can open a herdr-core session on this machine as your user. In practice that is a shell.** Only allow devices you trust as much as yourself.
- Connections are authenticated by iroh with each machine's key pair. A connection is accepted only if the remote EndpointId is in the peer list with the matching direction. If the list is empty, every connection is rejected. Permission is checked again for every session, and changes to the list take effect immediately on a running agent.
- The command allowlist (only the exact `hc` connect lines) reduces misuse and makes anomalies visible: rejected lines are logged. It is not a privilege boundary, because `herdr-core connect` itself can run anything. The peer key list is the security boundary.
- herdvia refuses to run as root on macOS/Unix.
- The local channel between `hc`'s `via` process and the agent is restricted to your user: on macOS/Unix a socket with mode 0600 in a 0700 data folder, with the connecting user's UID checked; on Windows a named pipe whose access is granted only to your user SID, with remote clients rejected.
- **The secret key** is `secret.key` in the data folder. On macOS/Unix it is created with mode 0600, and herdvia refuses to start if its permissions are wider. On Windows it keeps the default permissions of your user profile; herdvia sets no extra ACL. Anyone who obtains this file can act as this machine toward peers that trust it. If it leaks, `revoke` this machine on every peer. There is no key rotation command; if you delete `secret.key`, a new identity is created on next use and peers must be allowed again.
- The fingerprint is only the first 16 characters of the EndpointId. Use it for comparing by eye; the full EndpointId is what is stored and checked.

**Reporting vulnerabilities:** please use GitHub's [private vulnerability reporting](https://github.com/photojupi/herdvia/security/advisories/new) for this repository, or open an issue if the problem is not sensitive.

## Network and published information

herdvia uses iroh's default `N0` preset and has no option to change it:

- It uses the public relay servers operated by [n0](https://n0.computer): each agent keeps a home relay, connections are set up through the relay, and traffic stays on the relay when a direct path cannot be established.
- It uses n0's DNS address lookup (`dns.iroh.link`) so peers can find each other by EndpointId. Each agent publishes a signed record containing its **EndpointId and home relay URL** to that service.
- iroh's port mapping is on: the agent asks your router (UPnP, PCP, or NAT-PMP, if the router supports them) to open a port mapping to help direct connections.
- Traffic is end-to-end encrypted; relays cannot read it. According to n0, relays can see connection metadata such as IP addresses, connection times, and the amount of data transferred.
- n0 describes its public relays as "suitable for **development and hobby use only**", with no SLA or uptime guarantee, and rate-limited. See [Public Relays](https://docs.iroh.computer/iroh-services/relays/public) and [DNS address lookup](https://docs.iroh.computer/connecting/dns-address-lookup) in the iroh documentation.

iroh's internal logs are off. The agent logs events such as connections, rejections, and disconnections, not session contents.

## Supported platforms

| Platform | Status |
|---|---|
| macOS, Apple Silicon | Tested on real hardware. Automatic startup via launchd. |
| Windows 11, x64 | Tested on real hardware. Automatic startup via the `Run` registry key. |
| Linux | Not tested. No automatic startup. |

When B is a Windows machine, `hc` first sends its Unix connect line. herdvia answers as `cmd` would for a missing `sh` (exit code 1), and `hc` then sends its Windows line, which herdvia runs directly without a shell.

## Configuration and files

| Item | Location |
|---|---|
| Data folder (macOS) | `~/Library/Application Support/herdvia` |
| Data folder (Windows) | `%APPDATA%\herdvia` |
| Data folder (Linux) | `$XDG_DATA_HOME/herdvia` (default `~/.local/share/herdvia`) |
| Override | `HERDVIA_DIR=<path>` replaces the data folder. Every command, including the `via` process that `hc` starts, must see the same value. Automatic startup refuses to install while it is set. |
| `secret.key` | This machine's 32-byte secret key. |
| `peers` | Peer list, one `name EndpointId permission` per line (`in`, `out`, or `both`). Edit it with `allow` and `revoke`. |
| `agent.sock` | Local socket on macOS/Unix (Windows uses a named pipe). |
| Log (macOS, automatic startup) | `~/Library/Logs/herdvia.log` |
| Log (Windows, automatic startup) | `%LOCALAPPDATA%\herdvia\herdvia.log` |
| Log (`herdvia agent` in the foreground) | standard error |

herdvia does not rotate its log files.

## Design

The design notes and measured `hc` behavior are in [DESIGN.md](https://github.com/photojupi/herdvia/blob/main/DESIGN.md) (in Korean).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
