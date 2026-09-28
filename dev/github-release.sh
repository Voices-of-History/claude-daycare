#!/bin/bash
# Prepared, not in use: host runner binaries on GitHub Releases of this public
# repo instead of in the platform's git. Josh has not decided to move hosting.
# Until he does, dev/publish-release.sh copies binaries into the platform.
#
#   dev/github-release.sh --release <id> --artifacts <dir> [--yes]
#
# Without --yes it only prints what it would run. With --yes it creates a
# *draft* release tagged runner-<id> on Voices-of-History/claude-daycare with
# the binaries under the names publish-release.sh gives them. Then:
#
#   dev/publish-release.sh --platform <dir> --release <id> --artifacts <dir> \
#     --base-url https://github.com/Voices-of-History/claude-daycare/releases/download/runner-<id>
#
# and publish the draft before the platform deploy goes out. The platform's
# current.json test pins URLs to claudedaycare.com and reads the binaries from
# public/releases; it must change in the same deploy as the move.

set -euo pipefail

repo="Voices-of-History/claude-daycare"
release=""
artifacts=""
yes=0
while [ $# -gt 0 ]; do
  case "$1" in
    --release) release="$2"; shift 2 ;;
    --artifacts) artifacts="$2"; shift 2 ;;
    --yes) yes=1; shift ;;
    *) echo "github-release: unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$release" ] && [ -n "$artifacts" ] || {
  echo "usage: $0 --release <id> --artifacts <dir> [--yes]" >&2
  exit 2
}

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
assets=()
while IFS= read -r path; do
  target="${path##*/daycare-runner-}"
  sha="$(sha256 "$path")"
  if [ "$target" = "aarch64-apple-darwin" ]; then
    name="daycare-runner-$release-${sha:0:8}"
  else
    name="daycare-runner-$release-$target-${sha:0:8}"
  fi
  cp "$path" "$stage/$name"
  assets+=("$stage/$name")
  echo "  $target -> $name"
done < <(find "$artifacts" -type f -name 'daycare-runner-*' ! -name '*.sha256' | sort)
[ ${#assets[@]} -gt 0 ] || { echo "github-release: no binaries under $artifacts" >&2; exit 1; }

cmd=(gh release create "runner-$release" --repo "$repo" --draft
  --title "daycare-runner $release" --notes "Runner release $release." "${assets[@]}")
if [ "$yes" -ne 1 ]; then
  printf 'would run:'; printf ' %q' "${cmd[@]}"; echo
  exit 0
fi
"${cmd[@]}"
