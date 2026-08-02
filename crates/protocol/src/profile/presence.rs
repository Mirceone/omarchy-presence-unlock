use super::{Observation, Profile};
use crate::ble::{Advertisement, Needs};

pub(super) static PROFILE: Profile =
    Profile::new("presence", &["ble"], Needs::nothing(), false, evaluate);

fn evaluate(_advertisement: &Advertisement<'_>) -> Observation {
    Observation::Qualify
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_matched_advertisement_qualifies() {
        assert_eq!(
            PROFILE.evaluate(&Advertisement::new([1; 6], -90)),
            Observation::Qualify
        );
        assert!(!PROFILE.attests_device_state());
    }
}
