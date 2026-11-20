use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::led::{LED_FILE, STATUS_FILE};

const NAME_HINTS: &[&str] = &["sidepulsepro", "sidepulsedot", "pulsedot", "sidepulse"];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Device {
    pub root: PathBuf,
    pub name: String,
    pub model: String,
    pub led_count: u8,
    pub serial: Option<String>,
}

impl Device {
    pub fn target(&self) -> PathBuf {
        self.root.join(LED_FILE)
    }
}

pub fn mount_roots() -> Vec<PathBuf> {
    if let Some(roots) = std::env::var_os("SIDEPULSE_MOUNT_ROOTS") {
        return std::env::split_paths(&roots).collect();
    }
    if cfg!(target_os = "macos") {
        return vec![PathBuf::from("/Volumes")];
    }
    let user = std::env::var("USER").unwrap_or_default();
    vec![
        PathBuf::from(format!("/media/{user}")),
        PathBuf::from(format!("/run/media/{user}")),
        PathBuf::from("/mnt"),
    ]
}

pub fn discover() -> Vec<Device> {
    let mut devices = Vec::new();
    for root in mount_roots() {
        let Ok(entries) = fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "Macintosh HD" || name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(device) = describe(&path) {
                devices.push(device);
            }
        }
    }
    devices.sort_by_key(|d| d.name.to_lowercase());
    devices
}

pub fn describe(root: &Path) -> Option<Device> {
    let name = root.file_name()?.to_string_lossy().into_owned();
    let normalized: String = name.to_lowercase().chars().filter(char::is_ascii_alphanumeric).collect();
    let has_files = root.join(LED_FILE).exists() || root.join(STATUS_FILE).exists();
    if !has_files && !NAME_HINTS.iter().any(|h| normalized.contains(h)) {
        return None;
    }
    let serial = fs::read_to_string(root.join(STATUS_FILE)).ok().and_then(|status| {
        status
            .lines()
            .find_map(|l| l.strip_prefix("serial ").map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
    });
    let is_dot = serial.as_deref().is_some_and(|s| s.starts_with("SPD")) || normalized.contains("dot");
    Some(Device {
        root: root.to_path_buf(),
        name,
        model: if is_dot { "SidePulse Dot" } else { "SidePulse Pro" }.into(),
        led_count: if is_dot { 2 } else { 8 },
        serial,
    })
}

pub fn resolve(explicit: Option<&str>) -> Result<Device, String> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        let root = if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(LED_FILE)) {
            path.parent().map(Path::to_path_buf).unwrap_or(path)
        } else {
            path
        };
        return describe(&root).ok_or_else(|| format!("{} does not look like a SidePulse device", root.display()));
    }
    let mut devices = discover();
    match devices.len() {
        0 => Err("No SidePulse device found under /Volumes.".into()),
        1 => Ok(devices.remove(0)),
        _ => Err(format!(
            "Multiple devices found; pass --device with one of: {}",
            devices.iter().map(|d| d.root.display().to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}
