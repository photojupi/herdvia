#!/bin/sh
# Installs herdvia on macOS (Apple Silicon), or upgrades it:
#
#   curl -fsSL https://github.com/photojupi/herdvia/releases/latest/download/install.sh | sh
#
# Puts the executable at ~/.local/bin/herdvia, adds ~/.local/bin to PATH in
# ~/.zprofile if it is missing, and starts the agent at login (herdvia agent
# install). It does not install herdr-core; it only says how. The identity
# and peer list are in the data folder, so upgrading keeps them.
#
# Environment variables:
#   HERDVIA_VERSION=0.1.0       install this release instead of the latest
#   HERDVIA_NO_AGENT=1          do not run herdvia agent install
#   HERDVIA_NO_MODIFY_PATH=1    do not change ~/.zprofile
#   HERDVIA_RELEASE_BASE=<url>  download <url>/<file> instead (mirrors, tests);
#                               https:// only, or http:// to 127.0.0.1,
#                               localhost, or [::1]
set -eu

repo=https://github.com/photojupi/herdvia/releases
archive=herdvia-macos-arm64.zip
# The herdr-core version herdvia was verified with.
herdr_version=0.1.22

say() { printf '%s\n' "$*"; }
fail() { say "herdvia install: $*" >&2; exit 1; }
# Quotes $1 for sh, unless it has nothing a shell would treat specially.
quote() {
  case $1 in
    '' | *[!A-Za-z0-9._/:@%+=,-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s' "$1" ;;
  esac
}
# Whether $1 is an http:// URL to 127.0.0.1, localhost, or [::1], with at most
# a numeric port: no user part, so http://localhost:1@example.com is not one.
loopback_http() {
  case $1 in
    http://*) ;;
    *) return 1 ;;
  esac
  authority=${1#http://}
  authority=${authority%%/*}
  case $authority in
    127.0.0.1 | localhost | '[::1]') return 0 ;;
    127.0.0.1:*) port=${authority#127.0.0.1:} ;;
    localhost:*) port=${authority#localhost:} ;;
    '[::1]:'*) port=${authority#'[::1]:'} ;;
    *) return 1 ;;
  esac
  case $port in
    '' | *[!0-9]*) return 1 ;;
  esac
}

tmp=
staged=
cleanup() {
  if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
  if [ -n "$staged" ]; then rm -f "$staged"; fi
}

# Everything runs from main, called on the last line: under `curl | sh`, a
# download cut short then ends in a syntax error before anything has run.
main() {
  [ -n "${HOME:-}" ] || fail "HOME is not set"
  [ "$(id -u)" != 0 ] || fail "do not run this as root; herdvia refuses to run as root"

  arm64=0
  if [ "$(uname -s)" = Darwin ]; then
    # A shell running under Rosetta reports x86_64 on Apple Silicon.
    if [ "$(uname -m)" = arm64 ] || [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
      arm64=1
    fi
  fi
  if [ "$arm64" != 1 ]; then
    say "herdvia install: there is a prebuilt herdvia only for macOS on Apple Silicon; this is $(uname -s) $(uname -m). Nothing was changed." >&2
    say "Build it from source instead (Rust 1.91 or later):" >&2
    say "  cargo install --locked --git https://github.com/photojupi/herdvia" >&2
    say "See https://github.com/photojupi/herdvia#from-source" >&2
    exit 1
  fi

  for tool in curl unzip shasum; do
    command -v "$tool" >/dev/null 2>&1 || fail "needs $tool"
  done

  # SHA256SUMS comes from the same place as the archive, so it cannot catch a
  # download changed on the way: only HTTPS, also after a redirect.
  proto='=https'
  if [ -n "${HERDVIA_RELEASE_BASE:-}" ]; then
    base=${HERDVIA_RELEASE_BASE%/}
    case $base in
      https://*) ;;
      *)
        # Plain http only to this machine, for tests with a local server.
        loopback_http "$base" || fail "HERDVIA_RELEASE_BASE must start with https:// (http:// only to 127.0.0.1, localhost, or [::1]): $HERDVIA_RELEASE_BASE; nothing was installed"
        proto='=http,https'
        ;;
    esac
  elif [ -n "${HERDVIA_VERSION:-}" ]; then
    version=${HERDVIA_VERSION#v}
    case $version in
      '' | *[!0-9A-Za-z.-]*) fail "invalid HERDVIA_VERSION: $HERDVIA_VERSION" ;;
    esac
    base=$repo/download/v$version
  else
    base=$repo/latest/download
  fi

  bin=$HOME/.local/bin
  exe=$bin/herdvia
  trap cleanup EXIT
  trap 'exit 1' HUP INT TERM
  tmp=$(mktemp -d)

  # Commands below read from /dev/null: under `curl | sh`, standard input is
  # the rest of this script.
  say "Downloading $base/$archive"
  curl -fsSL --proto "$proto" --proto-redir =https --tlsv1.2 "$base/$archive" -o "$tmp/$archive" </dev/null || fail "download failed: $base/$archive; nothing was installed"
  curl -fsSL --proto "$proto" --proto-redir =https --tlsv1.2 "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" </dev/null || fail "download failed: $base/SHA256SUMS; nothing was installed"
  want=$(tr -d '\r' <"$tmp/SHA256SUMS" | awk -v f="$archive" '$2 == f || $2 == "*" f { print tolower($1); exit }')
  [ -n "$want" ] || fail "SHA256SUMS has no line for $archive; nothing was installed"
  got=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
  [ "$got" = "$want" ] || fail "checksum mismatch for $archive (the download is damaged or was changed); nothing was installed"

  mkdir "$tmp/x"
  unzip -q "$tmp/$archive" herdvia -d "$tmp/x" </dev/null || fail "cannot extract herdvia from $archive; nothing was installed"
  [ -f "$tmp/x/herdvia" ] || fail "$archive holds no herdvia; nothing was installed"

  action=Installed
  [ ! -e "$exe" ] || action=Upgraded
  mkdir -p "$bin" || fail "cannot create $bin"
  # Stage beside the target and move it into place: copying over the file a
  # running agent was started from makes macOS kill the next start of it.
  staged=$(mktemp "$bin/.herdvia.XXXXXX") || fail "cannot write to $bin"
  cp "$tmp/x/herdvia" "$staged" || fail "cannot write to $bin"
  chmod 755 "$staged"
  if xattr -p com.apple.quarantine "$staged" >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$staged"
  fi
  "$staged" --version </dev/null >/dev/null || fail "the downloaded herdvia does not run on this machine; nothing was installed"
  mv -f "$staged" "$exe" || fail "cannot replace $exe"
  staged=
  installed=$("$exe" --version </dev/null) || fail "$exe was installed, but herdvia --version failed"
  say "$action $installed at $exe"

  line='export PATH="$HOME/.local/bin:$PATH"'
  case ":$PATH:" in
    *":$bin:"*) ;;
    *)
      shell=${SHELL:-}
      if [ "${HERDVIA_NO_MODIFY_PATH:-}" = 1 ]; then
        say "$bin is not on your PATH, and HERDVIA_NO_MODIFY_PATH=1 left it that way. Run herdvia by its full path, or add this line to your shell's startup file:"
        say "  $line"
      elif [ "${shell##*/}" = zsh ]; then
        profile=$HOME/.zprofile
        if [ -f "$profile" ] && grep -qxF "$line" "$profile"; then
          say "$profile already adds $bin to your PATH. Open a new terminal to use herdvia."
        else
          # Do not join the line to a last line that has no newline.
          if [ -s "$profile" ] && [ -n "$(tail -c 1 "$profile")" ]; then
            printf '\n' >>"$profile"
          fi
          printf '%s\n' "$line" >>"$profile"
          say "Added $bin to your PATH in $profile. Open a new terminal to use herdvia."
        fi
      else
        say "$bin is not on your PATH. Your login shell is not zsh (${shell:-unknown}), so this script did not change it. Add this line to your shell's startup file and open a new terminal:"
        say "  $line"
      fi
      ;;
  esac

  # herdvia runs herdr-core; it does not install it, and neither does this script.
  if [ -x "$bin/herdr-core" ]; then
    say "herdr-core: found at $bin/herdr-core"
  elif found=$(command -v herdr-core 2>/dev/null); then
    say "herdr-core is at $found, but the agent that starts at login searches only $bin:/usr/bin:/bin:/usr/sbin:/sbin, so it will not find it. Link it into $bin:"
    say "  ln -s $(quote "$found") $(quote "$bin/herdr-core")"
  else
    say "herdr-core is not installed. herdvia needs it on both machines. This script does not install it; Herdr's own install script does:"
    say "  curl -fsSL https://releases.herdr.dev/core/install.sh | sh"
    say "To install $herdr_version, the version herdvia was verified with:"
    say "  curl -fsSL https://releases.herdr.dev/core/install.sh | HC_VERSION=$herdr_version sh"
  fi

  say "This machine's EndpointId and fingerprint:"
  "$exe" id </dev/null || fail "herdvia is installed at $exe, but herdvia id failed (see above)"

  # The commands below, quoted to paste into a shell.
  run=$(quote "$exe")
  if [ "${HERDVIA_NO_AGENT:-}" = 1 ]; then
    say "The agent was not started (HERDVIA_NO_AGENT=1). To start it now and at every login, or to restart it with this version:"
    say "  $run agent install"
  else
    say "Starting the agent now and at every login (herdvia agent install)"
    if ! "$exe" agent install </dev/null; then
      say "herdvia install: herdvia is installed at $exe, but herdvia agent install failed (see the error above). Fix the problem, then run:" >&2
      say "  $run agent install" >&2
      exit 1
    fi
  fi

  say "macOS may ask whether herdvia may find devices on your local network. Allow it: until you do, a machine on the same local network as this Mac is reached only through a relay. The setting is under System Settings > Privacy & Security > Local Network."

  say ""
  say "Next steps"
  say "If another machine (running Herdr) will control this Mac, wait for pairing here:"
  say "  $run listen"
  say "If this Mac runs Herdr and will control another machine, run 'herdvia listen' there first, then run here:"
  say "  $run connect <name>"
  say "On different networks, pass the hvi1-... invite printed by herdvia listen:"
  say "  $run connect <name> <invite>"
  say "Check that both screens show the same 6-digit verification code before approving on the target machine."
  say "For manual pairing (allow / pending / accept), see https://github.com/photojupi/herdvia#manual-pairing"
}

main "$@"
