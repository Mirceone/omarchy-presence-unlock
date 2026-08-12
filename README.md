# Omarchy Presence Unlock

Omarchy Presence Unlock makes unlocking an Omarchy desktop less repetitive when
a trusted Bluetooth device is nearby. It keeps the user's intent explicit: device
presence authorizes an unlock request, while the lock screen still requires a
confirmation gesture.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Mirceone/omarchy-presence-unlock/main/install.sh | bash
```

## Why

Passwords and fingerprints remain the right fallback, but repeatedly using them
on a personal machine can add friction. Presence authorization offers a narrower
convenience layer without replacing the existing authentication methods or
silently unlocking the session.

Apple Watch is the best-supported device because it can report both proximity
and whether the Watch itself is unlocked. Phones, bands, fobs, and other BLE
devices can provide proximity authorization, but presence alone cannot prove
that the person carrying them is authorized.

## Security tradeoff

This project favors convenience, not stronger authentication. It does not defend
against Bluetooth relay or replay attacks, compromised Bluetooth devices, or a
process that already controls the user's session. Anyone adopting it should keep
password or fingerprint authentication enabled and decide whether Bluetooth
presence is appropriate for their threat model.

## Documentation policy

The code, command help, and observable behavior are the source of truth. This
README intentionally explains the project's purpose and tradeoffs rather than
duplicating operational details that can become stale.

## Inspiration

- [KatelynHaworth/watch-unlock-rs](https://github.com/KatelynHaworth/watch-unlock-rs)
- [DavidSt49/watch-unlock-linux](https://github.com/DavidSt49/watch-unlock-linux)

## License

Licensed under the MIT License.
