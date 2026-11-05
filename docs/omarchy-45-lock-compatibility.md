# Omarchy 4.5 lock setup compatibility

Setup clones Omarchy's stock lock and applies exact text anchors for the
presence integration. The previous blanking-timer patch required this sequence:

```qml
    id: idleBlankTimer
    interval: 5000
```

Omarchy 4.5 inserts `readonly property int idleMs: 5000` and
`readonly property int resumeMs: 15000`, and uses `interval: idleMs`.
The literal anchor therefore no longer exists, so rendering Service.qml fails
before setup can install the presence lock.

Remove the blanking-timer patch instead of matching another timer layout. Setup
now preserves Omarchy's own timer values, interval binding, and resume handling.
This also preserves the 5000 ms interval on older releases rather than replacing
it with 30000 ms. Anchors for the actual presence authentication integration
remain strict: a missing or ambiguous required anchor still stops rendering.

Regression tests render both the legacy timer layout and a fixture reflecting
the Omarchy 4.5 idle/resume layout. They assert that each entire timer block is
preserved, including the newer resume-gap handling, while the presence code is
still added. Rendering tests do not establish runtime sleep/wake or reboot
reliability; neither is claimed as tested here.

The installed integration was reported working on native Omarchy 4.5 on an
Apple M1 Pro, with doctor checks passing and an actual Watch unlock succeeding
when used together with the separate BLE scanner fix. This setup change has
no code dependency on that scanner fix. Standalone validation for this branch
is documented in the pull request.
