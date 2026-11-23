# Installs herdvia on Windows (x64), or upgrades it:
#
#   powershell -c "irm https://github.com/photojupi/herdvia/releases/latest/download/install.ps1 | iex"
#
# Puts herdvia.exe in %LOCALAPPDATA%\Programs\herdvia, adds that folder to
# your user PATH, and starts the agent at sign-in (herdvia agent install). No
# administrator rights are needed; from an administrator PowerShell the agent
# is not registered. It does not install herdr-core; it only says how. The
# identity and peer list are in the data folder, so upgrading keeps them.
#
# Environment variables:
#   HERDVIA_VERSION=0.1.0       install this release instead of the latest
#   HERDVIA_NO_AGENT=1          do not run herdvia agent install
#   HERDVIA_NO_MODIFY_PATH=1    do not change the user PATH
#   HERDVIA_RELEASE_BASE=<url>  download <url>/<file> instead (mirrors, tests);
#                               https:// only, or http:// to 127.0.0.1,
#                               localhost, or [::1]

# Its own scope: under `irm | iex` it would otherwise set the caller's variables.
& {
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# throw, not exit: under `irm | iex`, exit would close the person's window.
function Fail($why) { throw "herdvia install: $why" }

$repo = "https://github.com/photojupi/herdvia/releases"
$archive = "herdvia-windows-x64.zip"
# The herdr-core version herdvia was verified with.
$herdrVersion = "0.1.22"

$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne "AMD64") { Fail "there is a prebuilt herdvia only for x64 Windows; this is $arch. Nothing was changed." }
if (-not $env:LOCALAPPDATA) { Fail "LOCALAPPDATA is not set" }
if (-not $env:USERPROFILE) { Fail "USERPROFILE is not set" }
$curl = Join-Path $env:SystemRoot "System32\curl.exe"
if (-not (Test-Path -LiteralPath $curl)) { Fail "needs $curl (Windows 10 1803 or newer)" }

# SHA256SUMS comes from the same place as the archive, so it cannot catch a
# download changed on the way: only HTTPS, also after a redirect.
$proto = "=https"
if ($env:HERDVIA_RELEASE_BASE) {
    $base = $env:HERDVIA_RELEASE_BASE.TrimEnd("/")
    if ($base -cmatch '^http://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]+)?(/|$)') {
        # Plain http only to this machine, for tests with a local server.
        $proto = "=http,https"
    } elseif ($base -cnotmatch '^https://') {
        Fail "HERDVIA_RELEASE_BASE must start with https:// (http:// only to 127.0.0.1, localhost, or [::1]): $($env:HERDVIA_RELEASE_BASE); nothing was installed"
    }
} elseif ($env:HERDVIA_VERSION) {
    $version = $env:HERDVIA_VERSION -replace "^v", ""
    if ($version -notmatch "^[0-9A-Za-z.-]+$") { Fail "invalid HERDVIA_VERSION: $($env:HERDVIA_VERSION)" }
    $base = "$repo/download/v$version"
} else {
    $base = "$repo/latest/download"
}

$dir = Join-Path $env:LOCALAPPDATA "Programs\herdvia"
$exe = Join-Path $dir "herdvia.exe"
# Ends in .exe: PowerShell opens a file with another extension instead of running it.
$staged = Join-Path $dir "herdvia.new.exe"

# A replaced herdvia.exe that is still running cannot be deleted yet; a later
# run deletes it. Only once the new herdvia.exe is in place and runs: until
# then, a renamed one may be the only copy that works.
function Remove-Old {
    foreach ($old in @(Get-ChildItem -LiteralPath $dir -Filter "herdvia.exe*.old")) {
        try { Remove-Item -LiteralPath $old.FullName -Force } catch { }
    }
}

$placed = $false
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("herdvia-install-" + [IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zip = Join-Path $tmp $archive
    $sums = Join-Path $tmp "SHA256SUMS"
    Write-Host "Downloading $base/$archive"
    # Continue around programs that may write to stderr: with stderr redirected,
    # Windows PowerShell turns those lines into errors, which Stop would throw.
    # Quoted: Windows PowerShell may split an unquoted -argument at a dot.
    & { $ErrorActionPreference = "Continue"; & $curl -fsSL --proto $proto --proto-redir "=https" "--tlsv1.2" "$base/$archive" -o $zip }
    if ($LASTEXITCODE) { Fail "download failed: $base/$archive; nothing was installed" }
    & { $ErrorActionPreference = "Continue"; & $curl -fsSL --proto $proto --proto-redir "=https" "--tlsv1.2" "$base/SHA256SUMS" -o $sums }
    if ($LASTEXITCODE) { Fail "download failed: $base/SHA256SUMS; nothing was installed" }

    $want = $null
    foreach ($line in [IO.File]::ReadAllLines($sums)) {
        $fields = $line.Trim() -split "\s+"
        if ($fields.Count -ge 2 -and ($fields[1] -eq $archive -or $fields[1] -eq "*$archive")) {
            $want = $fields[0]
            break
        }
    }
    if (-not $want) { Fail "SHA256SUMS has no line for $archive; nothing was installed" }
    # .NET, not Get-FileHash: that one fails when PowerShell 7 starts 5.1.
    $stream = [IO.File]::OpenRead($zip)
    try { $got = -join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString("x2") }) } finally { $stream.Close() }
    if ($got -ne $want) { Fail "checksum mismatch for $archive (the download is damaged or was changed); nothing was installed" }

    # .NET, not Expand-Archive, which is a script module (blocked by a Restricted execution policy).
    $x = Join-Path $tmp "x"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory($zip, $x)
    $new = Join-Path $x "herdvia.exe"
    if (-not (Test-Path -LiteralPath $new)) { Fail "$archive holds no herdvia.exe; nothing was installed" }

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    # Copy beside herdvia.exe and check that the copy runs before anything is
    # renamed: a copy cut short then leaves only a broken herdvia.new.exe.
    Copy-Item -LiteralPath $new -Destination $staged -Force
    $runs = $false
    try { & $staged --version | Out-Null; $runs = ($LASTEXITCODE -eq 0) } catch { }
    if (-not $runs) { Fail "the downloaded herdvia.exe does not run on this machine; nothing was installed" }

    $action = "Installed"
    $old = $null
    if (Test-Path -LiteralPath $exe) {
        $action = "Upgraded"
        # A running herdvia.exe cannot be overwritten, but it can be renamed.
        $old = Join-Path $dir "herdvia.exe.old"
        if (Test-Path -LiteralPath $old) { $old = Join-Path $dir ("herdvia.exe." + [IO.Path]::GetRandomFileName() + ".old") }
        Move-Item -LiteralPath $exe -Destination $old
    }
    try {
        Move-Item -LiteralPath $staged -Destination $exe
    } catch {
        if ($old) { Move-Item -LiteralPath $old -Destination $exe }
        throw
    }
    foreach ($name in "README.md", "LICENSE-MIT", "LICENSE-APACHE", "THIRD-PARTY-LICENSES.html") {
        $file = Join-Path $x $name
        if (Test-Path -LiteralPath $file) { Copy-Item -LiteralPath $file -Destination $dir -Force }
    }
    $installed = & $exe --version
    if ($LASTEXITCODE) { Fail "$exe was installed, but herdvia --version failed" }
    $placed = $true
    Write-Host "$action $installed at $exe"

    if ($env:HERDVIA_NO_MODIFY_PATH -eq "1") {
        Write-Host "HERDVIA_NO_MODIFY_PATH=1: your PATH was not changed. Run herdvia by its full path, or add $dir to your PATH."
    } else {
        # The raw user PATH, so %VARIABLES% in it stay unexpanded and keep their type.
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
        try {
            $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $listed = $raw -split ";" | Where-Object { [Environment]::ExpandEnvironmentVariables($_).Trim().TrimEnd("\") -ieq $dir.TrimEnd("\") }
            if (-not $listed) {
                $kind = if ($key.GetValueNames() -contains "Path") { $key.GetValueKind("Path") } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
                $key.SetValue("Path", $(if ($raw) { "$($raw.TrimEnd(';'));$dir" } else { $dir }), $kind)
                # Setting any user variable tells running programs the environment changed.
                [Environment]::SetEnvironmentVariable("HERDVIA_PATH_CHANGED", "1", "User")
                [Environment]::SetEnvironmentVariable("HERDVIA_PATH_CHANGED", $null, "User")
                Write-Host "Added $dir to your user PATH. Open a new PowerShell window (and restart Herdr if it is running) to use herdvia."
            }
        } finally {
            $key.Close()
        }
        if (-not ($env:Path -split ";" | Where-Object { $_.Trim().TrimEnd("\") -ieq $dir.TrimEnd("\") })) {
            $env:Path = "$($env:Path.TrimEnd(';'));$dir"
        }
    }

    # herdvia runs herdr-core; it does not install it, and neither does this script.
    $core = Join-Path $env:USERPROFILE "AppData\Local\Programs\herdr-core\herdr-core.exe"
    if (Test-Path -LiteralPath $core -PathType Leaf) {
        Write-Host "herdr-core: found at $core"
    } else {
        Write-Host "herdr-core is not installed. herdvia needs it on both machines and looks for it only at $core."
        Write-Host "This script does not install it; Herdr's own install script does:"
        Write-Host '  powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"'
        Write-Host "To install $herdrVersion, the version herdvia was verified with on macOS (on Windows, last verified with 0.1.21):"
        Write-Host ('  $env:HC_VERSION = "' + $herdrVersion + '"; powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"')
    }

    Write-Host "This machine's EndpointId and fingerprint:"
    $identity = & $exe id
    if ($LASTEXITCODE) { Fail "herdvia is installed at $exe, but herdvia id failed (see above)" }
    foreach ($text in $identity) { Write-Host $text }

    # Single quotes, doubled inside: a path with $ or a backtick stays literal when pasted.
    $run = "& '" + $exe.Replace("'", "''") + "'"
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($env:HERDVIA_NO_AGENT -eq "1") {
        Write-Host "The agent was not started (HERDVIA_NO_AGENT=1). To start it now and at every sign-in, or to restart it with this version:"
        Write-Host "  $run agent install"
    } elseif ($admin) {
        Write-Host "The agent was not started: this PowerShell is running as administrator, and herdvia agent install refuses to run there. Open a normal PowerShell window and run:"
        Write-Host "  $run agent install"
    } else {
        Write-Host "Starting the agent now and at every sign-in (herdvia agent install)"
        & { $ErrorActionPreference = "Continue"; & $exe agent install } | Out-Host
        if ($LASTEXITCODE) { Fail "herdvia is installed at $exe, but herdvia agent install failed (see the error above). Fix the problem, then run: $run agent install" }
    }

    Write-Host ""
    Write-Host "Next steps"
    Write-Host "If another machine (running Herdr) will control this PC, wait for pairing here:"
    Write-Host "  $run listen"
    Write-Host "If this PC runs Herdr and will control another machine, run 'herdvia listen' there first, then run here:"
    Write-Host "  $run connect <name>"
    Write-Host "On different networks, pass the hvi1-... invite printed by herdvia listen:"
    Write-Host "  $run connect <name> <invite>"
    Write-Host "Check that both screens show the same 6-digit verification code before approving on the target machine."
    Write-Host "For manual pairing (allow / pending / accept), see https://github.com/photojupi/herdvia#manual-pairing"
} finally {
    try { Remove-Item -LiteralPath $tmp -Recurse -Force } catch { }
    if (Test-Path -LiteralPath $staged) { try { Remove-Item -LiteralPath $staged -Force } catch { } }
    if ($placed) { Remove-Old }
}
}
