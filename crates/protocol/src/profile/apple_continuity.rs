use super::{Observation, Profile};
use crate::{
    apple,
    ble::{Advertisement, Needs},
};

pub(super) static PROFILE: Profile = Profile::new(
    "apple-continuity",
    &["apple-watch"],
    Needs {
        manufacturer_data: true,
        ..Needs::nothing()
    },
    true,
    evaluate,
);

fn evaluate(advertisement: &Advertisement<'_>) -> Observation {
    let Some(payload) = advertisement.manufacturer(apple::COMPANY_ID) else {
        return Observation::Ignore;
    };
    match apple::parse_nearby_info(payload) {
        Ok(info) if info.watch_locked || !info.auto_unlock_enabled => Observation::Revoke,
        Ok(_) => Observation::Qualify,
        // An undecodable state claim is never evidence that a Watch is unlocked.
        Err(_) => Observation::Revoke,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn frame(flags: u8) -> HashMap<u16, Vec<u8>> {
        HashMap::from([(apple::COMPANY_ID, vec![0x10, 3, 0, flags, 0])])
    }

    #[test]
    fn unlocked_qualifies_and_locked_or_disabled_revokes() {
        let bare = Advertisement::new([1; 6], -50);
        let unlocked = frame(apple::AUTO_UNLOCK_ENABLED);
        let locked = frame(apple::AUTO_UNLOCK_ENABLED | apple::WATCH_LOCKED);
        let disabled = frame(0);
        assert_eq!(
            PROFILE.evaluate(&bare.with_manufacturer_data(&unlocked)),
            Observation::Qualify
        );
        assert_eq!(
            PROFILE.evaluate(&bare.with_manufacturer_data(&locked)),
            Observation::Revoke
        );
        assert_eq!(
            PROFILE.evaluate(&bare.with_manufacturer_data(&disabled)),
            Observation::Revoke
        );
    }

    #[test]
    fn absent_data_is_ignored_and_malformed_data_revokes() {
        let bare = Advertisement::new([1; 6], -50);
        let malformed = HashMap::from([(apple::COMPANY_ID, vec![0x10, 5, 0])]);
        assert_eq!(PROFILE.evaluate(&bare), Observation::Ignore);
        assert_eq!(
            PROFILE.evaluate(&bare.with_manufacturer_data(&malformed)),
            Observation::Revoke
        );
        assert!(PROFILE.attests_device_state());
    }
}
