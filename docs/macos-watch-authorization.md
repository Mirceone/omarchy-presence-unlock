# macOS Apple Watch authorization observations

Observed on 2026-08-25 using a paired Apple Watch and `sudo -v`; timings below are from macOS unified logs. Raw capture files are intentionally not stored in this repository because they contain device identifiers and encrypted protocol payloads.

## Result

macOS administrator approval with an Apple Watch is an active, request-scoped BLE authentication protocol. It is not passive proximity authorization.

`sharingd` owns the Watch exchange. The authorization client connects through `com.apple.AutoUnlock.AuthenticationHintsProvider`; `coreauthd`/LocalAuthentication consumes the result. BLE uses the `SharingMacAutoUnlock` use case and an LE pipe, with a negotiated MTU of 527.

## Successful sudo flow

1. `sharingd` begins an Auto Unlock attempt, selects a cached Watch, and creates a lock session.
2. It establishes a BLE connection with the `SharingMacAutoUnlock` use case.
3. It sends `SDAutoUnlockAuthPromptRequest` to the Watch.
4. The Watch replies with `SDAutoUnlockAuthPromptResponse`.
5. The Mac sends `SDUnlockSessionKeyExchangeResponse`.
6. After the user double-clicks the Watch side button, the Watch returns `SDUnlockSessionAuthToken`.
7. macOS reports `AKS Unlock succeeded`, sends `SDUnlockSessionConfirmation`, receives its ACK, and returns a credential-bearing authentication context to the client.

In the captured successful run, the prompt response arrived about 0.6 seconds after the request. The auth token arrived about 3.1 seconds later, matching the user’s side-button interaction.

### Prompt messages

Static Objective-C metadata from `/usr/libexec/sharingd` identifies the protobuf schemas:

- `SDAutoUnlockAuthPromptRequest` (`PBRequest`): `version`, `iconHash`, `appName`, `navBarTitle`.
- `SDAutoUnlockAuthPromptResponse` (`PBCodable`): `version`, `keyData`, `needsImageData`, `errorCode`.

The request is the specific message that causes the Watch prompt. It is still carried in an encrypted `AutoUnlockTransportWrapper`; reproducing its plaintext fields alone is insufficient.


## Cryptographic boundary

The captured logs name cached device IDs, encrypted wrappers, session keys, and Apple KeyStore (AKS) validation. The Watch token is not a proximity signal and IRK resolution cannot generate or verify it. The remaining interoperability problem is reproducing Apple-enrolled device/session credentials and token verification outside Apple’s AKS/LocalAuthentication stack.

### Pairing material

`SDAutoUnlockAKSManager` persists per-device long-term keys (LTKs), attested LTKs, pairing IDs, escrow secrets, session keys, and ranging keys. It derives session keys from a shared secret and encrypts/decrypts transport wrappers with per-device keys, nonces, and authentication tags. The key manager is backed by the keychain and synchronizes remote LTK state with the Apple-device ecosystem.

This establishes the required trust boundary: a Linux reimplementation needs a legitimate replacement provisioning protocol and verifier. It cannot rely on the Watch IRK or replay a captured encrypted request.

### Toggle provisioning trace

Disabling Auto Unlock deletes the Watch device's AKS escrow record and session key. Re-enabling validates the local passcode, confirms the local LTK and attested LTK, signs the remote LTK, exchanges data with the Watch over BLE, persists a keychain item, and completes `com.apple.sharing.AutoUnlock.Setup` successfully. The trace also shows a private IDS channel, `com.apple.private.alloy.continuity.unlock`.



## Session lifecycle

The authorization-client connection is part of the protocol contract:

- Force-closing SecurityAgent invalidated its `AuthenticationHintsProvider` connection and the active Auto Unlock attempt.
- macOS then started a replacement BLE attempt and prompted the Watch again.
- A second double-click yielded a new `SDUnlockSessionAuthToken`; the logs show `AKS Unlock succeeded`, confirmation ACK, and `Client ack'd did complete`.

Therefore an approval belongs to a live active session. It must not be cached as general user authorization.

## Timeout and local cancellation

- The Watch first acknowledged the prompt with `SDAutoUnlockAuthPromptResponse`, then the Mac waited for the side-button token.
- After about 35 seconds without `SDUnlockSessionAuthToken`, `sharingd` logged `Response timer fired`, dismissed the session with `SFAutoUnlockErrorDomain` code `103` (`Request failure`), and invalidated the attempt.
- Cancelling the fallback macOS authentication UI deactivated the hints provider and cancelled the replacement Watch session. No auth token was received.


## Linux design constraints

A Linux implementation should keep persistent protocol state outside PAM:

```text
PAM request → root-owned broker → one pending Watch session
            → request-bound, single-use result → PAM success or fallback
```

Required invariants:

- Presence/IRK/Nearby Info only select an eligible Watch; they do not authorize `sudo`.
- One pending authorization owns one Watch session.
- Cancellation or timeout invalidates the associated Watch session.
- Approval binds to the request and expires quickly.
- Result delivery is acknowledged before session teardown.
- Password or fingerprint fallback remains available.
