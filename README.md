# herdvia

A transport for attaching PCs behind different routers to Herdr as remote machines, without an SSH server or a VPN.

`hc machine add --via` normally takes a command such as `ssh`. herdvia is a drop-in for that command: it carries the bytes of an `hc` session between two of your machines over [iroh](https://www.iroh.computer) (peer-to-peer QUIC with NAT traversal, falling back to a relay server when a direct path is not possible). It does not interpret the Herdr protocol; it only moves bytes.

> **Unofficial.** herdvia is a personal project. It is not affiliated with, endorsed by, or supported by the Herdr / herdr-core project. It is experimental, and its commands, file formats, and wire protocol may change in incompatible ways.

## Requirements

- herdr-core on both machines. Install it on each machine yourself with Herdr's install script. These scripts are part of Herdr, not herdvia. herdvia's own [install script](#install-script) does not install herdr-core; it only prints these commands when herdr-core is missing. Running a script again updates herdr-core.
  - macOS/Linux:
    ```sh
    curl -fsSL https://releases.herdr.dev/core/install.sh | sh
    ```
    It installs `~/.local/bin/herdr-core`, and an `hc` link beside it if there is no `hc` yet. It does not change your `PATH`; if `~/.local/bin` is not on it, the script says so.
  - Windows (PowerShell):
    ```powershell
    powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
    ```
    It installs `%LOCALAPPDATA%\Programs\herdr-core\herdr-core.exe`, and `hc.exe` beside it if there is no `hc` yet, and adds that folder to your user `PATH`. No administrator rights are needed.
- Use the same herdr-core version on both machines. The scripts install the latest version unless `HC_VERSION` is set. To install 0.1.21, the version herdvia was verified with:
  ```sh
  curl -fsSL https://releases.herdr.dev/core/install.sh | HC_VERSION=0.1.21 sh
  ```
  ```powershell
  $env:HC_VERSION = "0.1.21"; powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
  ```
  On macOS the script does not replace a newer herdr-core with an older one; it stops with "... is older than ...; use hc update --from with --force to downgrade".
- The receiving machine needs only herdr-core. It does not need the `hc` link or the Herdr app.
- On the receiving side, herdvia runs only the `herdr-core connect` line that `hc` sends (with or without a session name), and only when it matches that line character for character. Every other command is rejected. This was verified with **hc 0.1.21**. If a future `hc` changes that line, herdvia will reject it until herdvia is updated.
- Because of this, `hc machine add --install` and `hc machine update` cannot install or update herdr-core on the remote machine through herdvia. herdvia does not install or update it either.
- On the remote machine, herdvia looks for herdr-core where `hc`'s line looks for it:
  - macOS/Unix: `herdr-core` on the agent's `PATH`, or `~/.local/bin/herdr-core`. When the agent is started by `herdvia agent install` on macOS, its `PATH` is only `~/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin`, so a herdr-core installed elsewhere (for example by Homebrew) is not found; install or link it at `~/.local/bin/herdr-core`.
  - Windows: `%USERPROFILE%\AppData\Local\Programs\herdr-core\herdr-core.exe`

## Install

### Install script

macOS (Apple Silicon):

```sh
curl -fsSL https://github.com/photojupi/herdvia/releases/latest/download/install.sh | sh
```

Windows (x64), in a normal PowerShell window, not an administrator one:

```powershell
powershell -c "irm https://github.com/photojupi/herdvia/releases/latest/download/install.ps1 | iex"
```

The script:

1. Downloads the release archive and `SHA256SUMS` into a temporary folder and checks the archive. If the download or the check fails, it stops and changes nothing.
2. Puts the executable at `~/.local/bin/herdvia` on macOS, or `%LOCALAPPDATA%\Programs\herdvia\herdvia.exe` on Windows (with the README and license files beside it).
3. Puts that folder on your `PATH` if it is not there yet. On macOS it adds `export PATH="$HOME/.local/bin:$PATH"` to `~/.zprofile` when your login shell is zsh; for another shell it only prints the line to add. On Windows it adds the folder to your user `PATH`.
4. Checks for herdr-core. **It does not install herdr-core.** If herdr-core is missing, it prints Herdr's install commands (see [Requirements](#requirements)). On macOS, if herdr-core is on your `PATH` but not at `~/.local/bin/herdr-core`, it prints an `ln -s` command, because the automatically started agent does not search elsewhere.
5. Prints this machine's EndpointId and fingerprint (`herdvia id`, which creates the secret key on first use).
6. Runs `herdvia agent install`, which starts the agent now and at every login (see [Automatic startup](#automatic-startup)). On Windows, from an administrator PowerShell, it skips this step and tells you to run it from a normal window.

It does not add peers or run `hc machine add`; continue with the [Quick start](#quick-start). Open a new terminal before you run `herdvia` by name (on Windows, a new PowerShell window, and restart Herdr if it is running).

The macOS script runs only on Apple Silicon; on other systems it changes nothing and points to [From source](#from-source). It refuses to run as root. The Windows script runs only on x64 and needs `curl.exe` in `System32` (Windows 10 1803 or later).

Options are environment variables. Only the value `1` turns a switch on.

| Variable | Effect |
|---|---|
| `HERDVIA_VERSION` | Install this release, for example `0.1.0` or `v0.1.0`, instead of the latest. |
| `HERDVIA_NO_AGENT=1` | Do not run `herdvia agent install`. |
| `HERDVIA_NO_MODIFY_PATH=1` | Do not change `~/.zprofile` or the user `PATH`. |
| `HERDVIA_RELEASE_BASE` | Download each file from `<url>/<file>` instead of GitHub Releases. Takes precedence over `HERDVIA_VERSION`. |

To install a specific version, set `HERDVIA_VERSION`. The script installs the latest release unless it is set, even when the script itself is downloaded from a version's URL:

```sh
curl -fsSL https://github.com/photojupi/herdvia/releases/download/v0.1.0/install.sh | HERDVIA_VERSION=0.1.0 sh
```

```powershell
$env:HERDVIA_VERSION = "0.1.0"; powershell -c "irm https://github.com/photojupi/herdvia/releases/download/v0.1.0/install.ps1 | iex"
```

To read the script before running it, download it first. `SHA256SUMS` lists the scripts too.

```sh
curl -fsSLO https://github.com/photojupi/herdvia/releases/latest/download/install.sh
less install.sh
sh install.sh
```

```powershell
curl.exe -fLO https://github.com/photojupi/herdvia/releases/latest/download/install.ps1
notepad install.ps1
Get-Content -Raw .\install.ps1 | Invoke-Expression
```

Running the script again upgrades herdvia; see [Upgrading](#upgrading).

### Manual install

Download from [GitHub Releases](https://github.com/photojupi/herdvia/releases):

- `herdvia-macos-arm64.zip` (Apple Silicon)
- `herdvia-windows-x64.zip`
- `install.sh` and `install.ps1` (the [install scripts](#install-script))
- `SHA256SUMS` (SHA-256 checksums of the archives and the scripts)

Each archive contains the executable, this README, the license files (`LICENSE-MIT`, `LICENSE-APACHE`), and `THIRD-PARTY-LICENSES.html`, with no enclosing folder. Put the executable in a stable location, for example `~/.local/bin/herdvia` on macOS or `%LOCALAPPDATA%\Programs\herdvia\herdvia.exe` on Windows. Automatic startup records the path of the executable you run it from, so do not leave it in a temporary or download folder.

The commands in this README assume that folder is on your `PATH`. If it is not, add it (and reopen your terminal and Herdr), or run the executable by its full path, for example in PowerShell: `& "$env:LOCALAPPDATA\Programs\herdvia\herdvia.exe" id`.

The binaries are not signed or notarized. There is no build for Intel Macs; build from source instead (see [From source](#from-source)).

#### macOS (Apple Silicon)

These commands download the archive and `SHA256SUMS` into a temporary folder, check the archive, and move the executable to `~/.local/bin/herdvia`:

```sh
mkdir -p ~/.local/bin
cd "$(mktemp -d)"
curl -fLO https://github.com/photojupi/herdvia/releases/latest/download/herdvia-macos-arm64.zip &&
curl -fLO https://github.com/photojupi/herdvia/releases/latest/download/SHA256SUMS &&
shasum -a 256 -c --ignore-missing SHA256SUMS &&
unzip -q herdvia-macos-arm64.zip -d herdvia &&
mv herdvia/herdvia ~/.local/bin/herdvia
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile
```

The last line puts `~/.local/bin` on your `PATH` in zsh, the default shell on macOS. Skip it if `~/.local/bin` is already on your `PATH` (for example because you added it for herdr-core). If `curl` prints an error or `shasum` prints `FAILED`, the commands stop before the executable is moved, so herdvia is not installed. Then open a new terminal and check that it runs:

```sh
herdvia id
```

Move the executable itself to `~/.local/bin`. Do not leave it in your download folder and link to it: automatic startup records a symbolic link's path, not the file it points to, so the agent can no longer start once that file is moved or deleted.

Gatekeeper may refuse to run the file. A web browser marks the archive it downloads with a quarantine attribute, and extracting the archive keeps it on the executable; `curl` does not set it. If you downloaded the archive with a browser, remove the attribute:

```sh
xattr -d com.apple.quarantine ~/.local/bin/herdvia
```

#### Windows (x64)

Run these commands one at a time in a normal PowerShell window, not an administrator one, and check the result of each before you run the next. If a command prints an error, stop there.

Make a new download folder and go to it:

```powershell
$tmp = Join-Path $env:TEMP "herdvia-$(Get-Random)"
New-Item -ItemType Directory $tmp
Set-Location $tmp
```

Download the archive and `SHA256SUMS`:

```powershell
curl.exe -fLO https://github.com/photojupi/herdvia/releases/latest/download/herdvia-windows-x64.zip
curl.exe -fLO https://github.com/photojupi/herdvia/releases/latest/download/SHA256SUMS
```

Check the archive. The first command prints the archive's hash on its second line; the second prints the archive's line of `SHA256SUMS` (after `SHA256SUMS:<line number>:`), which starts with the hash it should have:

```powershell
certutil -hashfile herdvia-windows-x64.zip SHA256
Select-String herdvia-windows-x64.zip SHA256SUMS
```

If the two hashes differ, stop: the download is damaged or was changed. Do not extract or run it.

Extract the archive into `%LOCALAPPDATA%\Programs\herdvia` with `tar.exe`, which is part of Windows 10 1803 and later and extracts zip files:

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Programs\herdvia"
tar.exe -xf herdvia-windows-x64.zip -C "$env:LOCALAPPDATA\Programs\herdvia"
```

To run `herdvia` by name, add that folder to your user `Path` (the install script does this for you). Search the Start menu for "Edit environment variables for your account", select `Path` under the user variables and choose **Edit**, then **New**, and paste the folder's full path, which `"$env:LOCALAPPDATA\Programs\herdvia"` prints in PowerShell. If there is no user `Path` variable, create one named `Path` with that folder as its value. Otherwise, run herdvia by its full path, for example `& "$env:LOCALAPPDATA\Programs\herdvia\herdvia.exe" id`.

Then open a new PowerShell window (and restart Herdr if it is running) and check that it runs:

```powershell
herdvia id
```

SmartScreen may show a warning the first time you run it.

### From source

Requires Rust 1.91 or later (required by the iroh dependency).

- **macOS:** the Xcode Command Line Tools (`xcode-select --install`) and Rust from [rustup](https://rustup.rs).
- **Windows:** Rust from [rustup](https://rustup.rs) and the C++ tools of Visual Studio Build Tools ("Desktop development with C++"). The release executable is cross-compiled on Linux with MinGW (`x86_64-pc-windows-gnu`); a build with MSVC on Windows is not covered by CI.

```sh
cargo install --locked --git https://github.com/photojupi/herdvia
```

This builds the `main` branch. To build a release instead, add `--tag v<version>`. The executable is placed in `~/.cargo/bin` (on Windows, `%USERPROFILE%\.cargo\bin`), which rustup puts on your `PATH`.

## Quick start

Two machines: **A** dials (you run Herdr / `hc` here) and **B** is the machine you want to reach.

**1. Show each machine's identity.** Run on both A and B:

```sh
herdvia id
```

The first line is the machine's EndpointId (its public key). The second line is a short fingerprint: the first 16 characters of the EndpointId in groups of four. The secret key is created on first use.

**2. Run the agent on both machines.** The [install script](#install-script) has already done this. Otherwise, either run it in the foreground:

```sh
herdvia agent
```

or register it to start automatically at login and start it now:

```sh
herdvia agent install
```

See [Automatic startup](#automatic-startup) for platform details.

**3. On A, allow B.** Peers are never trusted automatically; each side must list the other. Copy B's EndpointId to A and let A dial B:

```sh
herdvia allow B <EndpointId of B> --outbound
```

Then check that the first 16 characters of the EndpointId you pasted match the fingerprint that `herdvia id` prints on B. Compare them in person or over a channel other than the one you used to copy the ID.

- `--inbound`: that peer may open sessions **on this machine**.
- `--outbound`: this machine may connect **to that peer**.
- `--both`: both directions.

The name (`A`, `B`) is a local label: 1–32 characters of `A-Z a-z 0-9 . _ -`. Running `allow` again with the same name replaces the entry.

**4. On B, accept A.** A's agent now tries to connect to B. B rejects the connection, because A is not in B's peer list, but lists the attempt:

```sh
herdvia pending
```

```text
fingerprint id attempts last-seen
28af-63b6-258e-859d 28af63b6258e859dc534c014248ebf7284cdbb34e7cff524e3e4d35c2df1fc55 3 4s
```

Read A's fingerprint on A's own screen (the second line of `herdvia id` on A) and type it on B:

```sh
herdvia accept A <fingerprint of A> --inbound
```

`accept` registers the full EndpointId of the one attempt with that fingerprint, as `herdvia allow A <EndpointId of A> --inbound` would, and prints `A <EndpointId> in`. The fingerprint may be typed with or without the dashes, in upper or lower case. Use `--both` to let B dial A as well; `--outbound` is not accepted. Do not copy the fingerprint from the `pending` list: anyone who knows B's EndpointId can appear there.

A retries a rejected connection at growing intervals of up to 30 seconds. So it can take up to about 30 seconds for A to appear in `pending`, and as long again for A to connect after `accept`. If `accept` reports that there is no connection attempt with that fingerprint, wait and run `herdvia pending` again.

If you can copy A's EndpointId to B instead, `herdvia allow A <EndpointId of A> --inbound` on B works as well; then check its fingerprint as in step 3.

**5. Add B to Herdr on A.**

```sh
hc machine add --via 'herdvia via B' --name B
```

`B` after `via` is the peer name you gave to `herdvia allow` on A. `--name` is the name Herdr shows; it can differ. `hc` appends its remote line to this command as one extra argument. If `hc` cannot find `herdvia` on its `PATH`, use the full path to the executable, and quote it inside `--via` if it contains spaces. On macOS, `hc` runs `--via` as an `sh` line, so quote the path as you would for `sh` (the install script prints the command that way). On Windows, `hc` runs `--via` as a program and arguments without a shell, split the way `CommandLineToArgvW` splits a command line, so the path needs double quotes around it. Windows PowerShell 5.1 does not pass double quotes inside an argument on to a program unchanged, so it is simpler to have `herdvia` on your `PATH` (open a new window and restart Herdr after installing) and use `herdvia via B` as above.

**6. Check the connection.** On A:

```sh
herdvia status
```

```text
name permission connection path rtt
B out connected direct 4.12ms
```

`path` is `direct` or `relay`. `status` requires the local agent to be running. A connection starts on a relay and usually becomes `direct` within a few seconds. On macOS, if a peer on the same local network stays on `relay`, see [Local network access](#macos) under Automatic startup; `status` prints a note on standard error when a peer is on a relay.

Until B accepts A, `status` on A can briefly show B as `connected` each time A retries, because B closes each attempt only after it is set up; to be sure, run `herdvia status` on B, which shows A as `connected` only once A has really connected.

## Commands

| Command | Description |
|---|---|
| `herdvia id` | Print this machine's EndpointId and fingerprint. Creates the secret key if missing. |
| `herdvia allow <name> <EndpointId> (--inbound \| --outbound \| --both)` | Add a peer or replace its entry. A running agent applies the change immediately; otherwise it is applied when the agent starts. |
| `herdvia revoke <name>` | Remove a peer. A running agent closes connections to it immediately. |
| `herdvia status` | Show each peer's name, permission, connection state, direct/relay path, and round-trip time. Requires a running agent. On macOS, also prints a note on standard error while a peer is on a relay. |
| `herdvia pending` | Show the machines whose connections the agent rejected because they lack inbound permission here: fingerprint, EndpointId, number of attempts, and seconds since the last attempt, most recent first. The list is kept in memory only, holds at most 16 machines, and drops each one 10 minutes after its last attempt. Requires a running agent. |
| `herdvia accept <name> <fingerprint> (--inbound \| --both)` | Allow the one machine in `pending` whose EndpointId starts with this fingerprint, as `allow` would with its full EndpointId. Registers nothing if no machine or more than one matches, or if `<name>` is already registered with a different EndpointId. Requires a running agent. |
| `herdvia --version` | Print `herdvia <version>`. |
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

**Local network access.** On macOS 15 and later, the automatically started agent cannot reach addresses on your local network until you allow it. macOS asks with an alert when the agent first uses the local network; you can also turn herdvia on under System Settings > Privacy & Security > Local Network. herdvia cannot grant this itself. Until you allow it, a peer on the same local network as the Mac is reached through a relay instead of directly (`relay` in `herdvia status`); peers on other networks are not affected. An agent you start from a terminal with `herdvia agent` does not need this permission.

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

## Upgrading and uninstalling

Your identity (`secret.key`) and peer list (`peers`) are in the data folder, not next to the executable, so replacing the executable keeps them. `herdvia --version` prints the installed version.

### Upgrading

Run the [install script](#install-script) again. It replaces the executable and runs `herdvia agent install` again, which restarts the agent with the new executable. It does not add the folder to your `PATH` a second time. On macOS it moves the new executable into place rather than copying over the old one. On Windows a running `herdvia.exe` cannot be overwritten, so the script first copies the new one beside it as `herdvia.new.exe` and checks that it runs, then renames the old one to `herdvia.exe.old` and the new one to `herdvia.exe`. Once the new `herdvia.exe` runs, it deletes the `.old` file if nothing runs it any more, at the end of the run or on a later run; a run that fails before then keeps it.

`herdvia agent install` refuses while an agent you started by hand is running (on Windows, only if automatic startup is not registered yet), and while `HERDVIA_DIR` is set. The script then stops with an error after installing the new executable: stop that agent (or unset `HERDVIA_DIR`) and run `herdvia agent install`. With `HERDVIA_NO_AGENT=1` the script does not touch the agent; restart it yourself.

To upgrade by hand:

**macOS:** Run the commands from [macOS (Apple Silicon)](#macos-apple-silicon) again, without the `~/.zprofile` line. They replace the executable with `mv`. Do not copy a new executable over the old one with `cp` while the agent is running from it: starting the overwritten file can then fail with `Killed: 9`. Then restart the agent with the new executable:

```sh
herdvia agent install
```

If you run the agent in the foreground instead, stop it and start it again.

**Windows:** Stop the agent before you replace the executable, because `tar.exe` overwrites `herdvia.exe` and a running one cannot be overwritten:

```powershell
herdvia agent uninstall
```

Then run the commands from [Windows (x64)](#windows-x64) again, without the `Path` step: the folder is already on it. Register and start the agent again:

```powershell
herdvia agent install
```

If you run the agent in the foreground instead, stop it with Ctrl-C before you replace the executable, and start it again afterwards.

### Uninstalling

1. Stop the agent and remove automatic startup with `herdvia agent uninstall`. Stop an agent running in the foreground with Ctrl-C.
2. Delete the executable: `rm ~/.local/bin/herdvia` on macOS, or in PowerShell `Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\herdvia"`.
3. To remove the identity and peer list as well, delete the data folder, and the log if there is one. See [Configuration and files](#configuration-and-files) for their locations.
4. On Windows, remove the folder from your user `Path`: search the Start menu for "Edit environment variables for your account", select `Path` under the user variables and choose **Edit**, then select the `herdvia` folder and choose **Delete**.
   On macOS, the `~/.zprofile` line puts all of `~/.local/bin` on your `PATH`, including `hc` and herdr-core if they are there; remove it only if nothing else there needs it.
5. On each machine that allowed this one, remove it with `herdvia revoke <name>`. On the machine where you run Herdr, remove it with `hc machine rm <name>`.

## Security model

- **A peer you allow with `--inbound` can open a herdr-core session on this machine as your user. In practice that is a shell.** Only allow devices you trust as much as yourself.
- Connections are authenticated by iroh with each machine's key pair. A connection is accepted only if the remote EndpointId is in the peer list with the matching direction. If the list is empty, every connection is rejected. Permission is checked again for every session, and changes to the list take effect immediately on a running agent.
- The command allowlist (only the exact `hc` connect lines) reduces misuse and makes anomalies visible: rejected lines are logged. It is not a privilege boundary, because `herdr-core connect` itself can run anything. The peer key list is the security boundary.
- herdvia refuses to run as root on macOS/Unix.
- The local channel between `hc`'s `via` process and the agent is restricted to your user: on macOS/Unix a socket with mode 0600 in a 0700 data folder, with the connecting user's UID checked; on Windows a named pipe whose access is granted only to your user SID, with remote clients rejected.
- **The secret key** is `secret.key` in the data folder. On macOS/Unix it is created with mode 0600, and herdvia refuses to start if its permissions are wider. On Windows it keeps the default permissions of your user profile; herdvia sets no extra ACL. Anyone who obtains this file can act as this machine toward peers that trust it. If it leaks, `revoke` this machine on every peer. There is no key rotation command; if you delete `secret.key`, a new identity is created on next use and peers must be allowed again.
- The fingerprint is only the first 16 characters (64 bits) of the EndpointId. Use it for comparing by eye and for typing into `accept`; the full EndpointId is what is stored and checked.
- **`pending` and `accept`:** the agent records a rejected connection only after iroh's handshake has authenticated the other machine's key, and records only that machine's EndpointId. The list is kept in memory only, holds at most 16 machines, and drops each one 10 minutes after its last attempt or as soon as it gains inbound permission. `accept` registers a full EndpointId only when exactly one machine in the list has the fingerprint you typed. Anyone who knows this machine's EndpointId can appear in the list, so being listed is no reason to trust a machine. Read the fingerprint you type on the other machine's own screen, not from the list.

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
| macOS, Intel | No prebuilt binary; build from source. Not tested. |
| Windows 11, x64 | Tested on real hardware. Automatic startup via the `Run` registry key. |
| Windows 10, x64 | Not tested. |
| Windows on ARM | Not tested. Only an x64 build is provided, and Herdr's Windows install script stops on ARM64 ("x64 only"). |
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
