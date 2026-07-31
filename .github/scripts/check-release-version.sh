#!/usr/bin/env bash
#
# Fail unless the release tag matches the version in Cargo.toml.
#
#   .github/scripts/check-release-version.sh v1.0.0
#
# Or print the manifest's version, so that callers needing it do not
# reimplement the parsing below:
#
#   .github/scripts/check-release-version.sh --manifest-version
#
# WHY THIS EXISTS
#
# GoReleaser takes the release version from the git tag, but the version
# compiled *into* the binary comes from Cargo.toml — cargo exposes it as
# CARGO_PKG_VERSION, and `br version` prints it. Nothing connects the two.
#
# So tagging v1.1.0 against a Cargo.toml still saying 1.0.0 publishes an
# archive named br_1.1.0_<target>.tar.gz containing a binary that reports
# `br 1.0.0`. Both halves succeed; the release is simply wrong, and it is
# wrong in a way that is only discovered by a user running --version. This
# runs before anything is built or uploaded so the tag fails while it is
# still a no-op.

set -euo pipefail

tag="${1:-}"
manifest="${2:-Cargo.toml}"

if [ -z "$tag" ]; then
  echo "usage: $(basename "$0") <tag>|--manifest-version [manifest-path]" >&2
  exit 2
fi

if [ ! -f "$manifest" ]; then
  echo "error: $manifest not found" >&2
  exit 2
fi

# Read the version from the [package] table only. A bare `grep '^version'`
# over the whole file would also match the `version` key of any dependency
# written in table form, and would silently pick whichever came first.
manifest_version=$(awk '
  /^\[/ { in_package = ($0 == "[package]"); next }
  in_package && /^version[[:space:]]*=/ {
    # version = "1.0.0"  ->  1.0.0
    gsub(/^version[[:space:]]*=[[:space:]]*"/, "")
    gsub(/".*$/, "")
    print
    exit
  }
' "$manifest")

if [ -z "$manifest_version" ]; then
  echo "error: no [package] version found in $manifest" >&2
  exit 2
fi

if [ "$tag" = "--manifest-version" ]; then
  printf '%s\n' "$manifest_version"
  exit 0
fi

# The tag is the version with a leading 'v'. Pre-release suffixes are carried
# through unchanged, so v0.2.0-rc.1 requires Cargo.toml to say 0.2.0-rc.1 —
# cargo accepts semver pre-release versions, so this is a real requirement
# rather than an approximation.
tag_version="${tag#v}"

if [ "$tag_version" != "$manifest_version" ]; then
  echo "error: tag '$tag' does not match the version in $manifest" >&2
  echo "  tag $tag says:   $tag_version" >&2
  echo "  $manifest says:  $manifest_version" >&2
  echo "hint: bump the version in $manifest and commit it before tagging," >&2
  echo "      or delete the tag and re-tag the corrected commit." >&2
  exit 1
fi

echo "release version: $manifest_version (tag $tag)"
