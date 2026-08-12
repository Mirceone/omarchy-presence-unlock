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
UNIT=presenced.service
PATH_UNIT=presenced.path
USER_CONFIG_HOME=${XDG_CONFIG_HOME:-${HOME:?HOME is unset}}
CONFIG_FILE=$USER_CONFIG_HOME/omarchy-presence-unlock/config.toml
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

for command in cargo install sudo systemctl; do
  command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

[[ $(uname -m) == x86_64 ]] || die "this project currently supports x86-64 only"
[[ -n ${XDG_RUNTIME_DIR:-} ]] || die "XDG_RUNTIME_DIR is unset; run the installer from your logged-in desktop session"

cd "$PROJECT_DIR"

required_sources=(
  Cargo.lock
  packaging/presenced.service
  packaging/presenced.path
  packaging/omarchy-lock-presence.pam
  README.md
  LICENSE
)
for source in "${required_sources[@]}"; do
  [[ -f $source ]] || die "required source file is missing: $source"
done

printf 'Building release artifacts...\n'
cargo build --release --workspace --locked

required_artifacts=(
  target/release/omarchy-presence-unlock
  target/release/presenced
  target/release/libpam_omarchy_presence_unlock.so
)
for artifact in "${required_artifacts[@]}"; do
  [[ -f $artifact ]] || die "build did not produce $artifact"
done

# Authenticate before changing the system so a bad or cancelled credential
# prompt leaves the existing installation untouched.
sudo -v

printf 'Installing system files...\n'
sudo install -Dm755 target/release/omarchy-presence-unlock /usr/bin/omarchy-presence-unlock
sudo install -Dm755 target/release/presenced /usr/bin/presenced
sudo install -Dm755 target/release/libpam_omarchy_presence_unlock.so /usr/lib/security/pam_omarchy_presence_unlock.so
sudo install -Dm644 packaging/presenced.service /usr/lib/systemd/user/presenced.service
sudo install -Dm644 packaging/presenced.path /usr/lib/systemd/user/presenced.path
sudo install -Dm644 packaging/omarchy-lock-presence.pam /usr/share/omarchy-presence-unlock/omarchy-lock-presence.pam
sudo install -Dm644 README.md /usr/share/doc/omarchy-presence-unlock/README.md
sudo install -Dm644 LICENSE /usr/share/licenses/omarchy-presence-unlock/LICENSE

# Clean cutover from releases that used the project-specific daemon name.
systemctl --user disable --now omarchy-presence-unlockd.service >/dev/null 2>&1 || true
sudo rm -f /usr/bin/omarchy-presence-unlockd /usr/lib/systemd/user/omarchy-presence-unlockd.service

printf 'Enabling the user service and configuration watcher...\n'
systemctl --user daemon-reload
systemctl --user enable "$UNIT" "$PATH_UNIT"
if [[ -f $CONFIG_FILE ]]; then
  if ! systemctl --user restart "$UNIT"; then
    systemctl --user status --no-pager "$UNIT" >&2 || true
    die "$UNIT was installed but could not be restarted"
  fi
  if ! systemctl --user is-active --quiet "$UNIT"; then
    systemctl --user status --no-pager "$UNIT" >&2 || true
    die "$UNIT was installed but did not remain active"
  fi
  SERVICE_RESULT="restarted $UNIT"
else
  # A daemon with no enrolled devices deliberately refuses to run. Leave the
  # enabled unit stopped; the path unit starts it when configuration appears.
  systemctl --user stop "$UNIT"
  SERVICE_RESULT="enabled $UNIT; it will start automatically after you enroll a device"
fi
systemctl --user restart "$PATH_UNIT"

command -v omarchy-presence-unlock >/dev/null 2>&1 \
  || die "installation succeeded, but /usr/bin is not on PATH"

printf '\nInstalled Omarchy Presence Unlock and %s.\n' "$SERVICE_RESULT"
printf 'Run omarchy-presence-unlock to open the setup menu.\n'
