use super::{Guide, Provider, Request};
use omarchy_presence_unlock_protocol::profile::PRESENCE;

/// Where a phone lists a nearby pairable accessory. `{name}` is the alias this
/// computer advertises under, which is the only part the user has to recognise.
///
/// A phone may nonetheless list this computer under some older name: the
/// advertisement carries the adapter alias, but a phone caches names per
/// Bluetooth address, and this adapter's address is static and public.
/// Forgetting the device on the phone and re-adding it does not clear that
/// cache — measured, not assumed — so nothing this flow advertises can
/// correct it, and instructing the user to forget first only added a step
/// that fixed nothing.
///
/// Worded for iOS because that is where the wording differs most from the
/// obvious; Android's Bluetooth screen reaches the same list in the same order.
static STEPS: [&str; 4] = [
    "Open Settings",
    "Select Bluetooth",
    "Look under \u{201c}Other Devices\u{201d}",
    "Tap \u{201c}{name}\u{201d}",
];

pub(super) static PROVIDER: Provider = Provider::new(
    "phone",
    PRESENCE,
    Guide {
        label: "Phone",
        summary: "Your computer will appear as a pairable Bluetooth accessory.",
        steps: &STEPS,
        hint: "Open Settings \u{2192} Bluetooth on the phone and tap this computer under Other Devices.",
    },
    "Advertise this computer as pairable and capture the phone IRK during SMP",
    enroll,
);

/// A phone bonds through the same transport as a Watch, but it is enrolled as
/// proximity only: carrying a phone says nothing about who is holding it, so
/// the profile must not let its advertisements assert a device lock state.
fn enroll(request: &Request<'_>) -> Result<(), String> {
    crate::pairing::capture_peripheral(
        &crate::pairing::Enrollment {
            profile: PRESENCE.id(),
            fallback: "phone",
            id: request.id,
            save: request.save,
        },
        request.adapter,
        request.timeout_secs,
        request.cancel,
        request.progress,
    )
}
