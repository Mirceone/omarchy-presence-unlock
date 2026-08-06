use super::{Guide, Provider, Request};
use omarchy_presence_unlock_protocol::profile::APPLE_CONTINUITY;

/// What to tap on the Watch. `{name}` is the name this computer advertises
/// under, which is the only part the user has to recognise.
static STEPS: [&str; 4] = [
    "Open Settings",
    "Select Bluetooth",
    "Select Health Devices",
    "Tap \u{201c}{name}\u{201d}",
];

pub(super) static PROVIDER: Provider = Provider::new(
    "apple-watch",
    APPLE_CONTINUITY,
    Guide {
        label: "Apple Watch",
        summary: "Your computer will appear as a heart-rate sensor.",
        steps: &STEPS,
        hint: "Open Settings \u{2192} Bluetooth \u{2192} Health Devices on the Watch.",
    },
    "Advertise a Heart Rate peripheral and capture the Watch IRK during SMP",
    enroll,
);

fn enroll(request: &Request<'_>) -> Result<(), String> {
    crate::pairing::capture_apple_watch(
        request.adapter,
        request.timeout_secs,
        request.id,
        request.save,
        request.cancel,
        request.progress,
    )
}
