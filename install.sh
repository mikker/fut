#!/bin/sh
set -eu

repository="mikker/fut"
install_dir="${FUT_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Darwin) platform="macos" ;;
  Linux) platform="linux" ;;
  *) echo "fut: unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="arm64" ;;
  x86_64|amd64) arch="x86_64" ;;
  *) echo "fut: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

for command in curl tar; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "fut: $command is required" >&2
    exit 1
  }
done

archive="fut-$platform-$arch.tar.gz"
if [ -n "${FUT_VERSION:-}" ]; then
  release_url="https://github.com/$repository/releases/download/$FUT_VERSION"
else
  release_url="https://github.com/$repository/releases/latest/download"
fi

tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t fut)"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

echo "Downloading fut for $platform/$arch..."
curl -fsSL "$release_url/$archive" -o "$tmp_dir/$archive"
curl -fsSL "$release_url/$archive.sha256" -o "$tmp_dir/$archive.sha256"

expected="$(awk 'NR == 1 { print $1 }' "$tmp_dir/$archive.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp_dir/$archive" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp_dir/$archive" | awk '{ print $1 }')"
else
  echo "fut: sha256sum or shasum is required" >&2
  exit 1
fi

if [ "$actual" != "$expected" ]; then
  echo "fut: checksum verification failed" >&2
  exit 1
fi

tar -xzf "$tmp_dir/$archive" -C "$tmp_dir"
mkdir -p "$install_dir"
cp "$tmp_dir/fut" "$install_dir/.fut.$$"
chmod 755 "$install_dir/.fut.$$"
mv "$install_dir/.fut.$$" "$install_dir/fut"

echo "Installed $("$install_dir/fut" --version) to $install_dir/fut"
case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *) echo "Add $install_dir to your PATH to run fut." ;;
esac
