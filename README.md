# Omarchy Watch Unlock

BLE device presence unlock for Omarchy. An Apple Watch is the best-supported
device, but any BLE device that advertises can gate an unlock.

It is a personal-convenience feature, not a replacement for password or
fingerprint authentication: a capable relay/replay attacker is out of scope.

The control socket authorizes any process running as the same user, so the
`Alt+Enter` confirmation is a convenience gesture, not an attestation: a process
already running in your session can send the same confirmation while the device
is nearby. Anything with code execution in your session is outside this
feature's threat model.

## Architecture

```text
BlueZ scan  ->  Advertisement  ->  Identity  ->  Profile  ->  Eligibility
                (transport-        (is this     (what does    (per device)
                 neutral)           mine?)       it claim?)        |
                                                                   v
       PAM / CLI  <->  control socket  <->  Fleet quorum  ->  Unlocker
```

Each stage is independent:

| To add | Change |
| --- | --- |
| A device family with new advertisement semantics | one module and descriptor in the compile-time profile registry |
| A credential acquisition flow | one CLI enrollment-provider module and registry entry |
| A generic phone, fob, or beacon | configuration only: use the `presence` profile and identity criteria |
| A way of recognising a device | one criterion on `Identity` |
| A lock screen | one `Unlocker` implementation |
| A radio transport | one producer of `Advertisement` |

Crates: `protocol` (profile registry, identity, policy; no D-Bus or PAM),
`daemon` (BlueZ scan, control socket, unlockers), `cli` (enrollment-provider
registry and administration), and `pam`.

## Requirements

- Omarchy on Arch Linux with BlueZ, PAM, and a systemd user session
- a working Bluetooth LE adapter managed by `bluetoothd`
- Rust 1.88 or newer and Cargo when installing from source
- `sudo` access for installation and the narrowly scoped Watch IRK monitor

The package targets x86-64. Apple Watch enrollment is performed entirely on
Linux; a Mac is not required.

## Install from source

```sh
git clone https://github.com/mirceone/omarchy-watch-unlock.git
cd omarchy-watch-unlock
./install.sh
```

The installer builds the locked release workspace, writes the CLI, daemon, PAM
module, and service assets under `/usr`, then enables the systemd user service.
It does not modify enrollment, Hyprland, or PAM configuration. It is safe to
rerun after source changes.

## Apple Watch quick start

Stop the daemon's continuous scan while the PC temporarily advertises as a
Heart Rate Sensor:

```sh
systemctl --user stop omarchy-watch-unlockd
omarchy-watch-unlock enroll-device \
  --provider apple-watch \
  --save \
  --id watch \
  --timeout-secs 300
systemctl --user start omarchy-watch-unlockd
```

The command starts a private `sudo` helper; do not run the whole CLI as root.
When prompted, open **Settings > Bluetooth > Health Devices** on the Watch,
select the PC's Bluetooth name, and accept pairing. A successful enrollment
prints `IRK obtained and verified` before saving the `apple-continuity` profile.

Install the integration appropriate for the current Omarchy build, restart the
daemon after the configuration edit, and verify the complete installation:

```sh
omarchy-watch-unlock setup-omarchy
systemctl --user restart omarchy-watch-unlockd
omarchy-watch-unlock doctor
omarchy-watch-unlock status
```

On Hyprlock, press `Alt+Enter` while the lock screen is visible. Quattro builds
use the local lock plugin and dedicated PAM policy instead. Password and
fingerprint authentication remain available.

The Watch profile is the only built-in profile that reports the device's own
lock state. Authorization is revoked when the Watch says it is locked or Apple
Auto Unlock is disabled.

## Other BLE devices

List what the adapter can see, then enroll by whichever criterion is stable:

```sh
omarchy-watch-unlock devices
omarchy-watch-unlock add-device phone --address AA:BB:CC:DD:EE:FF
omarchy-watch-unlock add-device fob   --service-uuid 0000fe9f-0000-1000-8000-00805f9b34fb
omarchy-watch-unlock add-device band  --name-prefix "Mi Band"
```

Criteria combine with AND, and a device with none is refused. A device that
rotates private addresses (most phones, by design) cannot be matched by address;
enroll it with `--irk <base64>` so its addresses resolve.

These devices are **proximity only**: they assert nothing about their own lock
state, so presence alone authorizes. `status` reports their profile as `presence`.

Per-device policy overrides:

```sh
omarchy-watch-unlock add-device fob --address AA:BB:CC:DD:EE:FF \
  --threshold-dbm -60 --minimum-samples 3 --freshness-ms 2000
```

### Requiring more than one device

```sh
omarchy-watch-unlock quorum all           # every enrolled device must be present
omarchy-watch-unlock quorum at-least:2
omarchy-watch-unlock quorum any           # the default
```

## Unlock backends

```sh
omarchy-watch-unlock backend hyprlock-confirm
omarchy-watch-unlock backend process-signal --process swaylock --signal SIGUSR1
omarchy-watch-unlock backend command -- loginctl unlock-session
omarchy-watch-unlock backend disabled
```

`process-signal` covers lock screens that release on `SIGUSR1`/`SIGUSR2` but are
not Hyprlock; `--signal` defaults to `SIGUSR1`. Switching backends clears the
previous backend's keys, so a stale `unlock_command` never survives a change.

## Status

```sh
$ omarchy-watch-unlock status
DEVICE watch apple-continuity ALLOW rssi=-66
DEVICE phone presence DENY insufficient-samples rssi=-80
DENY quorum
```

One line per enrolled device, then the fleet's aggregate decision. Presence
denial reasons are stable tokens: `no-device`, `stale`,
`insufficient-samples`, and `quorum`. The `confirm` command can additionally
report `not-locked`, `not-eligible`, `backend`, or `unlock-failed`.

## Configuration

`~/.config/omarchy-watch-unlock/config.toml`, mode 0600. Schema 1 and 2 files
are read compatibly; the first CLI edit migrates them to schema 3 in place,
preserving comments and converting `kind` to a canonical profile id.

```toml
schema_version = 3
quorum = "any"
unlock_backend = "hyprlock-confirm"
# adapter = "hci1"

[[device]]
id = "watch"
profile = "apple-continuity"
irk_base64 = "..."
threshold_dbm = -75

[[device]]
id = "phone"
profile = "presence"
address = "AA:BB:CC:DD:EE:FF"
threshold_dbm = -70
minimum_samples = 3
```

Only the advertisement fields some enrolled device actually reads are fetched
from BlueZ; the daemon logs which reads it skips at startup.

Profiles are audited, compile-time advertisement decoders. List this build's
profiles and enrollment providers with:

```sh
omarchy-watch-unlock profiles
```

## How Apple Watch enrollment works

The `apple-watch` enrollment provider implements the Watch-tested path. The PC
advertises its existing Bluetooth alias, normally the hostname, as a Heart Rate
Sensor. Select it on the Watch under
**Settings > Bluetooth > Health Devices**:

```sh
omarchy-watch-unlock enroll-device --provider apple-watch --timeout-secs 300
omarchy-watch-unlock enroll-device --provider apple-watch --save --id watch
```

The command:

1. starts a narrowly scoped `sudo` helper that listens for the kernel's
   `MGMT_EV_NEW_IRK` event;
2. registers the tested Heart Rate, Device Information, Battery, and protected
   GATT services;
3. uses the `NoInputNoOutput`/Just Works agent profile;
4. calls BlueZ `Device1.Pair()` immediately when the Watch connects;
5. verifies the captured key against the Watch's resolvable private address; and
6. disconnects, removes the temporary BlueZ bond, and restores the adapter's
   previous pairability settings.

The IRK is never printed. Without `--save`, the provider reports the verified
result and changes no enrollment. With `--save`, it writes an
`apple-continuity` enrollment to `config.toml`. The temporary bond is removed
from the PC. watchOS may retain a stale Health Device entry after a test run;
forget that entry on the Watch before repeating enrollment.

`--adapter hci1` selects a non-default adapter. Stop
`omarchy-watch-unlockd` while enrolling because the daemon otherwise holds a
continuous LE scan:

```sh
systemctl --user stop omarchy-watch-unlockd
omarchy-watch-unlock enroll-device --provider apple-watch --save --id watch
systemctl --user start omarchy-watch-unlockd
```

If you already have a macOS Remote IRK, the legacy `enroll` command remains
available and reads the base64 key without echoing it:

```sh
omarchy-watch-unlock enroll
```

### Pairing diagnostics

`pair` is a central-initiated proof of concept. It scans, lets you select a
device, bonds with it, and reports whether BlueZ received an IRK:

```sh
omarchy-watch-unlock devices
omarchy-watch-unlock pair --scan-secs 20
```

Apple Watch bonding to a Linux central is unproven. Use the tested
`apple-watch` enrollment provider above for Watch enrollment; do not use
`pair --save` for an ordinary BLE device because that option creates an
`apple-continuity` enrollment.

`bond-info` dumps BlueZ's on-disk records and reports whether each existing bond
contains an `IdentityResolvingKey`:

```sh
omarchy-watch-unlock bond-info
omarchy-watch-unlock bond-info --show-keys
```

Key material is redacted unless `--show-keys` is explicitly supplied.

## Troubleshooting

- `DENY no-device` with `rssi=-` means no advertisement has matched the
  enrolled identity since the daemon started. Wake the Watch and keep it nearby.
  A locked Watch or one with Apple Auto Unlock disabled also revokes its current
  authorization.
- `DENY not-locked` from `confirm` means the lock backend reports that the
  screen is already unlocked.
- If the PC is absent from **Health Devices**, forget any stale entry for the
  same PC on the Watch and retry enrollment.
- A final `org.bluez.Error.Busy` pairability warning can occur while BlueZ
  settles after disconnect. Check `bluetoothctl show`; no action is needed when
  `Pairable: no`.
- Inspect daemon failures with:

  ```sh
  journalctl --user -u omarchy-watch-unlockd --no-pager -n 100
  ```

## Inspiration

- [KatelynHaworth/watch-unlock-rs](https://github.com/KatelynHaworth/watch-unlock-rs)
  demonstrated a Rust PAM/CLI design using an IRK, RSSI, and Apple Continuity
  Nearby Info to authenticate an Apple Watch.
- [DavidSt49/watch-unlock-linux](https://github.com/DavidSt49/watch-unlock-linux)
  provided a clear reference for BLE scanning, RPA resolution, Continuity state,
  and proximity thresholds on Linux.

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```


## License

Licensed under the MIT License.
