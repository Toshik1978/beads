#!/usr/bin/env bash
#
# Print the CHANGELOG.md section for a release tag.
#
# Used by .github/workflows/release.yml to build the GitHub release body, and
# runnable locally to preview that body before pushing a tag:
#
#   .github/scripts/extract-changelog.sh v1.0.0
#
# Exits 1 if the tag has no section, which is the guard against tagging before
# the release notes are written.

set -euo pipefail

tag="${1:-}"
changelog="${2:-CHANGELOG.md}"

if [ -z "$tag" ]; then
  echo "usage: $(basename "$0") <tag> [changelog-path]" >&2
  exit 2
fi

if [ ! -f "$changelog" ]; then
  echo "error: $changelog not found" >&2
  exit 2
fi

# Match the heading by literal prefix rather than regex: the tag contains dots,
# which a dynamic awk regex would treat as wildcards (v1.5.0 would also match
# v1050). Headings are "## <tag> — <date>"; the bare "## <tag>" form is accepted
# too. Printing stops at the next "## " heading, so exactly one section emerges.
section=$(awk -v tag="$tag" '
  $0 == "## " tag || index($0, "## " tag " ") == 1 { found = 1; print; next }
  found && index($0, "## ") == 1 { exit }
  found { print }
' "$changelog")

# Avoid `${section//[[:space:]]/}` here: bash 3.2 (still the default
# /bin/bash on macOS) has a catastrophic-slowdown bug in its pattern-
# substitution engine on strings of this size, so this check would hang
# for several minutes on the author's own machine for real release
# sections. `tr -d` is POSIX and linear-time on every bash this script
# runs under, so do not "simplify" this back to a parameter expansion.
if [ -z "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
  echo "error: no '## $tag' section found in $changelog" >&2
  echo "hint: write the release notes before tagging" >&2
  exit 1
fi

printf '%s\n' "$section"
