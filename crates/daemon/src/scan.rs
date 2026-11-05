//! Stable `BlueZ` discovery, with live signal evidence rather than cached devices.
//!
//! `DuplicateData` affects host notifications, not the controller duplicate
//! filter. Renew only our discovery session to obtain new reports on controllers
//! that otherwise deliver identical advertisements just once per scan period.

use crate::{clock::boottime_ms, control::Service};
use dbus::{
    arg::{PropMap, RefArg},
    message::MatchRule,
    nonblock::{Proxy, stdintf::org_freedesktop_dbus::Properties},
};
use omarchy_presence_unlock_protocol::{Advertisement, Needs, ble::parse_address};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use uuid::Uuid;

const RENEW_AFTER: Duration = Duration::from_secs(1);
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("BlueZ operation failed: {0}")]
    Bluez(#[from] bluer::Error),
    #[error("Bluetooth D-Bus operation failed: {0}")]
    Dbus(#[from] dbus::Error),
    #[error("Bluetooth adapter is not powered")]
    AdapterNotPowered,
    #[error("Bluetooth signal connection ended")]
    Disconnected,
    #[error("Bluetooth signal queue overflowed; evidence invalidated")]
    Overflow,
}

// Dropping a scan future (including on cancellation) closes its private bus
// connection, releasing its discovery session without affecting other clients.
struct BusTask(tokio::task::JoinHandle<dbus_tokio::connection::IOResourceError>);
impl Drop for BusTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Streams live evidence into the fleet using non-experimental `BlueZ` APIs.
///
/// # Errors
/// Returns transport errors, signal loss, or an unpowered adapter. The caller
/// must invalidate accumulated evidence on any return.
pub async fn scan(adapter_name: Option<&str>, service: Arc<Service>) -> Result<(), ScanError> {
    let session = bluer::Session::new().await?;
    let adapter = match adapter_name {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    if !adapter.is_powered().await? {
        return Err(ScanError::AdapterNotPowered);
    }
    let adapter_path = format!("/org/bluez/{}", adapter.name());
    let (resource, connection) = dbus_tokio::connection::new_system_sync()?;
    let mut bus_task = BusTask(tokio::spawn(resource));
    let proxy = Proxy::new(
        "org.bluez",
        adapter_path.clone(),
        CALL_TIMEOUT,
        connection.clone(),
    );

    let filter: PropMap = HashMap::from([
        (
            "Transport".into(),
            dbus::arg::Variant(Box::new("le".to_string()) as Box<dyn RefArg>),
        ),
        (
            "DuplicateData".into(),
            dbus::arg::Variant(Box::new(true) as Box<dyn RefArg>),
        ),
        (
            "RSSI".into(),
            dbus::arg::Variant(Box::new(-127_i16) as Box<dyn RefArg>),
        ),
    ]);
    let () = proxy
        .method_call("org.bluez.Adapter1", "SetDiscoveryFilter", (filter,))
        .await?;

    // Subscribe before discovery. Never enumerate Device1 objects or read their
    // cached RSSI / payload. Keep signal fields together: bluer's changes stream
    // splits a signal into properties and erases the evidence provenance.
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let overflow_tx = Arc::clone(&overflow);
    let mut rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    rule.sender = Some("org.bluez".into());
    let _signals = connection.add_match(rule).await?.msg_cb(move |message| {
        if let Some(report) = Report::from_message(&message, &adapter_path) {
            match tx.try_send((boottime_ms(), report)) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => (),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    overflow_tx.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        true
    });
    let () = proxy
        .method_call("org.bluez.Adapter1", "StartDiscovery", ())
        .await?;
    eprintln!(
        "presence scan: live signals only; discovery renewal=1000ms; controller duplicate filtering is independent of DuplicateData; shared discovery may prevent renewal"
    );
    let mut renew =
        tokio::time::interval_at(tokio::time::Instant::now() + RENEW_AFTER, RENEW_AFTER);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut diagnostics = tokio::time::interval_at(
        tokio::time::Instant::now() + DIAGNOSTIC_INTERVAL,
        DIAGNOSTIC_INTERVAL,
    );
    let mut counts = Counts::default();
    loop {
        if overflow.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ScanError::Overflow);
        }
        tokio::select! {
            _ = &mut bus_task.0 => return Err(ScanError::Disconnected),
            _ = renew.tick() => {
                // Await StopDiscovery completion: dropping a bluer discovery
                // stream releases the session asynchronously and can race Start.
                let () = proxy.method_call("org.bluez.Adapter1", "StopDiscovery", ()).await?;
                let shared: bool = proxy.get("org.bluez.Adapter1", "Discovering").await?;
                if shared { counts.shared += 1; }
                let () = proxy.method_call("org.bluez.Adapter1", "StartDiscovery", ()).await?;
                counts.renewals += 1;
            }
            _ = diagnostics.tick() => {
                let fleet = service.fleet.lock().await;
                counts.log(&fleet, boottime_ms());
                counts.reset();
            }
            item = rx.recv() => {
                let Some((received_ms, report)) = item else { return Err(ScanError::Disconnected); };
                let mut fleet = service.fleet.lock().await;
                if overflow.load(std::sync::atomic::Ordering::Relaxed) { return Err(ScanError::Overflow); }
                if !fleet.is_interested(&report.address) { continue; }
                counts.interested += 1;
                // Evaluate each configured identity using only fields from this
                // signal. Missing required fields deny; no cross-packet cache.
                if let Some(advertisement) = report.advertisement() {
                    counts.record(received_ms);
                    fleet.observe(received_ms, &advertisement);
                } else {
                    counts.incomplete += 1;
                    // A lock assertion must revoke even when unchanged RSSI was
                    // omitted. This path never adds samples or updates RSSI.
                    fleet.observe_revocations(received_ms, &report.view(-127));
                }
            }
        }
    }
}

#[derive(Default)]
struct Counts {
    renewals: u64,
    shared: u64,
    interested: u64,
    incomplete: u64,
    evidence: u64,
    last_ms: Option<u64>,
    max_gap_ms: u64,
}
impl Counts {
    fn record(&mut self, now: u64) {
        if let Some(last) = self.last_ms {
            self.max_gap_ms = self.max_gap_ms.max(now.saturating_sub(last));
        }
        self.last_ms = Some(now);
        self.evidence += 1;
    }
    fn log(&self, fleet: &omarchy_presence_unlock_protocol::Fleet, now: u64) {
        eprintln!(
            "presence scan (30s): renewals={} shared-or-stopping={} interested-signals={} missing-rssi-signals={} rssi-signals={} max-evidence-gap-ms={}; sparse evidence can indicate controller filtering/shared discovery or unchanged RSSI; doctor checks setup, not packet cadence",
            self.renewals,
            self.shared,
            self.interested,
            self.incomplete,
            self.evidence,
            self.max_gap_ms
        );
        for device in fleet.devices() {
            eprintln!(
                "presence evidence: profile={} samples={}/{} last-qualifying-age-ms={:?} decision={:?}",
                device.spec.profile.id(),
                device.qualifying_samples(now),
                device.spec.policy.minimum_samples,
                device.last_qualifying_age_ms(now),
                device.check(now)
            );
        }
    }
    fn reset(&mut self) {
        *self = Self {
            last_ms: self.last_ms,
            ..Self::default()
        };
    }
}

#[derive(Default)]
struct Report {
    address: [u8; 6],
    rssi: Option<i16>,
    manufacturer_data: Option<HashMap<u16, Vec<u8>>>,
    service_data: Option<HashMap<Uuid, Vec<u8>>>,
    service_uuids: Option<HashSet<Uuid>>,
    name: Option<String>,
}
impl Report {
    fn from_message(message: &dbus::Message, adapter: &str) -> Option<Self> {
        let path = message.path()?.to_string();
        let suffix = path.strip_prefix(adapter)?.strip_prefix("/dev_")?;
        let address = parse_address(&suffix.replace('_', ":")).ok()?;
        let (interface, fields, invalidated): (String, PropMap, Vec<String>) =
            message.read3().ok()?;
        if interface != "org.bluez.Device1" || !invalidated.is_empty() {
            return None;
        }
        Some(Self::from_fields(address, &fields))
    }

    fn from_fields(address: [u8; 6], fields: &PropMap) -> Self {
        Self {
            address,
            rssi: dbus::arg::prop_cast::<i16>(fields, "RSSI")
                .copied()
                .filter(|r| (-127..=20).contains(r)),
            manufacturer_data: fields
                .get("ManufacturerData")
                .and_then(|v| dictionary(&*v.0, |k| u16::try_from(k.as_u64()?).ok())),
            service_data: fields
                .get("ServiceData")
                .and_then(|v| dictionary(&*v.0, |k| Uuid::parse_str(k.as_str()?).ok())),
            service_uuids: fields.get("UUIDs").and_then(|v| {
                v.0.as_iter()?
                    .map(|s| Uuid::parse_str(s.as_str()?).ok())
                    .collect()
            }),
            name: dbus::arg::prop_cast::<String>(fields, "Name").cloned(),
        }
    }

    fn advertisement(&self) -> Option<Advertisement<'_>> {
        Some(self.view(self.rssi?))
    }

    fn view(&self, rssi: i16) -> Advertisement<'_> {
        let mut ad = Advertisement::new(self.address, rssi);
        ad.manufacturer_data = self.manufacturer_data.as_ref();
        ad.service_data = self.service_data.as_ref();
        ad.service_uuids = self.service_uuids.as_ref();
        ad.name = self.name.as_deref();
        ad
    }
}

fn dictionary<K: Eq + std::hash::Hash>(
    value: &dyn RefArg,
    key: impl Fn(&dyn RefArg) -> Option<K>,
) -> Option<HashMap<K, Vec<u8>>> {
    let mut iter = value.as_iter()?;
    let mut result = HashMap::new();
    while let Some(k) = iter.next() {
        let value = iter.next()?;
        let bytes = value.as_iter()?.next()?;
        let bytes = bytes
            .as_iter()?
            .map(|b| u8::try_from(b.as_u64()?).ok())
            .collect::<Option<Vec<_>>>()?;
        result.insert(key(k)?, bytes);
    }
    Some(result)
}

/// Fields not needed by any configured identity or profile (diagnostic only).
#[must_use]
pub fn skipped_reads(needs: Needs) -> Vec<&'static str> {
    let mut skipped = Vec::new();
    if !needs.manufacturer_data {
        skipped.push("manufacturer-data");
    }
    if !needs.service_data {
        skipped.push("service-data");
    }
    if !needs.service_uuids {
        skipped.push("service-uuids");
    }
    if !needs.name {
        skipped.push("name");
    }
    skipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::arg::Variant;
    use omarchy_presence_unlock_protocol::{
        Decision, DeviceSpec, Fleet, Identity, MultiDeviceAuth, Policy, profile, wire,
    };

    fn fields(rssi: Option<i16>, flags: Option<u8>) -> PropMap {
        let mut fields: PropMap = HashMap::new();
        if let Some(rssi) = rssi {
            fields.insert("RSSI".into(), Variant(Box::new(rssi)));
        }
        if let Some(flags) = flags {
            fields.insert(
                "ManufacturerData".into(),
                Variant(Box::new(HashMap::from([(
                    0x004c_u16,
                    Variant(vec![0x10_u8, 3, 0, flags, 0]),
                )]))),
            );
        }
        fields
    }

    // Exercise real D-Bus marshalling, including a{qv} and its byte arrays.
    fn message(fields: PropMap) -> dbus::Message {
        let msg = dbus::Message::new_signal(
            "/org/bluez/hci0/dev_01_02_03_04_05_06",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .unwrap();
        msg.append3(
            "org.bluez.Device1".to_string(),
            fields,
            Vec::<String>::new(),
        )
    }

    fn report(rssi: Option<i16>, flags: Option<u8>) -> Report {
        Report::from_message(&message(fields(rssi, flags)), "/org/bluez/hci0").unwrap()
    }

    fn fleet() -> Fleet {
        Fleet::new(
            vec![DeviceSpec {
                id: "watch".into(),
                identity: Identity::from_address([1, 2, 3, 4, 5, 6]),
                profile: profile::APPLE_CONTINUITY,
                policy: Policy::default(),
            }],
            MultiDeviceAuth::Any,
        )
    }
    fn feed(fleet: &mut Fleet, now: u64, report: &Report) {
        if let Some(ad) = report.advertisement() {
            fleet.observe(now, &ad);
        } else {
            fleet.observe_revocations(now, &report.view(-127));
        }
    }

    #[test]
    fn two_live_reports_with_identical_payload_can_authorize() {
        let report = report(Some(-45), Some(0x98));
        assert_eq!(
            report.manufacturer_data.as_ref().unwrap()[&0x4c],
            [0x10, 3, 0, 0x98, 0]
        );
        let mut fleet = fleet();
        feed(&mut fleet, 100, &report);
        assert_eq!(
            fleet.check(100),
            Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES)
        );
        feed(&mut fleet, 1100, &report);
        assert_eq!(fleet.check(1100), Decision::Allow);
        assert_eq!(fleet.check(3601), Decision::Deny(wire::DENY_STALE));
    }

    #[test]
    fn cached_properties_and_unrelated_changes_cannot_refresh_watch_evidence() {
        let mut fleet = fleet();
        let live = report(Some(-45), Some(0x98));
        feed(&mut fleet, 100, &live);
        // Former scanner would read cached ManufacturerData and RSSI for each.
        for now in [200, 1100, 2100, 3100] {
            feed(&mut fleet, now, &report(None, Some(0x98)));
            feed(&mut fleet, now, &report(Some(-45), None));
            feed(&mut fleet, now, &report(None, None));
        }
        assert_eq!(fleet.check(3100), Decision::Deny(wire::DENY_STALE));
    }

    #[test]
    fn ten_second_bursts_do_not_weaken_the_policy() {
        let mut fleet = fleet();
        let live = report(Some(-41), Some(0x98));
        for now in [100, 10100, 20100] {
            feed(&mut fleet, now, &live);
            feed(&mut fleet, now + 1, &live);
            assert_eq!(
                fleet.check(now + 1),
                Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES)
            );
            assert_eq!(fleet.check(now + 2501), Decision::Deny(wire::DENY_STALE));
        }
    }

    #[test]
    fn lock_weak_signal_and_identity_checks_remain_enforced() {
        let mut fleet = fleet();
        let live = report(Some(-45), Some(0x98));
        feed(&mut fleet, 100, &live);
        feed(&mut fleet, 1100, &live);
        assert_eq!(fleet.check(1100), Decision::Allow);
        feed(&mut fleet, 1200, &report(Some(-45), Some(0xb8)));
        assert_eq!(fleet.check(1200), Decision::Deny(wire::DENY_DEVICE_LOCKED));
        feed(&mut fleet, 1300, &live);
        feed(&mut fleet, 1400, &report(Some(-100), Some(0x98)));
        let mut stranger = report(Some(-45), Some(0x98));
        stranger.address = [9; 6];
        feed(&mut fleet, 1500, &stranger);
        assert_eq!(
            fleet.check(1500),
            Decision::Deny(wire::DENY_INSUFFICIENT_SAMPLES)
        );
        assert!(report(Some(127), Some(0x98)).advertisement().is_none());
    }

    #[test]
    fn live_lock_assertion_without_rssi_revokes_but_unlock_cannot_qualify() {
        let mut fleet = fleet();
        let live = report(Some(-45), Some(0x98));
        feed(&mut fleet, 100, &live);
        feed(&mut fleet, 1100, &live);
        feed(&mut fleet, 1200, &report(None, Some(0xb8)));
        assert_eq!(fleet.check(1200), Decision::Deny(wire::DENY_DEVICE_LOCKED));
        assert_eq!(fleet.devices()[0].last_rssi(), Some(-45));
        feed(&mut fleet, 1300, &report(None, Some(0x98)));
        assert_eq!(fleet.check(1300), Decision::Deny(wire::DENY_DEVICE_LOCKED));
        assert_eq!(fleet.devices()[0].qualifying_samples(1300), 0);
    }

    #[test]
    fn only_device_signals_on_selected_adapter_are_accepted() {
        let msg = message(fields(Some(-45), Some(0x98)));
        assert!(Report::from_message(&msg, "/org/bluez/hci1").is_none());
        let cached = dbus::Message::new_signal(
            "/org/bluez",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
        )
        .unwrap()
        .append2(
            "/org/bluez/hci0/dev_01_02_03_04_05_06".to_string(),
            fields(Some(-45), Some(0x98)),
        );
        assert!(Report::from_message(&cached, "/org/bluez/hci0").is_none());
    }
}
