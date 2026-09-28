#!/bin/bash
# Stage one runner release into a platform checkout. It writes files and
# commits nothing: review the diff, run the platform's tests, and commit it as
# one deploy.
#
#   dev/publish-release.sh --platform ../claude-daycare-platform \
#       --release <id> --artifacts <dir> [--base-url <url>]
#
# <dir> holds one binary per target, named daycare-runner-<rust target triple>
# (what the CI workflow uploads; `gh run download <run> -D <dir>` fetches them,
# one subdirectory per artifact, and nested files are found). Every binary
# must be stamped with the same release id (`DAYCARE_RUNNER_RELEASE=<id>`).
#
# For each target it writes, in the platform checkout:
#   public/releases/daycare-runner-<id>-<sha8>           aarch64-apple-darwin
#   public/releases/daycare-runner-<id>-<target>-<sha8>  every other target
#   public/releases/current.json   {release, targets: {<triple>: {url, sha256}}}
#   public/releases/current.txt    the release id (runners before current.json)
#   public/install.sh              RUNNER_VERSION, RUNNER_URL/RUNNER_SHA256 (the
#                                  Apple Silicon build), and
#                                  RUNNER_URL_<TRIPLE>/RUNNER_SHA256_<TRIPLE>
#                                  for each other target, triple upper-cased
#                                  with '-' as '_'
#   src/lib/daycare/runnerRelease.ts   CURRENT_RUNNER_RELEASE
#
# --base-url changes where current.json and install.sh point (default
# https://claudedaycare.com/releases). With any other base the binaries are
# not copied into the platform; upload them there yourself, e.g. with
# dev/github-release.sh. Moving hosting is Josh's call and not made yet, and
# the platform's current.json test also assumes the default base.

set -euo pipefail

platform=""
release=""
artifacts=""
base_url="https://claudedaycare.com/releases"

while [ $# -gt 0 ]; do
  case "$1" in
    --platform) platform="$2"; shift 2 ;;
    --release) release="$2"; shift 2 ;;
    --artifacts) artifacts="$2"; shift 2 ;;
    --base-url) base_url="${2%/}"; shift 2 ;;
    -h | --help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "publish-release: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -n "$platform" ] && [ -n "$release" ] && [ -n "$artifacts" ] || {
  echo "usage: $0 --platform <dir> --release <id> --artifacts <dir> [--base-url <url>]" >&2
  exit 2
}
[ -f "$platform/public/install.sh" ] || { echo "publish-release: $platform is not a platform checkout" >&2; exit 1; }

exec python3 - "$platform" "$release" "$artifacts" "$base_url" <<'PY'
import hashlib, json, pathlib, re, shutil, subprocess, sys

platform, release, artifacts, base_url = sys.argv[1:5]
platform = pathlib.Path(platform)
artifacts = pathlib.Path(artifacts)
releases = platform / "public" / "releases"
default_base = base_url == "https://claudedaycare.com/releases"
LEGACY = "aarch64-apple-darwin"

def fail(message):
    sys.exit(f"publish-release: {message}")

if not re.fullmatch(r"[0-9a-z]{7,40}", release):
    fail(f"release id {release!r} does not look like a short commit hash")

binaries = {}
for path in sorted(artifacts.rglob("daycare-runner-*")):
    if path.suffix == ".sha256" or not path.is_file():
        continue
    target = path.name[len("daycare-runner-"):]
    if target in binaries:
        fail(f"two binaries for {target}: {binaries[target]} and {path}")
    binaries[target] = path
if not binaries:
    fail(f"no daycare-runner-<target> binaries under {artifacts}")
if LEGACY not in binaries:
    fail(f"no {LEGACY} build: install.sh's RUNNER_URL and older runners need one")

# The id is baked in as a string constant; `--version` formats it at run
# time, so the bytes of the id are what a foreign-arch binary can be checked
# for. A binary this machine can run is asked directly.
targets = {}
for target, path in binaries.items():
    data = path.read_bytes()
    if release.encode() not in data:
        fail(f"{path} does not carry release {release}; rebuild with DAYCARE_RUNNER_RELEASE={release}")
    try:
        path.chmod(0o755)
        version = subprocess.run([str(path), "--version"], capture_output=True, text=True, timeout=10)
    except OSError:
        version = None  # another OS or architecture
    if version is not None and version.returncode == 0 and f"(release {release})" not in version.stdout:
        fail(f"{path} --version says {version.stdout.strip()!r}, not release {release}")
    sha = hashlib.sha256(data).hexdigest()
    name = (
        f"daycare-runner-{release}-{sha[:8]}"
        if target == LEGACY
        else f"daycare-runner-{release}-{target}-{sha[:8]}"
    )
    if default_base:
        destination = releases / name
        shutil.copyfile(path, destination)
        destination.chmod(0o755)
    targets[target] = {"url": f"{base_url}/{name}", "sha256": sha}
    print(f"  {target:30} {name}  {sha}")

(releases / "current.json").write_text(
    json.dumps({"release": release, "targets": targets}, indent=2) + "\n"
)
(releases / "current.txt").write_text(release + "\n")

def pin(text, name, value):
    pattern = re.compile(rf'^{name}="[^"]*"$', re.M)
    if not pattern.search(text):
        fail(f'install.sh has no {name}="..." line; add it before publishing this target')
    return pattern.sub(f'{name}="{value}"', text)

install = platform / "public" / "install.sh"
script = install.read_text()
script = pin(script, "RUNNER_VERSION", release)
for target, build in targets.items():
    suffix = "" if target == LEGACY else "_" + target.upper().replace("-", "_")
    script = pin(script, f"RUNNER_URL{suffix}", build["url"])
    script = pin(script, f"RUNNER_SHA256{suffix}", build["sha256"])
# A target this release does not carry must not keep an older release's pin.
known = {t.upper().replace("-", "_") for t in targets}
for match in list(re.finditer(r'^RUNNER_URL_([A-Z0-9_]+)="[^"]*"$', script, re.M)):
    if match.group(1) not in known:
        script = pin(script, f"RUNNER_URL_{match.group(1)}", "")
        script = pin(script, f"RUNNER_SHA256_{match.group(1)}", "")
install.write_text(script)

ts = platform / "src" / "lib" / "daycare" / "runnerRelease.ts"
source = ts.read_text()
source, count = re.subn(
    r'^export const CURRENT_RUNNER_RELEASE = "[^"]*";$',
    f'export const CURRENT_RUNNER_RELEASE = "{release}";',
    source,
    flags=re.M,
)
if count != 1:
    fail("runnerRelease.ts has no single CURRENT_RUNNER_RELEASE line")
ts.write_text(source)

print(f"\nStaged release {release} for {len(targets)} target(s) in {platform}.")
if not default_base:
    print(f"Binaries were NOT copied: upload them under {base_url}/ first.")
print("Next: review `git -C <platform> status`, delete the previous release's")
print("binaries if you mean to, run the platform tests, and commit as one deploy.")
PY
