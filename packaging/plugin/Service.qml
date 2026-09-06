import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.Pam

Item {
  id: root

  // omarchy-presence-unlock:companion
  property var shell: null
  property bool authenticating: false
  property bool pamConfigured: false
  property bool holdArmed: false
  // This stays here so the gesture does not inherit Hyprland's input:repeat_delay.
  readonly property int holdMs: 400
  readonly property string userName: Quickshell.env("USER") || Quickshell.env("LOGNAME")

  function lockService() {
    return shell && typeof shell.serviceFor === "function"
      ? shell.serviceFor("omarchy.lock")
      : null
  }

  function logEvent(event) {
    console.log("presence-unlock " + new Date().toISOString() + " " + event)
  }

  // Evaluated per request, never cached: the shell assigns `shell` after this
  // object is constructed, and omarchy.lock may be built after us, so anything
  // latched at startup would pin us to "incompatible" for the whole session.
  function lockCompatible() {
    var lock = lockService()
    return !!lock
      && typeof lock.locked !== "undefined"
      && typeof lock.finishUnlock === "function"
  }

  function refusalToken() {
    if (!lockCompatible()) return "incompatible-lock"

    var lock = lockService()
    if (!lock.locked) return "not-locked"
    if (!pamConfigured) return "missing-pam"
    if (authenticating || presencePam.active || holdTimer.running) return "busy"
    return ""
  }

  function logRefusal(token) {
    logEvent("hold-refused: " + token)
    return token
  }

  function startPresencePam() {
    var lock = lockService()
    if (!lockCompatible() || !lock || !lock.locked || !pamConfigured) return false

    authenticating = true
    if (!presencePam.start()) {
      authenticating = false
      logEvent("pam-error: start-failed")
      return false
    }

    return true
  }

  function cancelPresence() {
    holdArmed = false
    holdTimer.stop()
    if (presencePam.active) presencePam.abort()
    authenticating = false
  }

  Timer {
    id: holdTimer
    interval: root.holdMs
    repeat: false

    onTriggered: {
      if (!root.holdArmed) return
      root.holdArmed = false

      var lock = root.lockService()
      if (!root.lockCompatible() || !lock || !lock.locked || !root.pamConfigured) return

      root.logEvent("hold-complete: pam-starting")
      root.startPresencePam()
    }
  }

  PamContext {
    id: presencePam
    config: "omarchy-lock-presence"
    user: root.userName

    onCompleted: function(result) {
      root.authenticating = false
      if (result !== PamResult.Success) {
        root.logEvent("pam-failure")
        return
      }

      root.logEvent("pam-success")
      var lock = root.lockService()
      if (root.lockCompatible() && lock && lock.locked) lock.finishUnlock()
    }

    onError: function(error) {
      root.authenticating = false
      root.logEvent("pam-error: " + error)
    }
  }

  FileView {
    path: "/etc/pam.d/omarchy-lock-presence"
    watchChanges: true
    printErrors: false
    onLoaded: root.pamConfigured = true
    onLoadFailed: root.pamConfigured = false
    onFileChanged: reload()
  }

  Connections {
    target: root.lockService()

    function onLockedChanged() {
      if (!target.locked) root.cancelPresence()
    }
  }

  IpcHandler {
    target: "presence-unlock"

    function hold(): string {
      var token = root.refusalToken()
      if (token) return root.logRefusal(token)

      root.holdArmed = true
      holdTimer.restart()
      return "holding"
    }

    function release(): string {
      root.holdArmed = false
      holdTimer.stop()
      return "ok"
    }

    // No method authenticates without the hold: the deliberate gesture is the
    // whole of the user's intent, and an IPC target any session process can
    // reach must not offer a way around it.
    function ping(): string { return "ok" }
  }

  // Reports a broken stock-lock contract once, after the shell has finished
  // wiring every service, so an Omarchy update that moves finishUnlock() is
  // visible in the log instead of only at the next lock screen.
  Timer {
    running: true
    interval: 3000
    repeat: false
    onTriggered: {
      if (!root.lockCompatible()) {
        root.logEvent("warning: incompatible omarchy.lock: missing locked or finishUnlock()")
      }
    }
  }

  Component.onDestruction: {
    if (presencePam.active) presencePam.abort()
  }
}
