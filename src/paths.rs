use std::env;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_dir() -> PathBuf {
    match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("sidepulse").join("agent-monitor"),
        _ => home().join(".config").join("sidepulse").join("agent-monitor"),
    }
}

pub fn settings_file() -> PathBuf {
    env::var_os("SIDEPULSE_SETTINGS").map(PathBuf::from).unwrap_or_else(|| config_dir().join("settings.json"))
}

pub fn state_dir() -> PathBuf {
    env::var_os("SIDEPULSE_RS_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("state").join("sidepulse-rs"))
}

pub fn socket() -> PathBuf {
    state_dir().join("daemon.sock")
}

pub fn log_file() -> PathBuf {
    state_dir().join("daemon.log")
}

pub fn install_dir() -> PathBuf {
    home().join(".local").join("share").join("sidepulse-rs")
}

pub fn installed_binary() -> PathBuf {
    install_dir().join("bin").join("sidepulse")
}
