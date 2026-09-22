import QtQuick
import Quickshell
import Quickshell.Io

Item {
  id: root

  property var shell: null

  readonly property string configHome:
    Quickshell.env("XDG_CONFIG_HOME")
      || ((Quickshell.env("HOME") || "") + "/.config")

  readonly property string runtimeHome:
    Quickshell.env("XDG_RUNTIME_DIR") || ""

  readonly property string configPath:
    configHome + "/omarchy-presence-unlock/automation.json"

  readonly property string socketPath:
    runtimeHome + "/omarchy-presence-unlock/control.sock"

  property bool autoLockEnabled: false
  property bool autoUnlockEnabled: false
  property bool unlockOnlyAfterAutoLock: true
  property bool suspendWhenWatchLocked: true
  property bool suspendedForWatchLock: false

  property int lockAfterMs: 15000
  property int unlockAfterMs: 1000
  property int cooldownMs: 10000
  property int noDeviceLockAfterMs: 30000
  property int lockRssi: -85
  property int wakeRssi: -75
  property int unlockRssi: -55

  property bool armed: false
  property bool lockedByAutomation: false
  property bool arrivalWakeSent: false
  property double automaticLockAt: 0
  property int automaticLockRssi: 0
  property bool automaticLockHadRssi: false
  property int approachDeltaDb: 3
  property bool checkInFlight: false
  property bool responseSeen: false

  property double absentSince: 0
  property string departureReason: ""
  property double allowedSince: 0
  property double cooldownUntil: 0
  property double lastPollAt: 0
  property double lastRssiAt: 0
  property string lastDecision: "unknown"
  property int lastRssi: 0
  property bool hasLastRssi: false

  function logEvent(event) {
    console.log(
      "presence-automation "
      + new Date().toISOString()
      + " "
      + event
    )
  }

  function lockService() {
    return shell && typeof shell.serviceFor === "function"
      ? shell.serviceFor("omarchy.lock")
      : null
  }

  function unlockService() {
    return shell && typeof shell.serviceFor === "function"
      ? shell.serviceFor("presence.unlock")
      : null
  }

  function automationEnabled() {
    return autoLockEnabled || autoUnlockEnabled
  }

  function resetEvidence(disarm) {
    absentSince = 0
    departureReason = ""
    allowedSince = 0

    if (disarm) armed = false
  }

  function loadConfiguration() {
    autoLockEnabled = false
    autoUnlockEnabled = false
    unlockOnlyAfterAutoLock = true
    suspendWhenWatchLocked = true
    lockAfterMs = 15000
    unlockAfterMs = 1000
    cooldownMs = 10000
    noDeviceLockAfterMs = 30000
    lockRssi = -85
    wakeRssi = -85
    unlockRssi = -55
    approachDeltaDb = 3

    if (!configuration.loaded) {
      resetEvidence(true)
      logEvent("config-unavailable")
      return
    }

    try {
      var config = JSON.parse(configuration.text())

      autoLockEnabled = config.auto_lock === true
      autoUnlockEnabled = config.auto_unlock === true
      unlockOnlyAfterAutoLock =
        config.unlock_only_after_auto_lock !== false
      suspendWhenWatchLocked =
        config.suspend_when_watch_locked !== false

      if (typeof config.lock_after_seconds === "number"
          && isFinite(config.lock_after_seconds)) {
        lockAfterMs =
          Math.max(5, config.lock_after_seconds) * 1000
      }

      if (typeof config.unlock_after_seconds === "number"
          && isFinite(config.unlock_after_seconds)) {
        unlockAfterMs =
          Math.max(1, config.unlock_after_seconds) * 1000
      }

      if (typeof config.cooldown_seconds === "number"
          && isFinite(config.cooldown_seconds)) {
        cooldownMs =
          Math.max(5, config.cooldown_seconds) * 1000
      }

      if (typeof config.no_device_lock_after_seconds === "number"
          && isFinite(config.no_device_lock_after_seconds)) {
        noDeviceLockAfterMs =
          Math.max(15, config.no_device_lock_after_seconds) * 1000
      }

      if (typeof config.lock_rssi === "number"
          && isFinite(config.lock_rssi)) {
        lockRssi = Math.round(config.lock_rssi)
      }

      if (typeof config.wake_rssi === "number"
          && isFinite(config.wake_rssi)) {
        wakeRssi = Math.round(config.wake_rssi)
      }

      if (typeof config.approach_delta_db === "number"
          && isFinite(config.approach_delta_db)) {
        approachDeltaDb =
          Math.max(1, Math.round(config.approach_delta_db))
      }

      if (typeof config.unlock_rssi === "number"
          && isFinite(config.unlock_rssi)) {
        unlockRssi = Math.round(config.unlock_rssi)
      }

      if (wakeRssi > unlockRssi) {
        throw new Error(
          "wake_rssi must not be stronger than unlock_rssi"
        )
      }

      if (unlockRssi <= lockRssi) {
        throw new Error(
          "unlock_rssi must be stronger than lock_rssi"
        )
      }

      resetEvidence(true)
      lockedByAutomation = false

      logEvent(
        "config-loaded: auto-lock="
        + autoLockEnabled
        + " auto-unlock="
        + autoUnlockEnabled
      )
    } catch (error) {
      resetEvidence(true)
      lockedByAutomation = false
      logEvent("config-error: " + error)
    }
  }

  function requestAutomaticLock(now) {
    var lock = lockService()

    if (!lock
        || typeof lock.locked === "undefined"
        || typeof lock.beginLock !== "function") {
      logEvent("lock-refused: incompatible-lock")
      return
    }

    if (lock.locked) return

    if (lock.beginLock()) {
      lockedByAutomation = true
      arrivalWakeSent = false
      automaticLockAt = now
      automaticLockRssi = lastRssi
      automaticLockHadRssi = hasLastRssi
      cooldownUntil = now + cooldownMs
      logEvent(
        "lock-requested"
        + (hasLastRssi ? " rssi=" + lastRssi : "")
      )
    } else {
      cooldownUntil = now + cooldownMs
      logEvent("lock-refused: beginLock-failed")
    }
  }

  function requestAutomaticUnlock(now) {
    var lock = lockService()
    var unlock = unlockService()

    if (!lock || !lock.locked) return

    if (!unlock
        || typeof unlock.startPresencePam !== "function") {
      cooldownUntil = now + cooldownMs
      logEvent("unlock-refused: incompatible-presence-service")
      return
    }

    cooldownUntil = now + cooldownMs

    logEvent("unlock-requested: pam-starting")

    if (!unlock.startPresencePam()) {
      logEvent("unlock-refused: pam-start-failed")
    }
  }

  function logDecisionChange(decision) {
    if (decision === lastDecision) return

    lastDecision = decision
    logEvent(
      "presence: " + decision
      + (hasLastRssi ? " rssi=" + lastRssi : "")
    )
  }

  function handleDecision(decision) {
    responseSeen = true
    logDecisionChange(decision)
    checkInFlight = false
    presenceSocket.connected = false

    var now = Date.now()
    var lock = lockService()

    if (!lock || typeof lock.locked === "undefined") {
      resetEvidence(true)
      return
    }

    // Wake immediately when an automatically locked session sees the Watch
    // return at a strong signal. Authentication still waits for full ALLOW.
    if (lock.locked
        && lockedByAutomation
        && hasLastRssi
        && lastRssi >= unlockRssi
        && typeof lock.runWake === "function") {
      lock.runWake()
    }

    if (decision === "ALLOW") {
      // A weak qualifying packet does not prove arrival. Preserve the
      // departure countdown until the Watch crosses the stronger return
      // threshold, creating hysteresis between leaving and arriving.
      if (hasLastRssi && lastRssi <= lockRssi) {
        allowedSince = 0

        // Identity authorization can remain ALLOW at a distance. A weak
        // qualifying signal therefore starts or continues departure timing.
        if (autoLockEnabled
            && armed
            && !lock.locked
            && now >= cooldownUntil) {
          if (absentSince === 0) {
            absentSince = now
            logEvent(
              "distance-started: rssi="
              + lastRssi
              + " lock-in="
              + Math.round(lockAfterMs / 1000)
              + "s"
            )
          } else if (now - absentSince >= lockAfterMs) {
            absentSince = 0
            requestAutomaticLock(now)
          }
        }

        return
      }

      if (hasLastRssi && lastRssi < unlockRssi) {
        // Inside the hysteresis band. Neither prove departure nor arrival.
        allowedSince = 0
        return
      }

      if (absentSince !== 0) {
        logEvent("absence-cancelled: arrived rssi=" + lastRssi)
      }

      absentSince = 0

      if (suspendedForWatchLock) {
        suspendedForWatchLock = false
        logEvent("resumed: watch-unlocked")
      }

      if (!armed) {
        armed = true
        logEvent("armed")
      }

      if (!lock.locked) {
        allowedSince = 0

        if (lockedByAutomation) {
          lockedByAutomation = false
        }

        return
      }

      // Arrival should wake the display before PAM is attempted, so the
      // user sees the lock screen while stable presence is established.
      if (lockedByAutomation
          && typeof lock.runWake === "function") {
        lock.runWake()
      }

      if (!autoUnlockEnabled) {
        allowedSince = 0
        return
      }

      if (unlockOnlyAfterAutoLock && !lockedByAutomation) {
        allowedSince = 0
        return
      }

      if (now < cooldownUntil) return

      if (allowedSince === 0) {
        allowedSince = now
        return
      }

      if (now - allowedSince >= unlockAfterMs) {
        allowedSince = 0
        requestAutomaticUnlock(now)
      }

      return
    }

    allowedSince = 0

    // A locked Watch is commonly off-wrist or charging. Do not infer that
    // the user left the computer from that state alone. Require a fresh,
    // unlocked ALLOW before automatic locking can arm again.
    if (suspendWhenWatchLocked
        && decision === "DENY device-locked") {
      if (absentSince !== 0) {
        logEvent("departure-cancelled: watch-locked")
      }

      absentSince = 0
      armed = false

      if (!suspendedForWatchLock) {
        suspendedForWatchLock = true
        logEvent("suspended: watch-locked")
      }

      return
    }

    if (!autoLockEnabled || !armed || lock.locked) {
      absentSince = 0
      return
    }

    // Departure requires both a non-authorizing decision and a weak
    // last signal. This permits stale evidence after walking away without
    // treating ordinary nearby advertisement gaps as departure.
    var weakSignal =
      !hasLastRssi || lastRssi <= lockRssi

    var rssiFresh =
      hasLastRssi
      && lastRssiAt !== 0
      && now - lastRssiAt <= 12000

    var measuredAway =
      rssiFresh
      && lastRssi <= lockRssi

    var noDevice =
      decision === "DENY no-device"
      || !rssiFresh

    var lockworthy =
      measuredAway
      || noDevice

    if (!lockworthy) {
      if (absentSince !== 0) {
        logEvent(
          "departure-cancelled: "
          + decision
          + (hasLastRssi ? " rssi=" + lastRssi : "")
        )
      }

      absentSince = 0
      departureReason = ""
      return
    }

    var requiredDelay =
      measuredAway ? lockAfterMs : noDeviceLockAfterMs

    var currentDepartureReason =
      measuredAway ? "weak-rssi" : "no-device"

    // Each evidence type owns its countdown. A partial no-device
    // countdown cannot become an already-expired weak-RSSI countdown.
    if (absentSince === 0
        || departureReason !== currentDepartureReason) {
      absentSince = now
      departureReason = currentDepartureReason
      logEvent(
        "absence-started: lock-in="
        + Math.round(requiredDelay / 1000)
        + "s reason="
        + departureReason
      )
    }

    if (now - absentSince >= requiredDelay) {
      absentSince = 0
      departureReason = ""
      requestAutomaticLock(now)
    }
  }

  function pollPresence() {
    if (!automationEnabled()) return
    if (checkInFlight || presenceSocket.connected) return
    if (!runtimeHome) return

    var now = Date.now()

    // A long timer gap usually indicates suspend, resume, or shell
    // starvation. Require fresh ALLOW evidence before acting again.
    if (lastPollAt !== 0 && now - lastPollAt > 4000) {
      var lock = lockService()

      resetEvidence(true)

      // Creating the lock can briefly delay this timer. Preserve ownership
      // while that automation-created lock is still active, so approach wake
      // and automatic unlock remain available when polling resumes.
      if (!lock || !lock.locked) {
        lockedByAutomation = false
        automaticLockAt = 0
        automaticLockHadRssi = false
        arrivalWakeSent = false
      }

      logEvent(
        "reset: timer-gap"
        + (lockedByAutomation ? " preserving-auto-lock" : "")
      )
    }

    lastPollAt = now
    responseSeen = false
    checkInFlight = true
    presenceSocket.connected = true
  }

  FileView {
    id: configuration

    path: root.configPath
    watchChanges: true
    printErrors: false
    blockLoading: true

    onLoaded: root.loadConfiguration()
    onLoadFailed: root.loadConfiguration()
    onFileChanged: reload()
  }

  Socket {
    id: presenceSocket

    path: root.socketPath
    connected: false

    parser: SplitParser {
      onRead: function(line) {
        var value = String(line).trim()

        if (value.indexOf("DEVICE ") === 0) {
          var match = value.match(/ rssi=(-?[0-9]+)$/)

          if (match) {
            root.lastRssi = Number(match[1])
            root.hasLastRssi = true
            root.lastRssiAt = Date.now()

            // Wake immediately on a strong return observation. Do not wait
            // for the aggregate decision to collect enough samples for PAM.
            var lock = root.lockService()

            var now = Date.now()

            // Waking is harmless and should feel proactive. After an
            // automatic lock, wake on the first subsequent Watch observation.
            // Authentication remains gated by full ALLOW, RSSI, and PAM.
            if (lock
                && lock.locked
                && root.lockedByAutomation
                && !root.arrivalWakeSent
                && now - root.automaticLockAt >= 3000
                && typeof lock.runWake === "function") {
              root.arrivalWakeSent = true
              root.logEvent(
                "wake-requested: first-post-lock-observation rssi="
                + root.lastRssi
              )
              lock.runWake()
            }
          } else {
            root.hasLastRssi = false
            root.lastRssiAt = 0
          }

          return
        }

        if (value === "ALLOW"
            || value.indexOf("DENY ") === 0) {
          root.handleDecision(value)
        }
      }
    }

    onConnectionStateChanged: {
      if (connected) {
        root.responseSeen = false
        write("STATUS 1\n")
        flush()
        return
      }

      root.checkInFlight = false

      if (!root.responseSeen) {
        root.resetEvidence(true)
      }
    }

    onError: function(error) {
      root.checkInFlight = false
      root.resetEvidence(true)
      root.logEvent("socket-error: " + error)
    }
  }

  Timer {
    id: pollTimer

    interval: 1000
    repeat: true
    running: root.automationEnabled()

    onTriggered: root.pollPresence()
  }

  Connections {
    target: root.lockService()

    function onLockedChanged() {
      if (!target.locked) {
        root.allowedSince = 0
        root.absentSince = 0
        root.lockedByAutomation = false
        root.arrivalWakeSent = false
        root.automaticLockAt = 0
        root.automaticLockHadRssi = false
        root.armed = false
        root.cooldownUntil = Date.now() + root.cooldownMs
        root.logEvent("disarmed: waiting-for-fresh-arrival")
      } else {
        root.allowedSince = 0
        root.absentSince = 0
      }
    }
  }

  IpcHandler {
    target: "presence-automation"

    function status(): string {
      return JSON.stringify({
        autoLock: root.autoLockEnabled,
        autoUnlock: root.autoUnlockEnabled,
        unlockOnlyAfterAutoLock:
          root.unlockOnlyAfterAutoLock,
        suspendWhenWatchLocked:
          root.suspendWhenWatchLocked,
        suspendedForWatchLock:
          root.suspendedForWatchLock,
        armed: root.armed,
        lockedByAutomation:
          root.lockedByAutomation,
        lockAfterMs: root.lockAfterMs,
        unlockAfterMs: root.unlockAfterMs,
        cooldownMs: root.cooldownMs,
        noDeviceLockAfterMs: root.noDeviceLockAfterMs,
        lockRssi: root.lockRssi,
        wakeRssi: root.wakeRssi,
        unlockRssi: root.unlockRssi,
        approachDeltaDb: root.approachDeltaDb,
        automaticLockRssi:
          root.automaticLockHadRssi
            ? root.automaticLockRssi
            : null,
        arrivalWakeSent: root.arrivalWakeSent,
        checkInFlight: root.checkInFlight,
        lastDecision: root.lastDecision,
        rssi: root.hasLastRssi ? root.lastRssi : null,
        absentForMs:
          root.absentSince === 0
            ? 0
            : Date.now() - root.absentSince
      })
    }

    function reload(): string {
      configuration.reload()
      return "ok"
    }

    function ping(): string {
      return "ok"
    }
  }

  Component.onDestruction: {
    pollTimer.stop()
    presenceSocket.connected = false
  }
}
