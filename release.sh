#!/bin/sh

set -eu

repository_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$repository_dir"

usage() {
    printf 'usage: %s --major|--minor|--patch\n' "$0" >&2
}

[ "$#" -eq 1 ] || {
    usage
    exit 2
}

case "$1" in
    --major) bump=major ;;
    --minor) bump=minor ;;
    --patch) bump=patch ;;
    --help | -h)
        usage
        exit 0
        ;;
    *)
        usage
        exit 2
        ;;
esac

version=$(awk '
    /^\[package\]$/ { package = 1; next }
    package && /^\[/ { exit }
    package && /^[[:space:]]*version[[:space:]]*=/ {
        split($0, fields, "\"")
        print fields[2]
        exit
    }
' package/Cargo.toml)

[ -n "$version" ] || {
    printf 'error: package version not found in package/Cargo.toml\n' >&2
    exit 1
}
printf '%s\n' "$version" | awk -F. '
    NF == 3 &&
    $1 ~ /^(0|[1-9][0-9]*)$/ &&
    $2 ~ /^(0|[1-9][0-9]*)$/ &&
    $3 ~ /^(0|[1-9][0-9]*)$/ { valid = 1 }
    END { exit !valid }
' || {
    printf 'error: package version must use MAJOR.MINOR.PATCH\n' >&2
    exit 1
}

IFS=. read -r major minor patch <<EOF
$version
EOF
case "$bump" in
    major) next_version="$((major + 1)).0.0" ;;
    minor) next_version="${major}.$((minor + 1)).0" ;;
    patch) next_version="${major}.${minor}.$((patch + 1))" ;;
esac

[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || {
    printf 'error: releases must be created from main\n' >&2
    exit 1
}

[ -z "$(git status --porcelain)" ] || {
    printf 'error: commit all changes before creating a release\n' >&2
    exit 1
}

temporary_manifest=$(mktemp package/Cargo.toml.XXXXXX)
temporary_lock=$(mktemp package/Cargo.lock.XXXXXX)
trap 'rm -f "$temporary_manifest" "$temporary_lock"' EXIT HUP INT TERM

awk -v version="$next_version" '
    /^\[package\]$/ { package = 1; print; next }
    package && /^\[/ { package = 0 }
    package && /^[[:space:]]*version[[:space:]]*=/ {
        print "version = \"" version "\""
        updated = 1
        next
    }
    { print }
    END { exit !updated }
' package/Cargo.toml >"$temporary_manifest" || {
    printf 'error: package version not found in package/Cargo.toml\n' >&2
    exit 1
}

awk -v version="$next_version" '
    /^\[\[package\]\]$/ { package = 1; signal_tui = 0 }
    package && /^name = "signal-tui"$/ { signal_tui = 1 }
    package && signal_tui && /^version = / {
        print "version = \"" version "\""
        updated = 1
        next
    }
    { print }
    END { exit !updated }
' package/Cargo.lock >"$temporary_lock" || {
    printf 'error: signal-tui version not found in package/Cargo.lock\n' >&2
    exit 1
}

chmod 0644 "$temporary_manifest" "$temporary_lock"
mv "$temporary_manifest" package/Cargo.toml
mv "$temporary_lock" package/Cargo.lock

cargo metadata \
    --manifest-path package/Cargo.toml \
    --locked \
    --format-version 1 \
    >/dev/null

git add package/Cargo.toml package/Cargo.lock
git commit -m "Bump package version to ${next_version}"

tag="v${next_version}"
git tag -a "$tag" -m "$tag"
git push --atomic origin main "$tag"
