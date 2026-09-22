#!/usr/bin/env bash
# Build the checkout into a package; ask before replacing an existing install.
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
for command in cargo git makepkg pacman sudo realpath; do
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

# These are the exact system files written by the earlier direct-copy
# installers, including the former daemon name and companion plugin. Never
# remove a whole directory: it might contain files this installer did not own.
installed_files=(
  /usr/bin/omarchy-presence-unlock
  /usr/bin/presenced
  /usr/bin/omarchy-presence-unlockd
  /usr/lib/security/pam_omarchy_presence_unlock.so
  /usr/lib/systemd/user/presenced.service
  /usr/lib/systemd/user/presenced.path
  /usr/lib/systemd/user/omarchy-presence-unlockd.service
  /etc/pam.d/omarchy-lock-presence
  /usr/share/omarchy-presence-unlock/omarchy-lock-presence.pam
  /usr/share/omarchy-presence-unlock/plugin/manifest.json
  /usr/share/omarchy-presence-unlock/plugin/Service.qml
  /usr/share/doc/omarchy-presence-unlock/README.md
  /usr/share/licenses/omarchy-presence-unlock/LICENSE
)

check_ownership() {
  local path owner
  for path in "${installed_files[@]}"; do
    [[ -e $path || -L $path ]] || continue
    [[ -f $path && ! -L $path ]] \
      || die "unexpected file type at $path; refusing to replace it"
    if owner=$(pacman --query --owns --quiet "$path" 2>/dev/null); then
      [[ $owner == omarchy-presence-unlock ]] \
        || die "$path belongs to $owner, not this application; refusing to overwrite it"
    fi
  done
}

installed_package=$(pacman --query omarchy-presence-unlock 2>/dev/null || true)
replace=false
if [[ -n $installed_package ]]; then
  replace=true
fi
for path in "${installed_files[@]}"; do
  if [[ -e $path || -L $path ]]; then
    replace=true
  fi
done
check_ownership

# A second copy earlier on PATH would still open the old app after replacement.
# Do not delete arbitrary executables outside the historical install locations.
if executable=$(type -P omarchy-presence-unlock) \
  && [[ ! $executable -ef /usr/bin/omarchy-presence-unlock ]]; then
  die "$executable shadows /usr/bin/omarchy-presence-unlock; remove that separate installation first"
fi

if $replace; then
  printf 'Existing installation: %s\n' "${installed_package:-unpackaged system files from an earlier installer}"
  # The installed version knows its own plugin/binding layout. Some old
  # uninstallers remove only the integration; others also remove the package.
  # Versions without an uninstaller need manual cleanup, not guessed deletion.
  [[ -x /usr/bin/omarchy-presence-unlock ]] \
    || die "the existing installation has no executable uninstaller; repair it before replacing it"
  /usr/bin/omarchy-presence-unlock uninstall --help >/dev/null 2>&1 \
    || die "this version has no supported uninstall command; remove its integration before replacing it"
  printf 'Replacement removes the old application, then installs this build.\n'
  printf 'Enrolled devices are preserved. Package replacement affects all users.\n'
  printf 'The new package will be built before any installed files are removed.\n'
  # stdin contains the script in curl | bash, not the user's answer.
  if ! { exec 3<>/dev/tty; } 2>/dev/null; then
    die "replacement requires an interactive terminal; nothing was changed"
  fi
  printf 'Overwrite the existing installation? [y/N] ' >&3
  answer=
  IFS= read -r answer <&3 || true
  exec 3>&-
  case $answer in
    [yY]|[yY][eE][sS]) ;;
    *) printf 'Installation cancelled; the existing installation was not changed.\n'; exit 0 ;;
  esac
fi

printf 'Building the package...\n'
cd packaging/local
makepkg --force --cleanbuild
package_list=$(makepkg --packagelist)
mapfile -t package_files <<<"$package_list"
[[ ${#package_files[@]} -eq 1 ]] || die "expected exactly one replacement package"
package_file=$(realpath -- "${package_files[0]}")
[[ -f $package_file ]] || die "makepkg reported $package_file but it does not exist"
package_name=$(pacman --query --file --quiet "$package_file") \
  || die "the built package cannot be read; the existing installation was not changed"
[[ $package_name == omarchy-presence-unlock ]] \
  || die "the built package is $package_name, not omarchy-presence-unlock"
cd "$PROJECT_DIR"

# Authenticate after the build: a cancelled password prompt then costs a build
# rather than leaving a partially installed system.
sudo --validate
if $replace; then
  check_ownership
  # From here the old package may be gone before the new one lands. If the
  # script stops in that gap, keep the built package (a curl | bash build
  # lives in a temporary directory) and say how to finish.
  report_missing_package() {
    pacman --query omarchy-presence-unlock >/dev/null 2>&1 && return
    [[ -x /usr/bin/omarchy-presence-unlock ]] && return
    local keep=${XDG_CACHE_HOME:-$HOME/.cache}/omarchy-presence-unlock
    mkdir -p -- "$keep" && cp -f -- "$package_file" "$keep/" || keep=${package_file%/*}
    printf 'install: the previous installation was removed, but the replacement was not installed.\n' >&2
    printf 'install: finish with: sudo pacman -U %s\n' "$keep/${package_file##*/}" >&2
  }
  trap report_missing_package EXIT
  printf 'Removing the existing installation, keeping enrolled devices...\n'
  /usr/bin/omarchy-presence-unlock uninstall \
    || die "existing installation cleanup failed; replacement was not installed"
  if [[ -f /usr/lib/systemd/user/omarchy-presence-unlockd.service ]]; then
    systemctl --user disable --now omarchy-presence-unlockd.service
  fi
  if pacman --query omarchy-presence-unlock >/dev/null 2>&1; then
    sudo pacman --remove --nosave --noconfirm -- omarchy-presence-unlock
  fi
  # Older uninstallers leave system files behind. Check again after package
  # removal and remove only unowned files from the historical installer list.
  check_ownership
  for path in "${installed_files[@]}"; do
    [[ -e $path || -L $path ]] || continue
    if pacman --query --owns --quiet "$path" >/dev/null 2>&1; then
      die "$path is still package-owned after uninstall; refusing to delete it"
    fi
    sudo rm -- "$path"
  done
fi
printf 'Installing %s...\n' "$(basename "$package_file")"
sudo pacman --upgrade --noconfirm "$package_file"
trap - EXIT

[[ -x /usr/bin/omarchy-presence-unlock ]] \
  || die "the package installed, but its executable is missing"

# The per-user half needs a session: pacman has none, and neither does an
# install over SSH, so it is applied here when there is one to apply it to.
if [[ -n ${XDG_RUNTIME_DIR:-} ]] \
  && command -v omarchy-shell >/dev/null 2>&1 \
  && omarchy-shell shell ping >/dev/null 2>&1; then
  printf 'Applying per-user Omarchy integration...\n'
  /usr/bin/omarchy-presence-unlock setup
  printf '\nInstalled Omarchy Presence Unlock and applied per-user integration.\n'
  printf 'Run omarchy-presence-unlock to enroll a device.\n'
else
  printf '\nInstalled the Omarchy Presence Unlock package.\n'
  printf 'Run omarchy-presence-unlock setup from inside your logged-in Omarchy session.\n'
fi
