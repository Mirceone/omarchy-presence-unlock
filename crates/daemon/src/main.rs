use omarchy_presence_unlock_protocol::paths;
use omarchy_presence_unlockd::{ConfigFile, Fleet, Service, scan, serve, unlock};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = ConfigFile::load()?.resolve()?;
    let unlocker = unlock::build(&settings.backend);
    let adapter = settings.adapter.clone();

    let fleet = Fleet::new(settings.devices, settings.quorum);
    for device in fleet.devices() {
        eprintln!(
            "device {} ({}, {})",
            device.spec.id,
            device.spec.profile.id(),
            if device.spec.profile.attests_device_state() {
                "attests its own lock state"
            } else {
                "proximity only"
            }
        );
    }
    eprintln!(
        "quorum {:?}, backend {}, skipping per-advertisement reads: {}",
        settings.quorum,
        unlocker
            .as_ref()
            .map_or_else(|| "disabled".to_string(), |u| u.describe()),
        scan::skipped_reads(fleet.needs()).join(", ")
    );

    // Derived from the uid, matching the PAM module. Trusting $XDG_RUNTIME_DIR here
    // would serve a socket PAM never probes.
    let socket_dir = paths::current_socket_dir();
    // systemd's RuntimeDirectory= already creates this; kept so standalone runs work.
    std::fs::create_dir_all(&socket_dir)?;
    std::fs::set_permissions(
        &socket_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;

    let service = Service::new(fleet, unlocker);
    let mut server = tokio::spawn(serve(socket_dir.join("control.sock"), Arc::clone(&service)));
    loop {
        tokio::select! {
            // A daemon without a control socket authorizes nothing. Fail loudly so
            // systemd's Restart=on-failure engages instead of reporting a healthy unit.
            outcome = &mut server => {
                return Err(match outcome {
                    Ok(Err(error)) => format!("control socket failed: {error}").into(),
                    Ok(Ok(())) => {
                        Box::<dyn std::error::Error>::from("control socket listener exited")
                    }
                    Err(error) => format!("control socket task panicked: {error}").into(),
                });
            }
            outcome = scan::scan(adapter.as_deref(), Arc::clone(&service)) => {
                if let Err(error) = outcome {
                    eprintln!("presence scan stopped: {error}");
                }
                // A scan that died saw nothing; everything it accumulated is unverifiable.
                service.fleet.lock().await.invalidate();
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
