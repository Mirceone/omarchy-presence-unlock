//! Presence policy: per-device evidence and multi-device authentication.

use crate::{
    ble::{Advertisement, Needs},
    identity::{Identity, Verdict},
    profile::{Observation, Profile},
    wire,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub threshold_dbm: i16,
    pub minimum_samples: u8,
    pub sample_window_ms: u64,
    pub freshness_ms: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            threshold_dbm: -75,
            minimum_samples: 2,
            sample_window_ms: 3_000,
            freshness_ms: 2_500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(&'static str),
}

impl Decision {
    #[must_use]
    pub fn is_allow(self) -> bool {
        self == Self::Allow
    }

    /// How close this outcome is to authorising, lowest first.
    ///
    /// A fleet reports the reason of its closest device, so a user staring at
    /// `status` sees the obstacle that actually matters. Fresh but incomplete
    /// evidence ranks ahead of a locked device; a locked device ranks ahead of
    /// absence because unlocking it is actionable. Other denials are last.
    fn rank(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Deny(wire::DENY_INSUFFICIENT_SAMPLES) => 1,
            Self::Deny(wire::DENY_STALE) => 2,
            Self::Deny(wire::DENY_DEVICE_LOCKED) => 3,
            Self::Deny(_) => 4,
        }
    }
}

/// Minimum gap between two counted advertisements, in milliseconds.
///
/// Guards against one advertisement being counted twice; see [`Eligibility::observe`].
const MIN_SAMPLE_SPACING_MS: u64 = 50;

#[derive(Debug, Default)]
pub struct Eligibility {
    qualifying_samples: Vec<u64>,
    last_qualifying_ms: Option<u64>,
    last_revocation_ms: Option<u64>,
    last_rssi: Option<i16>,
}

impl Eligibility {
    pub fn invalidate(&mut self) {
        self.invalidate_evidence();
        self.last_revocation_ms = None;
        self.last_rssi = None;
    }

    fn invalidate_evidence(&mut self) {
        self.qualifying_samples.clear();
        self.last_qualifying_ms = None;
    }

    /// Most recent RSSI seen from this device, qualifying or not. Diagnostic only.
    #[must_use]
    pub fn last_rssi(&self) -> Option<i16> {
        self.last_rssi
    }

    /// Records one advertisement already matched to this device.
    ///
    /// [`Observation::Revoke`] is a discrete assertion by the device, so it
    /// revokes everything accumulated. A weak RSSI is not: it is a noisy analog
    /// measurement that routinely dips ten or more dB below its median on a
    /// wrist-worn device, so a single weak advertisement is no evidence of
    /// proximity but equally no evidence of absence. Treating it as a revocation
    /// lets ordinary RF fading deny a device that is right there. Absence expires
    /// through `sample_window_ms` and `freshness_ms` instead.
    pub fn observe(&mut self, now_ms: u64, rssi: i16, observation: Observation, policy: Policy) {
        self.last_rssi = Some(rssi);
        match observation {
            Observation::Revoke => {
                self.invalidate_evidence();
                self.last_revocation_ms = Some(now_ms);
                return;
            }
            Observation::Ignore => return,
            Observation::Qualify => self.last_revocation_ms = None,
        }
        self.qualifying_samples
            .retain(|sample| now_ms.saturating_sub(*sample) <= policy.sample_window_ms);
        if rssi < policy.threshold_dbm {
            return;
        }
        // BlueZ raises a property change for both RSSI and ManufacturerData on one
        // advertisement, so each advertisement is delivered twice within a
        // millisecond. Counting both would let a single advertisement satisfy
        // `minimum_samples`. Apple Continuity advertises roughly every 250 ms, so
        // this collapses the duplicate without discarding a genuine advertisement.
        if self
            .last_qualifying_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < MIN_SAMPLE_SPACING_MS)
        {
            return;
        }
        self.qualifying_samples.push(now_ms);
        self.last_qualifying_ms = Some(now_ms);
    }

    #[must_use]
    pub fn check(&self, now_ms: u64, policy: Policy) -> Decision {
        let Some(last) = self.last_qualifying_ms else {
            if self
                .last_revocation_ms
                .is_some_and(|last| now_ms.saturating_sub(last) <= policy.freshness_ms)
            {
                return Decision::Deny(wire::DENY_DEVICE_LOCKED);
            }
            return Decision::Deny(wire::DENY_NO_DEVICE);
        };
        if now_ms.saturating_sub(last) > policy.freshness_ms {
            return Decision::Deny(wire::DENY_STALE);
        }
        if self.qualifying_samples.len() < usize::from(policy.minimum_samples) {
            return Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES);
        }
        Decision::Allow
    }
}

/// A configured device: how to recognise it, how to read it, how strict to be.
#[derive(Debug)]
pub struct DeviceSpec {
    pub id: String,
    pub identity: Identity,
    pub profile: &'static Profile,
    pub policy: Policy,
}

#[derive(Debug)]
pub struct Device {
    pub spec: DeviceSpec,
    eligibility: Eligibility,
}

impl Device {
    #[must_use]
    pub fn new(spec: DeviceSpec) -> Self {
        Self {
            spec,
            eligibility: Eligibility::default(),
        }
    }

    #[must_use]
    pub fn check(&self, now_ms: u64) -> Decision {
        self.eligibility.check(now_ms, self.spec.policy)
    }

    #[must_use]
    pub fn last_rssi(&self) -> Option<i16> {
        self.eligibility.last_rssi()
    }
}

/// How many enrolled devices must be eligible before presence authentication succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MultiDeviceAuth {
    /// Any single device suffices. The default, and what a single-device setup wants.
    #[default]
    Any,
    /// Every configured device must be present.
    All,
    /// At least `n` devices must be present.
    AtLeast(u8),
}

impl MultiDeviceAuth {
    fn required(self, total: usize) -> usize {
        match self {
            Self::Any => 1,
            Self::All => total,
            Self::AtLeast(n) => usize::from(n).min(total).max(1),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DeviceStatus<'a> {
    pub id: &'a str,
    pub profile: &'static Profile,
    pub decision: Decision,
    pub rssi: Option<i16>,
}

/// Every configured device plus the multi-device authentication rule.
#[derive(Debug)]
pub struct Fleet {
    devices: Vec<Device>,
    multi_device_auth: MultiDeviceAuth,
    needs: Needs,
}

impl Fleet {
    #[must_use]
    pub fn new(specs: Vec<DeviceSpec>, multi_device_auth: MultiDeviceAuth) -> Self {
        let needs = specs.iter().fold(Needs::nothing(), |needs, spec| {
            needs
                .union(spec.identity.needs())
                .union(spec.profile.needs())
        });
        Self {
            devices: specs.into_iter().map(Device::new).collect(),
            multi_device_auth,
            needs,
        }
    }

    /// Which advertisement fields the scanner must fetch for this fleet.
    #[must_use]
    pub fn needs(&self) -> Needs {
        self.needs
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    #[must_use]
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// True when some device might be behind `address`, so the caller should pay
    /// for the payload reads and call [`Fleet::observe`].
    #[must_use]
    pub fn is_interested(&self, address: &[u8; 6]) -> bool {
        self.devices
            .iter()
            .any(|device| device.spec.identity.verdict(address) != Verdict::Reject)
    }

    /// Folds one advertisement into every device it matches.
    pub fn observe(&mut self, now_ms: u64, advertisement: &Advertisement<'_>) {
        for device in &mut self.devices {
            if !device.spec.identity.matches(advertisement) {
                continue;
            }
            let observation = device.spec.profile.evaluate(advertisement);
            device
                .eligibility
                .observe(now_ms, advertisement.rssi, observation, device.spec.policy);
        }
    }

    pub fn invalidate(&mut self) {
        for device in &mut self.devices {
            device.eligibility.invalidate();
        }
    }

    #[must_use]
    pub fn check(&self, now_ms: u64) -> Decision {
        if self.devices.is_empty() {
            return Decision::Deny(wire::DENY_NO_DEVICE);
        }
        let required = self.multi_device_auth.required(self.devices.len());
        let mut allowed = 0;
        let mut closest = Decision::Deny(wire::DENY_NO_DEVICE);
        for device in &self.devices {
            let decision = device.check(now_ms);
            if decision.is_allow() {
                allowed += 1;
            }
            if decision.rank() < closest.rank() {
                closest = decision;
            }
        }
        if allowed >= required {
            return Decision::Allow;
        }
        // Some devices qualified but not enough: report the multi-device rule,
        // not a per-device reason that looks satisfied.
        if allowed > 0 {
            return Decision::Deny(wire::DENY_MULTI_DEVICE_AUTH);
        }
        match closest {
            Decision::Allow => Decision::Deny(wire::DENY_MULTI_DEVICE_AUTH),
            deny @ Decision::Deny(_) => deny,
        }
    }

    /// Per-device breakdown for `status`. Ordered as configured.
    pub fn report(&self, now_ms: u64) -> impl Iterator<Item = DeviceStatus<'_>> {
        self.devices.iter().map(move |device| DeviceStatus {
            id: &device.spec.id,
            profile: device.spec.profile,
            decision: device.check(now_ms),
            rssi: device.last_rssi(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{apple, ble::Advertisement};
    use std::collections::HashMap;

    fn qualify(eligibility: &mut Eligibility, now: u64, rssi: i16, policy: Policy) {
        eligibility.observe(now, rssi, Observation::Qualify, policy);
    }

    #[test]
    fn policy_needs_two_fresh_samples() {
        let policy = Policy::default();
        let mut eligibility = Eligibility::default();
        qualify(&mut eligibility, 100, -70, policy);
        assert_eq!(
            eligibility.check(200, policy),
            Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES)
        );
        qualify(&mut eligibility, 400, -70, policy);
        assert_eq!(eligibility.check(500, policy), Decision::Allow);
        assert_eq!(
            eligibility.check(3_000, policy),
            Decision::Deny(wire::DENY_STALE)
        );
    }

    #[test]
    fn an_rssi_dip_does_not_revoke_samples_already_collected() {
        let policy = Policy::default();
        let mut eligibility = Eligibility::default();
        qualify(&mut eligibility, 100, -70, policy);
        qualify(&mut eligibility, 400, -70, policy);
        assert_eq!(eligibility.check(500, policy), Decision::Allow);

        // A single fade below the threshold is RF noise, not departure.
        qualify(&mut eligibility, 700, -90, policy);
        assert_eq!(eligibility.check(800, policy), Decision::Allow);

        // Sustained absence still expires: nothing qualifying since t=400.
        assert_eq!(
            eligibility.check(3_000, policy),
            Decision::Deny(wire::DENY_STALE)
        );
    }

    #[test]
    fn a_revocation_clears_everything_accumulated() {
        let policy = Policy::default();
        let mut eligibility = Eligibility::default();
        qualify(&mut eligibility, 100, -70, policy);
        qualify(&mut eligibility, 400, -70, policy);
        assert_eq!(eligibility.check(500, policy), Decision::Allow);
        eligibility.observe(500, -70, Observation::Revoke, policy);
        assert_eq!(
            eligibility.check(500, policy),
            Decision::Deny(wire::DENY_DEVICE_LOCKED)
        );
    }

    #[test]
    fn a_locked_watch_that_ages_out_is_treated_as_an_absent_device() {
        let mut fleet = Fleet::new(vec![watch_spec("watch")], MultiDeviceAuth::Any);
        feed_locked(&mut fleet, [1, 2, 3, 4, 5, 6], 500, -70);
        assert_eq!(fleet.check(3_001), Decision::Deny(wire::DENY_NO_DEVICE));
    }

    #[test]
    fn an_ignored_advertisement_neither_counts_nor_revokes() {
        let policy = Policy::default();
        let mut eligibility = Eligibility::default();
        qualify(&mut eligibility, 100, -70, policy);
        qualify(&mut eligibility, 400, -70, policy);
        eligibility.observe(450, -70, Observation::Ignore, policy);
        assert_eq!(eligibility.last_rssi(), Some(-70));
        assert_eq!(eligibility.check(500, policy), Decision::Allow);
    }

    #[test]
    fn duplicate_advertisements_within_the_spacing_count_once() {
        let policy = Policy::default();
        let mut eligibility = Eligibility::default();
        qualify(&mut eligibility, 100, -70, policy);
        qualify(&mut eligibility, 110, -70, policy);
        assert_eq!(
            eligibility.check(150, policy),
            Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES)
        );
    }

    fn watch_spec(id: &str) -> DeviceSpec {
        DeviceSpec {
            id: id.into(),
            identity: Identity::from_address([1, 2, 3, 4, 5, 6]),
            profile: crate::profile::APPLE_CONTINUITY,
            policy: Policy::default(),
        }
    }

    fn fob_spec(id: &str) -> DeviceSpec {
        DeviceSpec {
            id: id.into(),
            identity: Identity::from_address([9, 9, 9, 9, 9, 9]),
            profile: crate::profile::PRESENCE,
            policy: Policy::default(),
        }
    }

    fn feed(fleet: &mut Fleet, address: [u8; 6], times: &[u64], rssi: i16) {
        let data = HashMap::from([(
            apple::COMPANY_ID,
            vec![
                0x10,
                3,
                0,
                apple::WATCH_AUTO_UNLOCK_ENABLED | apple::AUTO_UNLOCK_ENABLED,
                0,
            ],
        )]);
        for now in times {
            let advertisement = Advertisement::new(address, rssi).with_manufacturer_data(&data);
            fleet.observe(*now, &advertisement);
        }
    }

    fn feed_locked(fleet: &mut Fleet, address: [u8; 6], now: u64, rssi: i16) {
        let data = HashMap::from([(
            apple::COMPANY_ID,
            vec![
                0x10,
                3,
                0,
                apple::AUTO_UNLOCK_ENABLED | apple::WATCH_LOCKED,
                0,
            ],
        )]);
        let advertisement = Advertisement::new(address, rssi).with_manufacturer_data(&data);
        fleet.observe(now, &advertisement);
    }

    #[test]
    fn an_empty_fleet_denies() {
        let fleet = Fleet::new(Vec::new(), MultiDeviceAuth::Any);
        assert_eq!(fleet.check(0), Decision::Deny(wire::DENY_NO_DEVICE));
        assert!(!fleet.is_interested(&[1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn any_device_rule_allows_on_one_device() {
        let mut fleet = Fleet::new(
            vec![watch_spec("watch"), fob_spec("fob")],
            MultiDeviceAuth::Any,
        );
        feed(&mut fleet, [1, 2, 3, 4, 5, 6], &[100, 400], -60);
        assert_eq!(fleet.check(500), Decision::Allow);
    }

    #[test]
    fn every_device_rule_requires_every_device() {
        let mut fleet = Fleet::new(
            vec![watch_spec("watch"), fob_spec("fob")],
            MultiDeviceAuth::All,
        );
        feed(&mut fleet, [1, 2, 3, 4, 5, 6], &[100, 400], -60);
        assert_eq!(
            fleet.check(500),
            Decision::Deny(wire::DENY_MULTI_DEVICE_AUTH)
        );
        feed(&mut fleet, [9, 9, 9, 9, 9, 9], &[100, 400], -60);
        assert_eq!(fleet.check(500), Decision::Allow);
    }

    #[test]
    fn a_locked_watch_reports_its_state_and_rssi_until_it_is_unlocked() {
        let mut fleet = Fleet::new(vec![watch_spec("watch")], MultiDeviceAuth::Any);
        let address = [1, 2, 3, 4, 5, 6];
        feed_locked(&mut fleet, address, 100, -54);
        assert_eq!(fleet.check(200), Decision::Deny(wire::DENY_DEVICE_LOCKED));
        let row = fleet.report(200).next().expect("one configured Watch");
        assert_eq!(row.decision, Decision::Deny(wire::DENY_DEVICE_LOCKED));
        assert_eq!(row.rssi, Some(-54));

        feed(&mut fleet, address, &[400, 700], -54);
        assert_eq!(fleet.check(800), Decision::Allow);
    }

    #[test]
    fn a_fleet_with_only_a_locked_watch_reports_device_locked() {
        let mut fleet = Fleet::new(vec![watch_spec("watch")], MultiDeviceAuth::Any);
        feed_locked(&mut fleet, [1, 2, 3, 4, 5, 6], 100, -54);
        assert_eq!(fleet.check(200), Decision::Deny(wire::DENY_DEVICE_LOCKED));
    }

    #[test]
    fn a_fleet_reports_the_closest_denial_when_nothing_qualifies() {
        let mut fleet = Fleet::new(
            vec![watch_spec("watch"), fob_spec("fob")],
            MultiDeviceAuth::Any,
        );
        // One sample only: closer to allowing than the untouched fob's no-device.
        feed(&mut fleet, [1, 2, 3, 4, 5, 6], &[100], -60);
        assert_eq!(
            fleet.check(200),
            Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES)
        );
    }

    #[test]
    fn the_report_carries_one_row_per_configured_device() {
        let mut fleet = Fleet::new(
            vec![watch_spec("watch"), fob_spec("fob")],
            MultiDeviceAuth::Any,
        );
        feed(&mut fleet, [1, 2, 3, 4, 5, 6], &[100, 400], -60);
        let rows: Vec<_> = fleet.report(500).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "watch");
        assert_eq!(rows[0].decision, Decision::Allow);
        assert_eq!(rows[0].rssi, Some(-60));
        assert_eq!(rows[1].id, "fob");
        assert_eq!(rows[1].decision, Decision::Deny(wire::DENY_NO_DEVICE));
        assert_eq!(rows[1].rssi, None);
    }

    #[test]
    fn a_fleet_is_interested_only_in_configured_addresses() {
        let fleet = Fleet::new(vec![watch_spec("watch")], MultiDeviceAuth::Any);
        assert!(fleet.is_interested(&[1, 2, 3, 4, 5, 6]));
        assert!(!fleet.is_interested(&[7, 7, 7, 7, 7, 7]));
    }
}
