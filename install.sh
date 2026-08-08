#!/bin/sh

set -eu

repository="emilianscheel/signal-cli"
install_dir="/usr/local/bin"
program="signal"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

require_command curl
require_command install
require_command mktemp
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

if [ -d "$install_dir" ] && [ -w "$install_dir" ]; then
    install -m 0755 "${temporary_dir}/${asset}" "${install_dir}/${program}"
else
    require_command sudo
    sudo install -d -m 0755 "$install_dir"
    sudo install -m 0755 "${temporary_dir}/${asset}" "${install_dir}/${program}"
fi

printf 'Installed %s to %s/%s\n' "$program" "$install_dir" "$program"
