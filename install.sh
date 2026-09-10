#!/usr/bin/env bash
# Build the current checkout into a package and install it. Safe to rerun.
#
# Everything lands through pacman, exactly as the AUR package does, so every
# installed file has one owner. A hand-rolled copy into /usr would be unowned,
# which makes a later package install fail on file conflicts and lets a package
# upgrade silently replace a local build. Two of the four artifacts -- the PAM
# module and the systemd user units -- must live under /usr because libpam and
# systemd only search there, so /usr/local is not available as a middle ground.
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
  die "run this script as your normal desktop user, not with sudo; makepkg refuses to run as root and it requests sudo only to install the package"
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

# pacman and makepkg are guaranteed rather than assumed: this depends on the
# omarchy package, which exists only on Arch.
for command in cargo git makepkg pacman sudo; do
  command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

[[ $(uname -m) == x86_64 ]] || die "this project currently supports x86-64 only"

cd "$PROJECT_DIR"

required_sources=(
  Cargo.lock
  packaging/local/PKGBUILD
  packaging/omarchy-presence-unlock.install
  packaging/presenced.service
  packaging/presenced.path
  packaging/omarchy-lock-presence.pam
  README.md
  LICENSE
)
for source in "${required_sources[@]}"; do
  [[ -f $source ]] || die "required source file is missing: $source"
done

printf 'Building the package...\n'
cd packaging/local
makepkg --force --cleanbuild
package_file=$(makepkg --packagelist | head --lines 1)
[[ -f $package_file ]] || die "makepkg reported $package_file but it does not exist"
cd "$PROJECT_DIR"

# Authenticate after the build: a cancelled password prompt then costs a build
# rather than leaving a partially installed system.
printf 'Installing %s...\n' "$(basename "$package_file")"
sudo pacman --upgrade --noconfirm "$package_file"

command -v omarchy-presence-unlock >/dev/null 2>&1 \
  || die "the package installed, but /usr/bin is not on PATH"

# The per-user half needs a session: pacman has none, and neither does an
# install over SSH, so it is applied here when there is one to apply it to.
if [[ -n ${XDG_RUNTIME_DIR:-} ]] \
  && command -v omarchy-shell >/dev/null 2>&1 \
  && omarchy-shell shell ping >/dev/null 2>&1; then
  printf 'Applying per-user Omarchy integration...\n'
  omarchy-presence-unlock setup
  printf '\nInstalled Omarchy Presence Unlock and applied per-user integration.\n'
  printf 'Run omarchy-presence-unlock to enroll a device.\n'
else
  printf '\nInstalled the Omarchy Presence Unlock package.\n'
  printf 'Run omarchy-presence-unlock setup from inside your logged-in Omarchy session.\n'
fi
