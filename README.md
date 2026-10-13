# Omarchy Presence Unlock

Unlock an Omarchy desktop by deliberately holding Alt while a trusted Apple
device is nearby. It removes routine lock-screen friction without making
proximity alone an unlock signal.

Requires Omarchy 4 (Quattro).

## Install

Run the installer as your normal desktop user, without `sudo`:

```sh
curl -fsSL https://raw.githubusercontent.com/Mirceone/omarchy-presence-unlock/main/install.sh | bash
```

If an installation already exists, the installer shows its package version
(or identifies an older unpackaged install) and asks **Overwrite? [y/N]**.
Answering no leaves it untouched. Answering yes builds and validates the new
package first, then uninstalls the old application and installs the replacement,
preserving enrolled devices. The prompt reads from your terminal, including
when using `curl | bash`; unattended replacement is refused.

Files owned by another package, separate copies shadowing the command on PATH,
and installations without a working uninstaller are not overwritten. Resolve
the reported conflict or repair the old installation before trying again.

Prebuilt packages for tagged versions are on the
[Releases](https://github.com/Mirceone/omarchy-presence-unlock/releases) page.
Install one with `sudo pacman -U omarchy-presence-unlock-<version>-x86_64.pkg.tar.zst`,
then run `omarchy-presence-unlock setup` inside your Omarchy session.

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
```

The menu's **Uninstall** action does the same: it restores Omarchy's built-in
lock, removes your integration, and uses `sudo` and pacman to remove the installed
application package. Package removal affects every user on the computer; other
users' personal settings are not deleted. If integration cleanup fails, the
package stays installed so you can fix the reported problem and retry.

Your enrolled devices remain available unless you choose `--forget-devices`, so
you can return without enrolling them again.

## Why

Passwords and fingerprints remain the reliable fallback, but using them dozens
of times a day adds friction. A trusted device can make a lock screen feel more
immediate while Alt still makes each unlock an intentional act.

Apple Watch and iPhone are currently the only devices that work reliably. They
broadcast Bluetooth advertisements continuously and indicate both proximity and
whether they are unlocked. Other Bluetooth devices, including Android phones,
are not currently supported.

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
