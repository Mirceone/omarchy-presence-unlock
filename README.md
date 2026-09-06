# Omarchy Presence Unlock

Unlock an Omarchy desktop by holding Alt when a trusted Bluetooth device is
nearby. Presence only authorizes the request; releasing the lock screen still
takes a deliberate gesture, so nothing unlocks on its own.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Mirceone/omarchy-presence-unlock/main/install.sh | bash
```

Then run `omarchy-presence-unlock` to enroll a device. An Apple Watch or a phone
pairs by picking this computer on the device itself; anything else is enrolled
by proximity. `Run diagnostics` reports what is wired up.

## How it works

A user service answers one question — is an enrolled device present and
asserting a state consistent with unlocking. A PAM module asks it, and a
companion Omarchy shell plugin runs that check when Alt is held on the lock
screen. Omarchy's lock screen is never modified or replaced: the plugin sits
beside it and asks it to finish the unlock, so Omarchy updates carry straight
through.

## Why

Passwords and fingerprints remain the right fallback, but reaching for them
dozens of times a day is friction. An Apple Watch is the best-supported device
because it reports both proximity and whether the Watch itself is unlocked. A
phone, band, or fob reports proximity alone: where the device is, never who is
carrying it.

## Security tradeoff

This favors convenience, not stronger authentication. It does not defend against
Bluetooth relay or replay attacks, a compromised device, or a process that
already controls the session. Keep password or fingerprint authentication
enabled, and decide whether Bluetooth presence suits your threat model.
Requiring several devices at once narrows the window without changing that.

## Notes

The code, command help, and observable behavior are the source of truth; this
README explains purpose and tradeoffs rather than details that go stale. Deeper
notes live in [`docs/`](docs). Prior art:
[watch-unlock-rs](https://github.com/KatelynHaworth/watch-unlock-rs),
[watch-unlock-linux](https://github.com/DavidSt49/watch-unlock-linux).

Licensed under the MIT License.
