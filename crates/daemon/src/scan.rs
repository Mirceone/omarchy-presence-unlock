//! The `BlueZ` transport: turns D-Bus discovery into [`Advertisement`]s.
//!
//! This is the only module that knows about `BlueZ`. Everything downstream sees
//! transport-neutral advertisements, which is what makes the policy layer
//! testable without a radio.

use crate::{clock::boottime_ms, control::Service};
use bluer::{AdapterEvent, DiscoveryFilter, DiscoveryTransport};
use futures_util::StreamExt;
use omarchy_presence_unlock_protocol::{Advertisement, Needs};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("BlueZ operation failed: {0}")]
    Bluez(#[from] bluer::Error),
    #[error("Bluetooth adapter is not powered")]
    AdapterNotPowered,
}

/// Streams advertisements into the fleet until the adapter goes away.
///
/// # Errors
///
/// Returns a `BlueZ` communication error or reports an unpowered adapter.
pub async fn scan(adapter_name: Option<&str>, service: Arc<Service>) -> Result<(), ScanError> {
    let session = bluer::Session::new().await?;
    let adapter = match adapter_name {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    if !adapter.is_powered().await? {
        return Err(ScanError::AdapterNotPowered);
    }
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await?;
    // Fixed for the lifetime of the scan: the fleet is immutable after startup,
    // so the per-advertisement D-Bus reads are decided once, here.
    let needs = service.fleet.lock().await.needs();
    // With duplicate_data, BlueZ re-signals ManufacturerData per advertisement, so
    // discover_devices_with_changes yields one DeviceAdded per advertisement.
    // Devices that match no configured identity cost no D-Bus traffic: the first
    // filter is arithmetic on the address already carried by the event.
    let mut events = std::pin::pin!(adapter.discover_devices_with_changes().await?);
    while let Some(event) = events.next().await {
        let AdapterEvent::DeviceAdded(address) = event else {
            continue;
        };
        if !service.fleet.lock().await.is_interested(&address.0) {
            continue;
        }
        let device = adapter.device(address)?;
        // RSSI is the one property every policy needs, and its absence means
        // BlueZ has no live advertisement for this device.
        let Some(rssi) = device.rssi().await? else {
            continue;
        };
        let manufacturer_data = fetch(needs.manufacturer_data, device.manufacturer_data()).await?;
        let service_data = fetch(needs.service_data, device.service_data()).await?;
        let service_uuids = fetch(needs.service_uuids, device.uuids()).await?;
        let name = fetch(needs.name, device.name()).await?;

        let mut advertisement = Advertisement::new(address.0, rssi);
        advertisement.manufacturer_data = manufacturer_data.as_ref();
        advertisement.service_data = service_data.as_ref();
        advertisement.service_uuids = service_uuids.as_ref();
        advertisement.name = name.as_deref();
        service
            .fleet
            .lock()
            .await
            .observe(boottime_ms(), &advertisement);
    }
    Ok(())
}

/// Awaits a property read only when some configured device depends on it.
async fn fetch<T>(
    wanted: bool,
    property: impl Future<Output = bluer::Result<Option<T>>>,
) -> Result<Option<T>, ScanError> {
    if wanted {
        Ok(property.await?)
    } else {
        Ok(None)
    }
}

/// Fields no configured device reads, for logging at startup.
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
