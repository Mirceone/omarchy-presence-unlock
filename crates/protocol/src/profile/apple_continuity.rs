use super::{Observation, Profile};
use crate::{
    apple,
    ble::{Advertisement, Needs},
};

pub(super) static PROFILE: Profile = Profile::new(
    "apple-continuity",
    "Apple Continuity",
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
        // `WATCH_LOCKED` is the state assertion. `AUTO_UNLOCK_ENABLED` is the
        // accompanying signal that Apple considers the assertion usable for
        // auto-unlock. `WATCH_AUTO_UNLOCK_ENABLED` is a separate capability /
        // configuration bit: Mirceone's unlocked, wrist-worn Watch advertises
        // 0x98 (unlocked + auto-unlock enabled, without that bit), and treating
        // its absence as "locked" contradicted the state the packet actually
        // carried.
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
    fn unlocked_watch_with_auto_unlock_enabled_qualifies() {
        let bare = Advertisement::new([1; 6], -50);
        // Exact data flags captured from an unlocked enrolled Watch. The
        // watch-specific capability bit is absent, but the lock bit is clear
        // and the general auto-unlock bit is set.
        let observed_unlocked = frame(0x98);
        let all_capabilities = frame(apple::WATCH_AUTO_UNLOCK_ENABLED | apple::AUTO_UNLOCK_ENABLED);
        let locked = frame(
            apple::WATCH_AUTO_UNLOCK_ENABLED | apple::AUTO_UNLOCK_ENABLED | apple::WATCH_LOCKED,
        );
        let auto_unlock_disabled = frame(apple::WATCH_AUTO_UNLOCK_ENABLED);

        for flags in [observed_unlocked, all_capabilities] {
            assert_eq!(
                PROFILE.evaluate(&bare.with_manufacturer_data(&flags)),
                Observation::Qualify
            );
        }
        for flags in [locked, auto_unlock_disabled] {
            assert_eq!(
                PROFILE.evaluate(&bare.with_manufacturer_data(&flags)),
                Observation::Revoke
            );
        }
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
