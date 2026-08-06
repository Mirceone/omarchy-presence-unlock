# Omarchy Watch Unlock

Unlock Omarchy with a nearby Bluetooth device. Apple Watch is the
best-supported option; phones, bands, fobs, and other BLE devices can also be
used for proximity-only authorization.

This is a convenience feature, not a replacement for your password or
fingerprint. It does not defend against Bluetooth relay/replay attacks or a
process that already has access to your user session. Password and fingerprint
authentication remain available.

## Requirements

- Omarchy on x86-64 Arch Linux, with BlueZ, PAM, and a systemd user session
- a working Bluetooth LE adapter managed by `bluetoothd`
- Rust 1.88 or newer and Cargo when installing from source
- `sudo` access for installation and Apple Watch enrollment

Apple Watch enrollment happens entirely on Linux; no Mac is required.

## Install

```sh
git clone https://github.com/mirceone/omarchy-watch-unlock.git
cd omarchy-watch-unlock
./install.sh
```

The installer builds and installs the CLI, daemon, PAM module, and systemd user
service under `/usr`. It enables the daemon but does not enroll a device or
change your lock-screen configuration. Re-running it after source changes is
safe.

## Set up with the wizard

```sh
omarchy-watch-unlock init
```

Running `omarchy-watch-unlock` without a subcommand opens the same full-screen
wizard in a terminal. It handles enrollment, enrolled-device management,
unlock settings, lock-screen integration, diagnostics, and live status.

The controls are consistent throughout the wizard:

| Key | Action |
| --- | --- |
| `Enter` | Select or continue |
| `Esc` | Cancel the current operation or go back |
| `Ctrl+C` | Leave the wizard |

Pairing waits end on success, timeout, error, or `Esc`, then show a result
screen with the next available actions. You do not need `Ctrl+C` to dismiss a
completed operation.

### Apple Watch

Choose **Apple Watch** in the enrollment menu and follow the checklist. When
prompted, open **Settings > Bluetooth > Health Devices** on the Watch, select
the computer's Bluetooth name, and accept pairing.

The wizard temporarily pauses the unlock daemon, advertises the computer as a
Heart Rate Sensor, captures and verifies the Watch identity, then attempts to
clean up the temporary Bluetooth state and restart the daemon. Any cleanup
failure is shown in the result. A narrowly scoped `sudo` helper reads the
kernel IRK event; do not run the whole CLI as root.

Apple Watch is the only built-in profile that also reports whether the device
itself is locked. A locked Watch, or one with Apple Auto Unlock disabled, does
not authorize an unlock.

### Other Bluetooth devices

Choose **Other Bluetooth device** to scan and select a nearby advertiser. This
does not pair with the device: it saves its address for proximity detection.

These devices are **proximity only**. Their presence can authorize an unlock,
but they cannot prove that they are unlocked. Devices that rotate private
addresses cannot be tracked reliably by address and need an IRK instead.

## Finish and verify setup

The wizard can install the correct lock-screen integration, or you can run:

```sh
omarchy-watch-unlock setup-omarchy
systemctl --user restart omarchy-watch-unlockd
omarchy-watch-unlock doctor
omarchy-watch-unlock status
```

On Hyprlock, press `Alt+Enter` while the lock screen is visible to confirm an
unlock. Omarchy Quattro builds use the local lock plugin and its dedicated PAM
policy instead. `status` prints each device's decision followed by the overall
quorum result.

## Useful CLI commands

Scan and enroll proximity devices manually:

```sh
omarchy-watch-unlock devices
omarchy-watch-unlock add-device phone --address AA:BB:CC:DD:EE:FF
```

Identity criteria combine with AND. At least one of `--address`, `--irk`,
`--service-uuid`, or `--name-prefix` is required. Per-device policy can be
tuned when adding a device:

```sh
omarchy-watch-unlock add-device fob --address AA:BB:CC:DD:EE:FF \
  --threshold-dbm -60 --minimum-samples 3 --freshness-ms 2000
```

Choose how many enrolled devices must be present:

```sh
omarchy-watch-unlock quorum any
omarchy-watch-unlock quorum all
omarchy-watch-unlock quorum at-least:2
```

Choose how an authorized request releases the lock screen:

```sh
omarchy-watch-unlock backend hyprlock-confirm
omarchy-watch-unlock backend process-signal --process swaylock --signal SIGUSR1
omarchy-watch-unlock backend command -- loginctl unlock-session
omarchy-watch-unlock backend disabled
```

Run `omarchy-watch-unlock --help` or a subcommand with `--help` for the complete
CLI reference.

## Configuration

Configuration is stored at
`~/.config/omarchy-watch-unlock/config.toml` with mode `0600`. Older schema 1
and 2 files remain readable and are migrated to schema 3 by the next CLI edit.

```toml
schema_version = 3
quorum = "any"
unlock_backend = "hyprlock-confirm"
# adapter = "hci1"

[[device]]
id = "phone"
profile = "presence"
address = "AA:BB:CC:DD:EE:FF"
minimum_samples = 3
```

List the profiles and enrollment providers compiled into the installed build:

```sh
omarchy-watch-unlock profiles
```

## Advanced usage

Enroll an Apple Watch without the wizard by pausing the daemon first:

```sh
systemctl --user stop omarchy-watch-unlockd
omarchy-watch-unlock enroll-device --provider apple-watch --save --id watch
systemctl --user start omarchy-watch-unlockd
```

Use `--adapter hci1` to select a non-default adapter or `--timeout-secs` to
change the default five-minute wait. Without `--save`, enrollment verifies the
identity without changing the configuration. If you already have a macOS
Remote IRK, `omarchy-watch-unlock enroll` reads it without echoing it.

The `pair` command is an experimental central-initiated pairing diagnostic. It
is not the supported Apple Watch enrollment path, and `pair --save` creates an
`apple-continuity` enrollment rather than a generic proximity device.

```sh
omarchy-watch-unlock pair --scan-secs 20
omarchy-watch-unlock bond-info
omarchy-watch-unlock bond-info --show-keys
```

`bond-info` redacts key material unless `--show-keys` is explicitly supplied.

## Troubleshooting

- Run `omarchy-watch-unlock doctor` first; it checks the configuration, daemon
  socket, and lock-screen integration.
- `DENY no-device` with `rssi=-` means nothing has matched since the daemon
  started. Wake the device and bring it closer.
- A locked Apple Watch, or one with Apple Auto Unlock disabled, revokes its
  current authorization.
- If the computer no longer appears under **Health Devices**, forget its stale
  entry on the Watch and retry enrollment.
- Check the daemon log with:

  ```sh
  journalctl --user -u omarchy-watch-unlockd --no-pager -n 100
  ```

## Architecture

```text
BlueZ advertisements -> identity + profile -> per-device policy -> quorum
                                                                  |
Lock screen / CLI <------------ user control socket <-------------+
```

The workspace contains four crates: `protocol` owns identities, profiles,
configuration, and policy; `daemon` scans through BlueZ and serves unlock
requests; `cli` provides setup and administration; and `pam` connects the
Quattro lock screen to the daemon.

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Inspiration

- [KatelynHaworth/watch-unlock-rs](https://github.com/KatelynHaworth/watch-unlock-rs)
- [DavidSt49/watch-unlock-linux](https://github.com/DavidSt49/watch-unlock-linux)

## License

Licensed under the MIT License.
