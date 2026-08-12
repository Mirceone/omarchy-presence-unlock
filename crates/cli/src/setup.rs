//! Lock-screen integration: Hyprlock keybinding, or Omarchy's Quattro plugin.

mod quattro;

use crate::{atomic::write_atomic, devices};
use std::{env, fs, path::PathBuf, process::Command};

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}

/// # Errors
///
/// Returns an error when neither integration is available, or when the chosen
/// integration cannot be applied.
pub fn setup_omarchy() -> Result<(), String> {
    let commands = Command::new("omarchy")
        .args(["commands", "--all"])
        .output()
        .map_err(|error| error.to_string())?;
    if String::from_utf8_lossy(&commands.stdout).contains("omarchy plugin clone") {
        return quattro::setup();
    }
    setup_hyprlock()
}

fn setup_hyprlock() -> Result<(), String> {
    if !Command::new("hyprlock")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err("this Omarchy build has neither Quattro plugins nor Hyprlock".into());
    }
    devices::set_backend("hyprlock-confirm", None, None, &[])?;
    let bindings = home_dir()?.join(".config/hypr/bindings.lua");
    let binding_marker = "-- omarchy-presence-unlock Alt+Enter confirmation";
    let binding = format!(
        "\n{binding_marker}\no.bind(\"ALT + RETURN\", \"Presence unlock confirmation\", \"omarchy-presence-unlock confirm\", {{ locked = true }})\n"
    );
    let binding_text = fs::read_to_string(&bindings).map_err(|error| error.to_string())?;
    if !binding_text.contains(binding_marker) {
        write_atomic(&bindings, &format!("{binding_text}{binding}"), 0o644)?;
    }
    run(Command::new("hyprctl").arg("reload"))?;
    let validation = Command::new("hyprctl")
        .arg("configerrors")
        .output()
        .map_err(|error| error.to_string())?;
    if !validation.status.success() {
        return Err(String::from_utf8_lossy(&validation.stderr)
            .trim()
            .to_string());
    }
    if !validation.stdout.is_empty() {
        eprintln!(
            "Hyprland config validation output:\n{}",
            String::from_utf8_lossy(&validation.stdout)
        );
    }
    println!("Enabled Alt+Enter unlock confirmation for Hyprlock. Restart presenced.");
    Ok(())
}
