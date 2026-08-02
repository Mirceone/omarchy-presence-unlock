#!/usr/bin/env bash
# Build and install the current checkout. Safe to rerun after any source change.
set -euo pipefail

PROJECT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

cd "$PROJECT_DIR"

cargo build --release --workspace --locked

sudo install -Dm755 target/release/omarchy-watch-unlock /usr/bin/omarchy-watch-unlock
sudo install -Dm755 target/release/omarchy-watch-unlockd /usr/bin/omarchy-watch-unlockd
sudo install -Dm755 target/release/libpam_omarchy_watch_unlock.so /usr/lib/security/pam_omarchy_watch_unlock.so
sudo install -Dm644 packaging/omarchy-watch-unlockd.service /usr/lib/systemd/user/omarchy-watch-unlockd.service
sudo install -Dm644 packaging/omarchy-lock-watch.pam /usr/share/omarchy-watch-unlock/omarchy-lock-watch.pam

systemctl --user daemon-reload
systemctl --user enable --now omarchy-watch-unlockd.service
systemctl --user restart omarchy-watch-unlockd.service

echo "Installed current checkout and restarted omarchy-watch-unlockd."
