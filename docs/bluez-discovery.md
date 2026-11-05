# Sparse Apple Watch evidence on BlueZ

## Findings

The reported HCI trace shows advertising reports in bursts about 10 seconds
apart with controller duplicate filtering enabled. That cannot satisfy the
unchanged default policy: two qualifying samples in 3000 ms, latest no older
than 2500 ms, with at least 50 ms between counted samples. Strong RSSI and an
active discovery session do not establish sufficient fresh evidence.

`DuplicateData=true` requests repeated BlueZ ManufacturerData / ServiceData
notifications. It does not promise to disable controller duplicate filtering.
The inspected Aurora kernel source (`hci_active_scan_sync` in
`net/bluetooth/hci_sync.c`) defaults to controller duplicate filtering and only
disables it for specific conditions, including advertisement monitoring and a
strict-duplicate-filter quirk with result filtering. The application should not
assume a discovery filter overrides that controller setting.

The previous `discover_devices_with_changes` path also replayed cached devices,
converted every Device1 property change into DeviceAdded, and read cached RSSI
and payloads with a newly generated timestamp. A 50 ms spacing rule cannot
establish packet provenance. Besides causing misleading diagnostics, that path
could turn unrelated cached property updates into authorization evidence.

References:

- [BlueZ 5.87 Adapter API](https://github.com/bluez/bluez/blob/5.87/doc/org.bluez.Adapter.rst)
- [bluer discovery implementation](https://github.com/bluez/bluer/blob/master/bluer/src/adapter.rs)
- [Linux active discovery implementation](https://github.com/torvalds/linux/blob/master/net/bluetooth/hci_sync.c)

## Scanner changes

The scanner uses stable Adapter1 discovery methods on a private D-Bus connection.
It releases and reacquires **only its own** discovery session once per second,
awaiting each operation. When it is the sole discovery client, this gives the
controller new scan periods rather than waiting for the approximately 10-second
bursts. No adapter power cycling, Bluetooth service restart, experimental
monitor registration, direct HCI commands, or added privileges are involved.
Cancelling the scanner closes its connection and releases its session.

Evidence comes from Device1 PropertiesChanged signals on the selected adapter;
there is no device enumeration or property-read fallback. Fields stay within one
signal. A Watch sample requires live RSSI and ManufacturerData together, plus
any payload identity criteria configured for that device. One signal is at most
one observation. Equal payload bytes in separate live reports are permitted;
an unchanged unlocked Watch is expected to repeat its payload. The existing
spacing, signal threshold, identity matching, lock-state evaluation, sample
window, freshness, PAM protocol, and password fallback remain in force.

A live lock assertion without RSSI can revoke previous evidence, but never add
samples or refresh qualifying evidence. Missing payload criteria never match by
borrowing an older name, service UUID, or manufacturer payload.

Signal receipt time is retained through the bounded processing queue rather
than replaced at policy evaluation. Queue overflow or transport failure stops
the scanner and causes the existing daemon loop to invalidate evidence.

Every 30 seconds, diagnostics report session renewals, discovery still active
after release, interested signals, missing RSSI, report gaps, and per-profile
counted samples, last qualifying age and decision. No keys, addresses, names,
or advertising bytes are logged. `doctor` remains a setup check; passing it
does not establish that packet cadence can satisfy the policy.

## Validation

Regression tests use real D-Bus message marshalling and the existing fleet policy.
The six scanner tests cover repeated identical live payloads, unrelated or
partial signals, cached object notifications, wrong adapters, invalid RSSI,
10-second bursts, weak signal, identity mismatch, lock revocation and expiry.

Hardware testing reported by the contributor:

- MacBook Pro 14-inch (2021), Apple M1 Pro.
- Native Omarchy 4.5 on aarch64.
- Aurora kernel 7.2.11-14-1-ARCH; BlueZ 5.87-2.
- Before the patch: alternating `DENY stale` and `DENY insufficient-samples`.
  HCI capture showed controller duplicate filtering and advertising reports in
  approximately 10-second bursts.
- With the patched daemon: consistent `ALLOW`; the installed version passed
  all doctor checks and an actual Watch unlock succeeded.
- Removing and locking the Watch correctly prevented unlocking.
- The combined working tree passed 159 workspace tests, including six new
  scanner regressions; Clippy passed for the daemon and protocol crates.

These results establish working authorization on the tested setup. They do not
prove that the controller filter was explicitly disabled, explain the exact
controller/firmware cause, or establish continuous reliability on other systems.
No IRKs, Bluetooth identifiers, or raw captures accompany this report.

## Remaining limitations

Sleep/wake reliability and reboot behavior have not been verified. The clock
and fail-closed transport handling are not substitutes for those hardware tests.

Other clients may keep the shared discovery procedure running when this client
releases its session. The `shared-or-stopping` counter identifies that condition
but is not proof of another client: BlueZ can still be completing a stop. This
scanner never stops another client's session. Renewals also create short scan
gaps and add controller work; radio load and power use need measurement.

BlueZ can omit unchanged RSSI and can coalesce notifications. The first report
that creates a Device1 object is intentionally not counted from InterfacesAdded;
if temporary objects are repeatedly removed and only their first report is
delivered, this conservative path can still fail to collect enough evidence.
The scanner conservatively denies when a signal lacks required fields; profiles
that need name or UUID fields may therefore see reduced availability. Signals are live
host notifications, not HCI packet IDs or radio timestamps. This change never
counts a cached property read, but cannot recover reports suppressed below
D-Bus. A future packet transport would need separately reviewed access and
packet timestamp handling if this conservative stable API path is insufficient.

On another system, compare HCI scan periods and fresh Watch report cadence with
the diagnostic counters. Verify authorization, locked/off-wrist denial, stale
evidence denial, and password fallback independently. If qualifying reports
remain roughly 10 seconds apart, retain denial and investigate the
controller/driver path; do not widen evidence lifetimes or reduce sample counts.

The scanner branch is independent of the Omarchy 4.5 setup compatibility fix.
Hardware testing used the working installation with both changes; standalone
branch checks are documented in the pull request. Automated tests do not
establish radio behavior, installation behavior, sleep/wake, or reboot reliability.
