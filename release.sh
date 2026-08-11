#!/bin/sh

set -eu

repository_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$repository_dir"

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

[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || {
    printf 'error: releases must be created from main\n' >&2
    exit 1
}

[ -z "$(git status --porcelain)" ] || {
    printf 'error: commit all changes before creating a release\n' >&2
    exit 1
}

cargo metadata \
    --manifest-path package/Cargo.toml \
    --locked \
    --format-version 1 \
    >/dev/null

tag="v${version}"
git tag -a "$tag" -m "$tag"
git push --atomic origin main "$tag"
