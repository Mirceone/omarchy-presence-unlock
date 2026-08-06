#!/usr/bin/env bash
# Build and install the current checkout. Safe to rerun after any source change.
set -euo pipefail

PROJECT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

cd "$PROJECT_DIR"

cargo build --release --workspace --locked

sudo install -Dm755 target/release/omarchy-presence-unlock /usr/bin/omarchy-presence-unlock
sudo install -Dm755 target/release/omarchy-presence-unlockd /usr/bin/omarchy-presence-unlockd
sudo install -Dm755 target/release/libpam_omarchy_presence_unlock.so /usr/lib/security/pam_omarchy_presence_unlock.so
sudo install -Dm644 packaging/omarchy-presence-unlockd.service /usr/lib/systemd/user/omarchy-presence-unlockd.service
sudo install -Dm644 packaging/omarchy-lock-presence.pam /usr/share/omarchy-presence-unlock/omarchy-lock-presence.pam

systemctl --user daemon-reload
systemctl --user enable --now omarchy-presence-unlockd.service
systemctl --user restart omarchy-presence-unlockd.service

echo "Installed current checkout and restarted omarchy-presence-unlockd."
