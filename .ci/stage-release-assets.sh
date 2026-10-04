#!/usr/bin/env bash

# Name every asset rather than counting them. A bare count passes when a target
# is renamed or swapped for another, which publishes the wrong set under a name
# users are told to trust, and these names are what README documents. The list
# duplicates the build matrix on purpose: Actions has no YAML anchors, and a
# rename should fail here rather than ship quietly.
#
# Runs before anything is deleted, so a missing artifact fails while the old
# release is still up.

set -eu

expected=(
    hackmd-mcp-x86_64-unknown-linux-gnu.tar.gz
    hackmd-mcp-aarch64-apple-darwin.tar.gz
    hackmd-mcp-x86_64-pc-windows-msvc.zip
)

if ! diff <(printf '%s\n' "${expected[@]}" | sort) <(cd dist && printf '%s\n' * | sort) >&2; then
    echo "release assets do not match the expected set (< expected, > found)" >&2
    exit 1
fi

# A rolling tag gives no version to pin, so the checksums are what a user can
# record and compare against the next download.
(cd dist && sha256sum "${expected[@]}" > SHA256SUMS)
