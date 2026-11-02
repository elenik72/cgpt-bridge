#!/usr/bin/env bash
#
# cgpt-bridge — the build id.
#
# Prints the id that goes into the binaries (`pong`, the log lines), the
# extension (`status_result`, manifest `version_name`) and the bundle name:
# $CGPT_BUILD when set, else the commit's 12-character short SHA, else
# `unknown` (a source tree without .git, such as an unpacked tarball). A
# build from a tree with uncommitted changes (tracked or untracked, not
# ignored) gets the suffix `-dirty` after the SHA: that id no longer names
# one commit.
# A plain `cargo build` / `npm run build` without CGPT_BUILD says `dev`.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -n "${CGPT_BUILD:-}" ]]; then
  build="$CGPT_BUILD"
# Only this tree's own .git: a tarball unpacked inside another repository
# must not take that repository's commit.
elif [[ -e "$ROOT/.git" ]] && build="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null)"; then
  # A failing `git status` counts as dirty. The SHA part is cut so that the
  # suffix stays within the 64 characters checked below (a short SHA can grow
  # past 12 when 12 is ambiguous).
  if changes="$(git -C "$ROOT" status --porcelain 2>/dev/null)" && [[ -z "$changes" ]]; then
    :
  else
    build="${build:0:58}-dirty"
  fi
else
  build="unknown"
fi

# It ends up in file names and log lines.
if [[ ! "$build" =~ ^[A-Za-z0-9._-]{1,64}$ ]]; then
  echo "error: build id '$build' must be 1-64 characters of A-Z a-z 0-9 . _ -" >&2
  exit 1
fi
echo "$build"
