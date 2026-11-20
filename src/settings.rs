use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::led;
use crate::paths;
use crate::status::Mode;

pub const DEFAULT_SESSION_TIMEOUT_SECONDS: f64 = 3600.0;
pub const WORKING_MODES: [&str; 3] = ["working", "tool_running", "long_task_progress"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    Agent,
    Battery,
    Custom,
}

#[derive(Clone, Debug)]
pub struct DeviceSetting {
    pub id: String,
    pub path: String,
    pub display: Display,
    pub brightness: u8,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepPolicy {
    Never,
    Agents,
    Always,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub default_display: Display,
    pub devices: Vec<DeviceSetting>,
    pub session_timeout_seconds: f64,
    pub show_menu_bar_icon: bool,
    pub sleep_policy: SleepPolicy,
    pub sleep_min_battery_percent: f64,
    pub show_on_power_change: bool,
    pub power_change_preview_seconds: f64,
    pub lid_open_seconds: f64,
    pub lid_closed_seconds: f64,
    pub terminal_app: String,
    pub session_open: HashMap<String, String>,
    selections: HashMap<String, String>,
    custom_programs: HashMap<String, String>,
    pub custom: Vec<CustomAnimation>,
}

#[derive(Clone, Debug)]
pub struct CustomAnimation {
    pub id: String,
    pub name: String,
    pub file: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_display: Display::Agent,
            devices: Vec::new(),
            session_timeout_seconds: DEFAULT_SESSION_TIMEOUT_SECONDS,
            show_menu_bar_icon: true,
            sleep_policy: SleepPolicy::Agents,
            sleep_min_battery_percent: 20.0,
            show_on_power_change: true,
            power_change_preview_seconds: 7.0,
            lid_open_seconds: 1.0,
            lid_closed_seconds: 1.3,
            terminal_app: "terminal".into(),
            session_open: HashMap::new(),
            selections: HashMap::new(),
            custom_programs: HashMap::new(),
            custom: Vec::new(),
        }
    }
}

pub const BUILTIN_ANIMATIONS: &[(&str, &str)] = &[
    ("off", "Fade Off"),
    ("immediate-off", "Off"),
    ("idle-pulse", "Idle Pulse"),
    ("cyan-roll", "Cyan Roll"),
    ("cyan-complete", "Cyan Complete"),
    ("amber-pulse", "Amber Pulse"),
    ("solid-green", "Solid Green"),
    ("kitt", "Scanner"),
    ("kitt-red", "Red Scanner"),
    ("night-rider", "Night Rider"),
    ("ember-idle", "Ember Idle"),
    ("ember-tide", "Ember Tide"),
    ("ember-attention", "Ember Attention"),
    ("ember-complete", "Ember Complete"),
    ("ember-lid-open", "Ember Lid Open"),
    ("purple-idle", "Purple Idle"),
    ("purple-tide", "Purple Tide"),
    ("purple-attention", "Purple Attention"),
    ("purple-complete", "Purple Complete"),
    ("purple-lid-open", "Purple Lid Open"),
    ("lid-open", "Lid Open"),
    ("lid-closed", "Lid Closed"),
];

pub fn default_animation_id(key: &str) -> &'static str {
    match key {
        "working" | "tool_running" | "long_task_progress" => "cyan-roll",
        "waiting_for_input" | "blocked_error" => "amber-pulse",
        "completed" => "cyan-complete",
        "lid_open" => "lid-open",
        "lid_closed" => "lid-closed",
        _ => "idle-pulse",
    }
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from(&paths::settings_file())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let Value::Object(data) = &root else {
            return Self::default();
        };
        let animation_dir = path.parent().map(|p| p.join("animations")).unwrap_or_default();
        let mut settings = Self { default_display: display(data.get("led_display")).unwrap_or(Display::Agent), ..Self::default() };
        if let Some(Value::Array(devices)) = data.get("devices") {
            settings.devices = devices
                .iter()
                .filter_map(|d| {
                    let id = d.get("id")?.as_str()?.to_string();
                    Some(DeviceSetting {
                        path: d.get("path").and_then(Value::as_str).unwrap_or(&id).to_string(),
                        display: display(d.get("led_display")).unwrap_or(settings.default_display),
                        brightness: d
                            .get("brightness")
                            .and_then(Value::as_f64)
                            .map_or(255, |b| b.round().clamp(0.0, 255.0) as u8),
                        id,
                    })
                })
                .collect();
        }

        let agent_list = data.get("agent_list");
        for key in ["recent_session_retention_seconds", "idle_timeout_seconds", "session_timeout_seconds"] {
            let value = agent_list.and_then(|a| a.get(key)).or_else(|| data.get(key));
            if let Some(seconds) = value.and_then(Value::as_f64).filter(|s| *s >= 0.0) {
                settings.session_timeout_seconds = seconds;
            }
        }

        if let Some(show) = data.get("show_menu_bar_icon").and_then(Value::as_bool) {
            settings.show_menu_bar_icon = show;
        }
        settings.sleep_policy = match data.get("sleep_prevention_policy").and_then(Value::as_str) {
            Some("never") => SleepPolicy::Never,
            Some("always") => SleepPolicy::Always,
            _ => SleepPolicy::Agents,
        };
        if let Some(percent) = root.pointer("/sleep_prevention/min_battery_percent").and_then(Value::as_f64) {
            settings.sleep_min_battery_percent = percent;
        }
        if let Some(show) = root.pointer("/battery_monitoring/show_on_power_change").and_then(Value::as_bool) {
            settings.show_on_power_change = show;
        }
        if let Some(seconds) = root.pointer("/battery_monitoring/power_change_preview_seconds").and_then(Value::as_f64) {
            settings.power_change_preview_seconds = seconds.max(0.0);
        }
        if let Some(seconds) = lid_duration(data.get("lid_open_animation")) {
            settings.lid_open_seconds = seconds;
        }
        if let Some(seconds) = lid_duration(data.get("lid_closed_animation")) {
            settings.lid_closed_seconds = seconds;
        }
        if let Some(app) = root.pointer("/session_terminal/app").and_then(Value::as_str) {
            settings.terminal_app = app.to_string();
        }
        if let Some(Value::Object(prefs)) = data.get("session_open_preferences") {
            settings.session_open = prefs
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect();
        }

        if let Some(Value::Object(custom)) = data.get("custom_agent_animations") {
            for (id, raw) in custom {
                if !id.starts_with("custom:") {
                    continue;
                }
                let file = raw
                    .get("file")
                    .and_then(Value::as_str)
                    .filter(|f| !f.contains('/') && f.to_ascii_uppercase().ends_with(".LED"));
                let program = file
                    .and_then(|f| fs::read_to_string(animation_dir.join(f)).ok())
                    .or_else(|| raw.get("program").and_then(Value::as_str).map(String::from));
                if let Some(program) = program {
                    settings.custom_programs.insert(id.clone(), normalize_file_program(&program));
                    let name = raw.get("name").and_then(Value::as_str).unwrap_or(id).to_string();
                    let file = file.map(String::from).unwrap_or_else(|| format!("{}.LED", id.trim_start_matches("custom:")));
                    settings.custom.push(CustomAnimation { id: id.clone(), name, file });
                }
            }
        }

        settings.custom.sort_by_key(|c| c.name.to_lowercase());
        if let Some(Value::Object(animations)) = data.get("agent_animations") {
            for (key, raw) in animations {
                let style = raw.get("style").and_then(Value::as_str).unwrap_or("default");
                if style == "custom" {
                    if let Some(program) = raw.get("custom_program").and_then(Value::as_str).filter(|p| !p.trim().is_empty()) {
                        let id = format!("custom:inline-{key}");
                        settings.custom_programs.insert(id.clone(), normalize_file_program(program));
                        settings.selections.insert(key.clone(), id);
                    }
                    continue;
                }
                if style != "default" && (led::is_builtin(style) || settings.custom_programs.contains_key(style)) {
                    settings.selections.insert(key.clone(), style.to_string());
                }
            }
            if let Some(working) = WORKING_MODES.iter().find_map(|m| settings.selections.get(*m).cloned()) {
                for mode in WORKING_MODES {
                    settings.selections.insert(mode.to_string(), working.clone());
                }
            }
        }
        settings
    }

    pub fn animation_id(&self, key: &str) -> String {
        self.selections
            .get(key)
            .cloned()
            .unwrap_or_else(|| default_animation_id(key).to_string())
    }

    pub fn program_for_key(&self, key: &str, led_count: u8) -> String {
        let id = self.animation_id(key);
        self.custom_programs
            .get(&id)
            .cloned()
            .or_else(|| led::builtin(&id, led_count))
            .or_else(|| led::builtin(default_animation_id(key), led_count))
            .unwrap_or_else(|| "off".into())
    }

    pub fn program_for_mode(&self, mode: Mode, led_count: u8) -> String {
        self.program_for_key(mode.as_str(), led_count)
    }

    pub fn device(&self, root: &Path) -> Option<&DeviceSetting> {
        let root = root.to_string_lossy();
        self.devices.iter().find(|d| d.path == root || d.id == root)
    }
}

fn display(value: Option<&Value>) -> Option<Display> {
    match value?.as_str()? {
        "agent" => Some(Display::Agent),
        "battery" => Some(Display::Battery),
        "custom" => Some(Display::Custom),
        _ => None,
    }
}

fn lid_duration(value: Option<&Value>) -> Option<f64> {
    value?.get("duration_seconds")?.as_f64().filter(|s| *s > 0.0).map(|s| s.min(30.0))
}

fn normalize_file_program(program: &str) -> String {
    program.replace("\r\n", "\n").replace('\r', "\n").trim().to_string()
}

pub fn update_file(change: impl FnOnce(&mut serde_json::Map<String, Value>)) -> bool {
    let path = paths::settings_file();
    let mut root = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    change(&mut root);
    let Ok(mut text) = serde_json::to_string_pretty(&Value::Object(root)) else { return false };
    text.push('\n');
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &path).is_ok()
}

pub fn update_device(id: &str, name: &str, change: impl FnOnce(&mut serde_json::Map<String, Value>)) -> bool {
    update_file(|root| {
        let devices = root.entry("devices").or_insert_with(|| Value::Array(Vec::new()));
        if !devices.is_array() {
            *devices = Value::Array(Vec::new());
        }
        let Value::Array(list) = devices else { return };
        let index = list.iter().position(|d| d.get("id").and_then(Value::as_str) == Some(id) || d.get("path").and_then(Value::as_str) == Some(id));
        let index = index.unwrap_or_else(|| {
            list.push(serde_json::json!({"id": id, "path": id, "name": name, "led_display": "agent", "brightness": 255}));
            list.len() - 1
        });
        if let Some(Value::Object(entry)) = list.get_mut(index) {
            change(entry);
        }
    })
}

pub fn animations_dir() -> PathBuf {
    paths::settings_file().parent().map(|p| p.join("animations")).unwrap_or_default()
}

pub struct SettingsWatcher {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
    animation_stamp: Option<SystemTime>,
    pub current: Settings,
}

impl SettingsWatcher {
    pub fn new() -> Self {
        let path = paths::settings_file();
        let mut watcher = Self { stamp: None, animation_stamp: None, current: Settings::default(), path };
        watcher.refresh();
        watcher
    }

    pub fn refresh(&mut self) -> bool {
        let stamp = fs::metadata(&self.path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        let animation_stamp = self
            .path
            .parent()
            .and_then(|p| fs::metadata(p.join("animations")).ok())
            .and_then(|m| m.modified().ok());
        if stamp == self.stamp && animation_stamp == self.animation_stamp && self.stamp.is_some() {
            return false;
        }
        self.stamp = stamp;
        self.animation_stamp = animation_stamp;
        self.current = Settings::load_from(&self.path);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(json: &str, animations: &[(&str, &str)]) -> Settings {
        let dir = std::env::temp_dir().join(format!("sp-settings-{}-{}", std::process::id(), json.len()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("animations")).unwrap();
        for (name, body) in animations {
            fs::write(dir.join("animations").join(name), body).unwrap();
        }
        fs::write(dir.join("settings.json"), json).unwrap();
        let settings = Settings::load_from(&dir.join("settings.json"));
        let _ = fs::remove_dir_all(&dir);
        settings
    }

    #[test]
    fn resolves_custom_and_builtin_animations() {
        let settings = load(
            r#"{
              "agent_animations": {
                "completed": {"style": "solid-green"},
                "working": {"style": "custom:rainbow"},
                "waiting_for_input": {"style": "custom:missing"}
              },
              "custom_agent_animations": {"custom:rainbow": {"file": "rainbow.LED", "name": "Rainbow"}},
              "agent_list": {"idle_timeout_seconds": 1800, "recent_session_retention_seconds": 172800}
            }"#,
            &[("rainbow.LED", "#ff0000\r\nroll 2s\n")],
        );
        assert_eq!(settings.program_for_key("working", 8), "#ff0000\nroll 2s");
        assert_eq!(settings.program_for_key("tool_running", 8), "#ff0000\nroll 2s");
        assert_eq!(settings.animation_id("completed"), "solid-green");
        assert_eq!(settings.animation_id("waiting_for_input"), "amber-pulse");
        assert_eq!(settings.animation_id("idle_ready"), "idle-pulse");
        assert_eq!(settings.session_timeout_seconds, 1800.0);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let settings = Settings::load_from(Path::new("/nonexistent/settings.json"));
        assert_eq!(settings.sleep_policy, SleepPolicy::Agents);
        assert!(settings.program_for_key("working", 8).contains("#00E5FF"));
    }
}
