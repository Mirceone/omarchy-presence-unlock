//! Integration test for the presence.unlock QML plugin assets.
//!
//! Verifies that the packaged plugin directory contains valid and complete assets,
//! ensuring the plugin code and validation logic stay in sync.

use std::fs;

/// Returns the path to the plugin directory relative to this test crate.
fn plugin_dir() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../../packaging/plugin")
}

#[test]
fn manifest_json_schema_is_valid_and_complete() {
    let manifest_path = plugin_dir().join("manifest.json");
    let manifest_json = fs::read_to_string(&manifest_path)
        .expect("failed to read manifest.json; plugin directory missing or unreadable?");

    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_json).expect("manifest.json does not parse as valid JSON");

    // schemaVersion must be the JSON number 1, not a string.
    assert_eq!(
        manifest.get("schemaVersion"),
        Some(&serde_json::json!(1)),
        "schemaVersion must be JSON number 1 (used by Omarchy plugin loader)"
    );

    // id must be exactly "presence.unlock".
    assert_eq!(
        manifest.get("id").and_then(|v| v.as_str()),
        Some("presence.unlock"),
        "id must be exactly 'presence.unlock' (omarchy-shell and doctor grep for this value)"
    );

    // kinds must contain "service" (the plugin type).
    let kinds = manifest
        .get("kinds")
        .and_then(|v| v.as_array())
        .expect("kinds field is missing or not an array");
    assert!(
        kinds.iter().any(|k| k.as_str() == Some("service")),
        "kinds array must contain 'service' (tells Omarchy to load as a service)"
    );

    // entryPoints.service must be "Service.qml".
    let service_entry = manifest
        .pointer("/entryPoints/service")
        .and_then(|v| v.as_str())
        .expect("entryPoints.service field is missing or not a string");
    assert_eq!(
        service_entry, "Service.qml",
        "entryPoints.service must point to Service.qml (Omarchy's loader looks for this)"
    );
}

#[test]
fn service_qml_file_exists() {
    let service_path = plugin_dir().join("Service.qml");
    assert!(
        service_path.exists(),
        "Service.qml must exist at {} (referenced by manifest entryPoints.service)",
        service_path.display()
    );
}

#[test]
fn service_qml_contains_companion_marker() {
    let service_path = plugin_dir().join("Service.qml");
    let service_content = fs::read_to_string(&service_path)
        .expect("failed to read Service.qml; file missing or unreadable?");

    assert!(
        service_content.contains("// omarchy-presence-unlock:companion"),
        "Service.qml must contain the exact marker '// omarchy-presence-unlock:companion' \
         (crates/cli/src/doctor.rs greps for this string to detect installed plugin version)"
    );
}

#[test]
fn service_qml_declares_ipc_target_and_required_methods() {
    let service_path = plugin_dir().join("Service.qml");
    let service_content = fs::read_to_string(&service_path)
        .expect("failed to read Service.qml; file missing or unreadable?");

    // IpcHandler target must be "presence-unlock" (used by omarchy-shell and Hyprland bindings).
    assert!(
        service_content.contains("presence-unlock"),
        "Service.qml must declare IPC target 'presence-unlock' \
         (used by omarchy-shell and generated Hyprland bindings to communicate with the plugin)"
    );

    // Must declare hold() method (called on Alt key press).
    assert!(
        service_content.contains("hold"),
        "Service.qml must declare hold() method (called on Alt key PRESS; starts 400ms hold timer)"
    );

    // Must declare release() method (called on Alt key release).
    assert!(
        service_content.contains("release"),
        "Service.qml must declare release() method (called on Alt key RELEASE; cancels hold timer)"
    );

    // No method may authenticate without the hold: the gesture is the user's
    // only expression of intent, and any session process can call this target.
    assert!(
        !service_content.contains("function confirm"),
        "Service.qml must not offer an IPC method that authenticates without the 400ms hold"
    );

    // Must declare ping() method (health check).
    assert!(
        service_content.contains("ping"),
        "Service.qml must declare ping() method (responds 'ok' for diagnostic health checks)"
    );
}
