use super::{Provider, Request};
use omarchy_watch_unlock_protocol::profile::APPLE_CONTINUITY;

pub(super) static PROVIDER: Provider = Provider::new(
    "apple-watch",
    APPLE_CONTINUITY,
    "Advertise a Heart Rate peripheral and capture the Watch IRK during SMP",
    enroll,
);

fn enroll(request: &Request<'_>) -> Result<(), String> {
    crate::pairing::capture_apple_watch(
        request.adapter,
        request.timeout_secs,
        request.id,
        request.save,
    )
}
