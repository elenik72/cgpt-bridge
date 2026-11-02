#!/usr/bin/env bash
#
# cgpt-bridge — checks for scripts/build-id.sh (run by CI on macOS and Linux
# and locally). Works on copies of the script in throwaway git repositories
# under a temporary directory; the real working tree is never touched.
#
# Usage:
#   ./scripts/check-build-id.sh

set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/build-id.sh"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/cgpt-build-id-check.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

FAILS=0
ok()   { echo "ok   - $*"; }
fail() { echo "FAIL - $*" >&2; FAILS=$((FAILS + 1)); }
eq() { # eq <description> <actual> <expected>
  if [[ "$2" == "$3" ]]; then ok "$1"; else fail "$1: got '$2', want '$3'"; fi
}
matches() { # matches <description> <actual> <regex>
  if [[ "$2" =~ $3 ]]; then ok "$1"; else fail "$1: got '$2', want match for $3"; fi
}

# A repository with the script copied in, one commit.
new_repo() {
  local dir="$WORK/$1"
  mkdir -p "$dir/scripts"
  cp "$SRC" "$dir/scripts/build-id.sh"
  printf 'ignored/\n' > "$dir/.gitignore"
  echo one > "$dir/file.txt"
  git -C "$dir" init -q
  git -C "$dir" add .gitignore file.txt scripts/build-id.sh
  git -C "$dir" -c user.name=t -c user.email=t@example.com commit -q -m one
  echo "$dir"
}
id_of() { (unset CGPT_BUILD; "$1/scripts/build-id.sh"); }

repo="$(new_repo clean)"
sha="$(git -C "$repo" rev-parse --short=12 HEAD)"
eq "clean tree: the 12-character short SHA" "$(id_of "$repo")" "$sha"

echo two > "$repo/file.txt"
eq "tracked file changed: -dirty" "$(id_of "$repo")" "$sha-dirty"
git -C "$repo" checkout -q -- file.txt
eq "change reverted: clean again" "$(id_of "$repo")" "$sha"

echo new > "$repo/new.txt"
eq "untracked file: -dirty" "$(id_of "$repo")" "$sha-dirty"
rm "$repo/new.txt"

mkdir "$repo/ignored" && echo x > "$repo/ignored/junk"
eq "ignored file only: still clean" "$(id_of "$repo")" "$sha"

echo two > "$repo/file.txt"
got="$(cd "$repo" && CGPT_BUILD=my.build_1 ./scripts/build-id.sh)"
eq "CGPT_BUILD wins and gets no suffix" "$got" "my.build_1"

matches "dirty id fits the 1-64 character rule" "$(id_of "$repo")" '^[A-Za-z0-9._-]{1,64}$'

# A long short SHA (git grows it when 12 is ambiguous): the suffix must not
# push the id past 64 characters. A fake `git` stands in for that.
bin="$WORK/fakebin"; mkdir -p "$bin"
cat > "$bin/git" <<'FAKE'
#!/usr/bin/env bash
case "$*" in
  *rev-parse*) printf '%s\n' "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" ;;
  *status*) echo " M file.txt" ;;
esac
FAKE
chmod +x "$bin/git"
got="$(cd "$repo" && unset CGPT_BUILD && PATH="$bin:$PATH" ./scripts/build-id.sh)"
matches "a very long SHA is cut so the dirty id stays valid" "$got" '^[0-9a-f]{58}-dirty$'

# Not a repository of its own: `unknown`, even inside another repository.
mkdir -p "$repo/inner/scripts"
cp "$SRC" "$repo/inner/scripts/build-id.sh"
eq "no own .git: unknown" "$(id_of "$repo/inner")" "unknown"

if [[ $FAILS -ne 0 ]]; then
  echo "$FAILS check(s) failed" >&2
  exit 1
fi
echo "all build-id checks passed"
