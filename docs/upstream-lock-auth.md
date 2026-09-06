# Proposal: supported secondary authentication for `omarchy.lock`
## Summary
**Proposal:** add a `lock-auth` plugin kind. The stock `omarchy.lock` service discovers enabled providers through `PluginRegistry`, renders an optional hint, and accepts requests only when its own lock state permits it.
The lock remains the security boundary: it owns lock state, starts and observes PAM, resets competing authenticators, and is the only caller of `finishUnlock()`. Providers supply availability, presentation metadata, and request intent; they never directly unlock a session.
## Problem
The stock lock is `shell/plugins/lock/Service.qml`. Its root `Item` owns the `WlSessionLock` named `sessionLock`, all lock lifecycle state, and the existing password and fingerprint authenticators.
| Current symbol | Current behaviour |
| --- | --- |
| `readonly property bool locked` | Combines `lockRequested`, `sessionLock.locked`, and `sessionLock.secure` (line 37). |
| `finishUnlock()` | Clears request/pending state, calls `resetAuthenticationState()`, sets `sessionLock.locked = false`, logs, and wakes the display (lines 148–160). |
| `resetAuthenticationState()` | Clears authentication state and aborts active `passwordPam` and `fingerprintPam` contexts (lines 116–126). |
| `IpcHandler { target: "lock" }` | Provides stock `lock()`, `isLocked()`, `status()`, `preview()`, and `hidePreview()` IPC (lines 510–550). |
Today, this repository's companion provider, `packaging/plugin/Service.qml`, implements `lockService()` with `shell.serviceFor("omarchy.lock")`. It reads the returned object's `locked` property in `refusalToken()` and `startPresencePam()`. When its local `PamContext { config: "omarchy-lock-presence" }` succeeds, its `onCompleted` calls `lock.finishUnlock()`.
Those are private implementation details, not a published provider API:
- `serviceFor()` is generic Shell service lookup, not lock authentication API.
- `locked` exposes the stock service's internal state-machine composition.
- `finishUnlock()` is the stock lock's internal transition; an external caller bypasses its ownership of request admission and PAM-result handling.
The concrete harm is established: an earlier approach cloned the complete lock plugin, text-patched it at brittle anchors, and regenerated the clone after every Omarchy update. The current companion plugin is smaller, but it still breaks if a normal lock refactor changes these private details.
The provider also has to prove, on its own, that the private shape it depends on still exists: `lockCompatible()` re-checks on every request that `locked` and a callable `finishUnlock` are present, and a startup timer logs a warning when they are not. That is defensive scaffolding around private coupling, not a primitive every authenticator should need to reimplement.
## Shipped precedent: fingerprint
Omarchy already has a second authenticator inside the stock lock:
- `fingerprintPam` is a `PamContext` using `omarchy-lock-fingerprint` (`Service.qml`, lines 341–354).
- `startFingerprint()` requires `lockRequested`, `sessionLock.secure`, and `fingerprintConfigured` before it starts PAM (lines 209–217).
- `handleFingerprintFinished(result)` calls `finishUnlock()` only on `PamResult.Success` (lines 219–228).
- `fingerprintCheckProc` requires `/etc/pam.d/omarchy-lock-fingerprint`, `fprintd-list`, and an enrolled finger before it sets `fingerprintConfigured` (lines 378–386).
The password policy is watched with `FileView` at `/etc/pam.d/omarchy-lock-password` (lines 484–491). Fingerprint uses a `Process` rather than `FileView`, but both patterns gate PAM on runtime availability before starting it.
`LockView.qml` supplies the visual precedent. It accepts `fingerprintConfigured`, derives `fingerprintReserve` from `fingerprintIcon` width (lines 27–29), reserves that space symmetrically for input margins (lines 136–141), and renders `fingerprintIndicator` only when configured (lines 200–216). An extra authenticator should generalize this stock indicator/layout pattern, not create a competing lock surface.
## Existing plugin infrastructure
`README.md` documents manifest `schemaVersion: 1`, `id`, `name`, `version`, `kinds`, and `entryPoints`; it lists `service` among current supported kinds. `PluginRegistry.validateManifest()` enforces those fields and rejects unsafe entry-point paths (`services/PluginRegistry.qml`, lines 43–90).
`PluginRegistry.rescan()` scans bundled manifests below `shell/plugins/` and third-party manifests below `~/.config/omarchy/plugins/` (lines 662–693). `PluginRegistry.isEnabled()` uses `shell.json`; third-party non-widget plugins are enabled by a `plugins[]` entry (lines 110–139 and 206–223).
For the existing `service` kind, `shell.qml` exposes `serviceFor(pluginId)` and `ensureService(pluginId)`. `ensureService()` requires `kinds` to include `service`, creates `entryPoints.service`, and injects `omarchyPath`, `shell`, `manifest`, and registries when those properties exist (lines 275–320). `_syncServices()` creates enabled services and destroys disabled/removed ones (lines 323–343).
`lock-auth` below is **proposed**, not current Omarchy behavior. It needs new validation and loading; it must not be silently treated as `service`.
## Option A: `secondaryPamService` configuration
### Proposed configuration
```json
{
  "lock": {
    "secondaryPamService": "omarchy-lock-presence",
    "secondaryAuthenticator": { "id": "presence", "indicator": "󰒑" }
  }
}
```
The location is a proposal: it could be `shell.json` or stock lock configuration. It is deliberately data, not a callable provider API.
### Responsibilities and migration
| Area | Proposed responsibility |
| --- | --- |
| Provider | Installs its PAM policy, writes configuration, and may install a compositor binding. It need not register an authentication object. |
| Stock lock | Watches the configured policy, shows the hint when available, owns a generic secondary `PamContext`, rejects inadmissible requests, and calls its own `finishUnlock()` on PAM success. |
| Manifest | No new kind. The provider may remain a current `service` plugin or have no Shell plugin. |
| Migration | Add the stock setting/PAM path; change the presence installer to write it; remove provider PAM and stock-service access; retain the binding only as input if desired. |
**Advantages:** smallest implementation, PAM wholly in the stock plugin, and easy migration for one presence service.
**Costs:** it supports one secondary service unless it grows an ad hoc list; availability beyond policy existence and indicator behavior are static configuration; every future authenticator risks another special field.
Option A is correct if Omarchy intends to support exactly one simple, administrator-configured additional PAM service.
## Option B: proposed `lock-auth` plugin kind
### Proposed manifest
```json
{
  "schemaVersion": 1,
  "id": "example.presence",
  "name": "Presence unlock",
  "version": "1.0.0",
  "kinds": ["lock-auth"],
  "entryPoints": { "lockAuth": "LockAuth.qml" },
  "lockAuth": { "displayName": "Trusted device", "indicator": "󰒑" }
}
```
This is an illustrative **proposal**, not a current schema. Metadata is declarative: it must not name an arbitrary QML authorization callback or grant an unlock callback.
### Responsibilities and migration
| Area | Proposed responsibility |
| --- | --- |
| Provider | Registers `entryPoints.lockAuth`, reports availability, exposes a visual hint, and emits a request. A compositor binding may still feed that request. |
| Stock lock | Discovers enabled `lock-auth` manifests via `PluginRegistry`, validates/loads providers, chooses indicator layout, admits requests, owns PAM contexts/results, and alone calls `finishUnlock()`. |
| Registry | Adds `lock-auth` to accepted kinds and a dedicated loader/lifecycle analogous to, but not overloaded onto, `ensureService()`. |
| Migration | Add registry/lock support; convert the companion to `lock-auth`; retain gesture/availability code; update installer manifest/binding; delete private compatibility code once a minimum Omarchy version requires this API. |
**Advantages:** supports several authenticator types without stock settings per provider; gives the lock structured availability and hint data; keeps providers independent of stock property/method names; makes future integrations first-class Shell plugins.
**Costs:** adds a public manifest kind, loader/lifecycle rules, and protocol that must be versioned; requires policy for multiple providers and bounded UI; is more code than one generic PAM setting.
## Recommendation
Recommend **Option B: `lock-auth`**, with a deliberately narrow provider contract. Fingerprint and `LockView` already demonstrate that the real need is availability gating plus an optional in-field hint, not merely another PAM string. A registry-backed extension makes that variation explicit and avoids accumulating one stock configuration field per future provider.
The recommendation depends on a hard boundary: providers state availability, presentation metadata, and request intent; the lock retains every security-sensitive state transition. If maintainers want to support the single presence provider forever, Option A is the simpler correct choice.
## Minimal API sketch for Option B
The following is **proposed API**, not existing Omarchy QML.
### Provider entry point
```qml
// LockAuth.qml -- proposed provider contract
QtObject {
    property string providerId: "example.presence"
    // Updated when policy or trusted-device prerequisites change.
    property bool available: false
    property string indicatorText: "󰒑"
    property string indicatorAccessibleName: "Trusted device available"
    // This asks the stock lock to begin an attempt. It cannot unlock directly.
    signal authenticationRequested()
    // Optional stock notification for admission refusal before PAM.
    function requestRejected(reason: string): void {}
}
```
A provider gesture handler emits `authenticationRequested()`. It must not hold a stock lock object, read `locked`, construct a lock-specific `PamContext`, or call an unlock function.
### Stock lock responsibilities
```qml
// Inside stock shell/plugins/lock/Service.qml -- proposed sketch
property var lockAuthProviders: []
property var activeSecondaryPam: null
function requestSecondaryAuthentication(provider) {
    if (!root.locked || !lockRequested || !sessionLock.secure) {
        provider.requestRejected("not-locked")
        return
    }
    if (!provider.available || authenticating || activeSecondaryPam) {
        provider.requestRejected("unavailable")
        return
    }
    var pam = secondaryPamFor(provider) // stock-owned, validated mapping
    if (!pam || !policyExistsFor(provider) || !pam.start()) {
        provider.requestRejected("unavailable")
        return
    }
    activeSecondaryPam = pam
}
function handleSecondaryPamCompleted(provider, pam, result) {
    if (pam !== activeSecondaryPam) return
    activeSecondaryPam = null
    if (!lockRequested) return
    if (result === PamResult.Success) finishUnlock()
}
```
The helper names are illustrative. The required boundaries are:
1. The lock checks its own state before starting a request.
2. The lock creates/starts PAM, or observes a result through a stock-owned PAM object; a provider cannot provide a Boolean "success".
3. Only the lock calls `finishUnlock()` after stock-owned success handling.
4. `resetAuthenticationState()` aborts active secondary PAM along with password and fingerprint authentication.
5. The lock validates a provider-to-PAM mapping and watches policy availability; installing a plugin alone is not proof that PAM exists.
For presentation, `LockView.qml` should receive stock-derived indicator data, not a provider-owned visual component. The lock can generalize `fingerprintReserve` to a selected indicator or bounded list, preserving centered password dots and stock ownership of accessibility and layout.
## What the presence provider could delete
After this contract is available and required, `packaging/plugin/Service.qml` can delete:
- `lockService()` and `shell.serviceFor("omarchy.lock")`;
- reads of the stock `locked` property in `refusalToken()` and `startPresencePam()`;
- the direct `lock.finishUnlock()` call in `presencePam.onCompleted`;
- `lockCompatible()` and its startup warning timer, which exist only to detect the private shape changing under us; and
- its lock-specific local `PamContext` and PAM-policy `FileView`, if the stock lock owns those for registered providers.
The doctor command can delete its warning that the stock lock is incompatible. That check exists only because the provider currently assumes the private `serviceFor("omarchy.lock")`, `locked`, and `finishUnlock()` shape.
The generated Hyprland block in `crates/cli/src/setup/quattro.rs` may remain an input mechanism during migration. It binds both `ALT_L` and `ALT_R`: press calls `omarchy-shell -q presence-unlock hold`; release calls `omarchy-shell -q presence-unlock release`. Its comments correctly keep the 400 ms hold duration in QML rather than Hyprland `long_press`; this proposal does not require changing that choice.
## Open maintainer questions
1. **How many providers may be active?** One is simplest. Multiple providers need deterministic ordering and rules to avoid competing PAM attempts.
2. **What is the indicator placement and layout budget?** The current `fingerprintReserve` safely handles one right-edge hint. Several providers need a fixed maximum, selected-provider policy, or compact aggregate hint.
3. **Where should the gesture live?** Presence currently uses compositor press/release bindings. Decide whether that remains provider-owned, routes to stock lock IPC, or becomes lock-owned input.
4. **What if a provider is installed but its PAM policy is absent?** Recommended: mark it unavailable, omit/disable its hint, reject requests without PAM, and preserve password unlock. Watch policy changes to avoid stale availability.
5. **Which manifest fields are stable?** Decide whether the PAM service name is manifest metadata, a stock allow-list mapping, or user configuration; do not infer it from arbitrary provider QML.
6. **How are provider failures surfaced?** Stock logging should diagnose missing policy or PAM-start failure without exposing secrets or making provider failure a lock failure.
## Requested decision
Please consider the proposed `lock-auth` extension point with the stock lock as the sole unlock authority. It gives third-party authenticators a supported path while preserving the stock lock's ownership of `WlSessionLock`, PAM outcomes, UI layout, and `finishUnlock()`.
