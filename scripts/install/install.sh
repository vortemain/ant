#!/bin/sh
# Ant installer for macOS and Linux (D-043).
#
#   curl -fsSL https://raw.githubusercontent.com/vortemain/ant/main/scripts/install/install.sh | sh
#
# Downloads the `ant` binary for this machine from GitHub Releases, verifies
# its SHA-256 against the release's SHA256SUMS, installs it into ~/.ant/bin,
# and adds that directory to PATH in your shell profile.
#
# Settings (environment):
#   ANT_VERSION         install this version (e.g. 0.2.0) instead of the latest
#   ANT_INSTALL_DIR     install directory (default: $HOME/.ant/bin)
#   ANT_NO_MODIFY_PATH  set to 1 to leave shell profiles alone
#   ANT_DOWNLOAD_BASE   mirror laid out like https://github.com/vortemain/ant/releases
set -eu

base=${ANT_DOWNLOAD_BASE:-https://github.com/vortemain/ant/releases}
install_dir=${ANT_INSTALL_DIR:-$HOME/.ant/bin}

say() { printf '%s\n' "$*"; }
fail() { printf 'ant install: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux) os_part=unknown-linux-musl ;;
  Darwin) os_part=apple-darwin ;;
  MINGW* | MSYS* | CYGWIN*)
    fail "on Windows, run this in PowerShell instead:  irm https://raw.githubusercontent.com/vortemain/ant/main/scripts/install/install.ps1 | iex" ;;
  *) fail "unsupported operating system: $os" ;;
esac
case "$arch" in
  x86_64 | amd64) arch_part=x86_64 ;;
  arm64 | aarch64) arch_part=aarch64 ;;
  *) fail "unsupported CPU architecture: $arch" ;;
esac
# A Rosetta shell on Apple Silicon reports x86_64; prefer the native build.
if [ "$os" = Darwin ] && [ "$arch_part" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
  arch_part=aarch64
fi
target="$arch_part-$os_part"
asset="ant-$target"

if [ -n "${ANT_VERSION:-}" ]; then
  url_base="$base/download/v${ANT_VERSION#v}"
else
  url_base="$base/latest/download"
fi

download() { # url destination
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    fail "curl or wget is required"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    fail "sha256sum or shasum is required to verify the download"
  fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading Ant for $target..."
download "$url_base/$asset" "$tmp/$asset" || fail "could not download $url_base/$asset"
download "$url_base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "could not download $url_base/SHA256SUMS"

expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $asset"
actual=$(sha256_of "$tmp/$asset")
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual); nothing was installed"

mkdir -p "$install_dir"
chmod +x "$tmp/$asset"
mv -f "$tmp/$asset" "$install_dir/ant"
version=$("$install_dir/ant" --version 2>/dev/null || echo "Ant")
say "Installed $version to $install_dir/ant"

case ":$PATH:" in
  *":$install_dir:"*) on_path=1 ;;
  *) on_path=0 ;;
esac
if [ "$on_path" = 0 ] && [ "${ANT_NO_MODIFY_PATH:-0}" != 1 ]; then
  line="export PATH=\"$install_dir:\$PATH\""
  for profile in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
    case "$profile" in
      */.zshrc) [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = zsh ] || [ -f "$profile" ] || continue ;;
      */.bashrc) [ -f "$profile" ] || [ "$(basename "${SHELL:-}")" = bash ] || continue ;;
    esac
    if ! grep -qs "$install_dir" "$profile"; then
      printf '\n# Ant\n%s\n' "$line" >> "$profile"
      say "Added $install_dir to PATH in $profile"
    fi
  done
  if [ -d "$HOME/.config/fish" ] && command -v fish >/dev/null 2>&1; then
    fish -c "fish_add_path -U $install_dir" 2>/dev/null || true
  fi
  say ""
  say "Open a new terminal (or run: export PATH=\"$install_dir:\$PATH\"), then:"
else
  say ""
  say "Get started:"
fi
say "  cd your-project && ant build"
say "Update later with: ant upgrade"
