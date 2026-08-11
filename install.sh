#!/bin/sh

set -eu

repository="emilianscheel/signal-cli"
install_dir="/usr/local/bin"
managed_dir="${HOME:?HOME is required}/.local/lib/signal-cli"
program="signal"
managed_program="${managed_dir}/${program}"
managed_marker="${managed_dir}/.signal-managed-install"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

require_command curl
require_command install
require_command ln
require_command mktemp
require_command mv
require_command uname
require_command awk

case "$(uname -s)" in
    Darwin) platform="apple-darwin" ;;
    Linux) platform="unknown-linux-gnu" ;;
    *) fail "unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) architecture="x86_64" ;;
    arm64 | aarch64) architecture="aarch64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

asset="${program}-${architecture}-${platform}"
release_url="https://github.com/${repository}/releases/latest/download"

temporary_dir=$(mktemp -d 2>/dev/null || mktemp -d -t signal-cli)
trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM

printf 'Downloading %s...\n' "$asset"
curl --fail --location --silent --show-error \
    "${release_url}/${asset}" \
    --output "${temporary_dir}/${asset}"
curl --fail --location --silent --show-error \
    "${release_url}/SHA256SUMS" \
    --output "${temporary_dir}/SHA256SUMS"

expected_checksum=$(awk -v asset="$asset" '$2 == asset { print $1 }' "${temporary_dir}/SHA256SUMS")
[ -n "$expected_checksum" ] || fail "SHA256SUMS does not contain a checksum for $asset"

if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$expected_checksum" "${temporary_dir}/${asset}" \
        | sha256sum --check --status - \
        || fail "checksum verification failed for $asset"
elif command -v shasum >/dev/null 2>&1; then
    printf '%s  %s\n' "$expected_checksum" "${temporary_dir}/${asset}" \
        | shasum -a 256 --check - >/dev/null \
        || fail "checksum verification failed for $asset"
else
    fail "required checksum command not found: install sha256sum or shasum"
fi

install -d -m 0755 "$managed_dir"
managed_temporary="${managed_dir}/.${program}.install.$$"
install -m 0755 "${temporary_dir}/${asset}" "$managed_temporary"
mv -f "$managed_temporary" "$managed_program"
install -m 0644 /dev/null "$managed_marker"

command_path="${install_dir}/${program}"
[ ! -d "$command_path" ] || fail "$command_path is a directory"
if [ -d "$install_dir" ] && [ -w "$install_dir" ]; then
    ln -sfn "$managed_program" "$command_path"
else
    require_command sudo
    sudo install -d -m 0755 "$install_dir"
    sudo ln -sfn "$managed_program" "$command_path"
fi

printf 'Installed %s to %s (managed binary: %s)\n' \
    "$program" "$command_path" "$managed_program"
