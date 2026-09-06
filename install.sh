#!/usr/bin/env bash
# Build and install the current checkout. Safe to rerun after source changes.
set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_SOURCE=${BASH_SOURCE[0]:-}
if [[ -n $SCRIPT_SOURCE ]]; then
  PROJECT_DIR=$(cd -- "$(dirname -- "$SCRIPT_SOURCE")" && pwd)
else
  PROJECT_DIR=
fi
REPOSITORY=${OPU_REPOSITORY:-Mirceone/omarchy-presence-unlock}
REF=${OPU_REF:-main}

die() {
  printf 'install: %s\n' "$*" >&2
  exit 1
}

if (( EUID == 0 )); then
  die "run this script as your normal desktop user, not with sudo; it will request sudo only when installing files"
fi

# A piped installer has no checkout beside it. Download the selected source
# revision into a temporary directory, then let that copy build normally.
if [[ -z $PROJECT_DIR || ! -f $PROJECT_DIR/Cargo.lock ]]; then
  for command in curl tar mktemp; do
    command -v "$command" >/dev/null 2>&1 || die "required bootstrap command not found: $command"
  done

  BOOTSTRAP_DIR=$(mktemp -d)
  trap 'rm -rf "$BOOTSTRAP_DIR"' EXIT
  printf 'Downloading %s at %s...\n' "$REPOSITORY" "$REF"
  curl -fsSL --retry 3 \
    "https://codeload.github.com/$REPOSITORY/tar.gz/$REF" \
    | tar -xz --strip-components=1 -C "$BOOTSTRAP_DIR"
  bash "$BOOTSTRAP_DIR/install.sh"
  exit
fi

for command in cargo install sudo; do
  command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

[[ $(uname -m) == x86_64 ]] || die "this project currently supports x86-64 only"

# Overwriting a packaged file succeeds silently and then rots: pacman reports
# the file as modified, and the next upgrade replaces this build without
# saying so. Fail before touching anything instead.
if command -v pacman >/dev/null 2>&1; then
  for owned in /usr/bin/omarchy-presence-unlock /usr/lib/security/pam_omarchy_presence_unlock.so; do
    if package=$(pacman --query --owns --quiet "$owned" 2>/dev/null) && [[ -n $package ]]; then
      die "the $package package owns $owned; install with pacman instead, or remove it first: sudo pacman -Rns $package"
    fi
  done
fi

cd "$PROJECT_DIR"

required_sources=(
  Cargo.lock
  packaging/presenced.service
  packaging/presenced.path
  packaging/omarchy-lock-presence.pam
  packaging/plugin/manifest.json
  packaging/plugin/Service.qml
  README.md
  LICENSE
)
for source in "${required_sources[@]}"; do
  [[ -f $source ]] || die "required source file is missing: $source"
done

# Authenticate before system changes so a cancelled or timed-out password
# prompt leaves the existing installation untouched.
sudo -v

printf 'Building release artifacts...\n'
cargo build --release --workspace --locked

printf 'Installing system files...\n'
sudo install -Dm755 target/release/omarchy-presence-unlock /usr/bin/omarchy-presence-unlock
sudo install -Dm755 target/release/presenced /usr/bin/presenced
sudo install -Dm755 target/release/libpam_omarchy_presence_unlock.so /usr/lib/security/pam_omarchy_presence_unlock.so
sudo install -Dm644 packaging/presenced.service /usr/lib/systemd/user/presenced.service
sudo install -Dm644 packaging/presenced.path /usr/lib/systemd/user/presenced.path
sudo install -Dm644 packaging/omarchy-lock-presence.pam /etc/pam.d/omarchy-lock-presence
sudo install -Dm644 packaging/plugin/manifest.json /usr/share/omarchy-presence-unlock/plugin/manifest.json
sudo install -Dm644 packaging/plugin/Service.qml /usr/share/omarchy-presence-unlock/plugin/Service.qml
sudo install -Dm644 README.md /usr/share/doc/omarchy-presence-unlock/README.md
sudo install -Dm644 LICENSE /usr/share/licenses/omarchy-presence-unlock/LICENSE

required_installed=(
  /usr/bin/omarchy-presence-unlock
  /usr/bin/presenced
  /usr/lib/security/pam_omarchy_presence_unlock.so
  /usr/lib/systemd/user/presenced.service
  /usr/lib/systemd/user/presenced.path
  /etc/pam.d/omarchy-lock-presence
  /usr/share/omarchy-presence-unlock/plugin/manifest.json
  /usr/share/omarchy-presence-unlock/plugin/Service.qml
)
for path in "${required_installed[@]}"; do
  [[ -f $path ]] || die "installation failed; expected file missing: $path"
done
command -v omarchy-presence-unlock >/dev/null 2>&1 \
  || die "installation succeeded, but /usr/bin is not on PATH"

if [[ -n ${XDG_RUNTIME_DIR:-} ]] \
  && command -v omarchy-shell >/dev/null 2>&1 \
  && omarchy-shell shell ping >/dev/null 2>&1; then
  printf 'Applying per-user Omarchy integration...\n'
  omarchy-presence-unlock setup
  printf '\nInstalled Omarchy Presence Unlock and applied per-user integration.\n'
else
  printf '\nInstalled Omarchy Presence Unlock system files.\n'
  printf 'Run omarchy-presence-unlock setup from inside your logged-in Omarchy session.\n'
fi

