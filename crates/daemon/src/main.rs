use omarchy_presence_unlock_protocol::paths;
use presenced::{ConfigFile, Fleet, Service, scan, serve};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = ConfigFile::load()?.resolve()?;
    let adapter = settings.adapter.clone();

    let fleet = Fleet::new(settings.devices, settings.multi_device_auth);
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
        "multi-device authentication {:?}, backend Omarchy Quattro (automatic), skipping per-advertisement reads: {}",
        settings.multi_device_auth,
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

    let service = Service::new(fleet);
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
