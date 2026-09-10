# Omarchy Presence Unlock

Unlock an Omarchy desktop by deliberately holding Alt while a trusted Bluetooth
device is nearby. It removes routine lock-screen friction without making
proximity alone an unlock signal.

Requires Omarchy 4 (Quattro).

## Install

Run the installer as your normal desktop user, without `sudo`:

```sh
curl -fsSL https://raw.githubusercontent.com/Mirceone/omarchy-presence-unlock/main/install.sh | bash
```

If it was installed over SSH or outside your graphical Omarchy session, run the
following after logging in:

```sh
omarchy-presence-unlock setup
```

Use **Setup › Security › Presence Unlock** to enroll and manage devices.
`omarchy-presence-unlock doctor` explains anything that needs attention.

## After an Omarchy update

Nothing is required. Presence Unlock continues to use your existing setup.

Rerun `omarchy-presence-unlock setup` only when you want it to adopt changes
from a newer Omarchy lock screen. It is safe to do so at any time.

## Removing

```sh
omarchy-presence-unlock uninstall
sudo pacman -Rns omarchy-presence-unlock
```

Your enrolled devices remain available unless you choose `--forget-devices`, so
you can return without enrolling them again.

## Why

Passwords and fingerprints remain the reliable fallback, but using them dozens
of times a day adds friction. A trusted device can make a lock screen feel more
immediate while Alt still makes each unlock an intentional act.

An Apple Watch is the best-supported device because it can indicate both
proximity and whether it is unlocked. A phone, band, or fob only indicates
proximity; it cannot establish who is carrying it.

## Security tradeoff

This is a convenience feature, not stronger authentication. It does not defend
against Bluetooth relay or replay attacks, a compromised trusted device, or a
process that already controls the desktop session. Keep password or fingerprint
authentication enabled and decide whether Bluetooth presence fits your threat
model. Requiring several devices can reduce accidental authorization, but does
not remove those risks.

## Notes

This README explains purpose and tradeoffs. The code, command help, and
observable behavior are the source of truth for operational details. Prior art:
[watch-unlock-rs](https://github.com/KatelynHaworth/watch-unlock-rs) and
[watch-unlock-linux](https://github.com/DavidSt49/watch-unlock-linux).

Licensed under the MIT License.
