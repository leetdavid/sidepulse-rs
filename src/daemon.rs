use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::battery::{self, Battery};
use crate::device::{self, Device};
use crate::led;
use crate::paths;
use crate::protocol::{MAX_MESSAGE_BYTES, Request};
use crate::settings::{Display, SettingsWatcher, SleepPolicy};
use crate::state::{Snapshot, Store};
use crate::status::Mode;

const KEEPALIVE: Duration = Duration::from_secs(60);
const ERROR_RETRY: Duration = Duration::from_secs(10);
const AWAKE_GRACE: Duration = Duration::from_secs(300);
const SAVE_DELAY: Duration = Duration::from_secs(10);
const MAX_IDLE_WAIT: Duration = Duration::from_secs(60);
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

pub enum Event {
    Request(Request, Option<SyncSender<Value>>),
    Lid(bool),
    Power,
    VolumesChanged,
    SettingsChanged,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceView {
    pub name: String,
    pub root: PathBuf,
    pub model: String,
    pub display: String,
    #[serde(default = "full_brightness")]
    pub brightness: u8,
    #[serde(default)]
    pub led_count: u8,
    pub error: Option<String>,
}

fn full_brightness() -> u8 {
    255
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    pub snapshot: Snapshot,
    pub devices: Vec<DeviceView>,
    pub outputs_enabled: bool,
    pub sleep_policy: String,
    pub keep_awake: bool,
}

pub type Publisher = Box<dyn Fn(UiState) + Send>;

fn power_control() -> bool {
    std::env::var_os("SIDEPULSE_NO_POWER_CONTROL").is_none()
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub fn log(message: &str) {
    let secs = now_ms() / 1000;
    eprintln!("{secs} {message}");
}

struct WriteError {
    message: String,
    program: String,
    at: Instant,
}

#[derive(Default)]
struct Awake {
    last_mode: Option<Mode>,
    grace_until: Option<Instant>,
    requested: bool,
    #[cfg(target_os = "macos")]
    assertion: Option<crate::macos::SleepAssertion>,
    system_disabled: Option<bool>,
    display_slept: bool,
}

pub struct Engine {
    rx: Receiver<Event>,
    store: Store,
    store_path: PathBuf,
    store_dirty_since: Option<Instant>,
    settings: SettingsWatcher,
    devices: Vec<Device>,
    written: HashMap<PathBuf, String>,
    errors: HashMap<PathBuf, WriteError>,
    outputs_enabled: bool,
    hold_until: Option<Instant>,
    lid_closed: Option<bool>,
    lid_hold: bool,
    battery: Option<Battery>,
    battery_preview_until: Option<Instant>,
    full_charge_watts: f64,
    awake: Awake,
    sleep_helper: bool,
    next_keepalive: Instant,
    published: Option<UiState>,
    publisher: Option<Publisher>,
    last_mode: Option<Mode>,
}

impl Engine {
    pub fn new(rx: Receiver<Event>, publisher: Option<Publisher>) -> Self {
        let store_path = paths::state_dir().join("sessions.json");
        Self {
            rx,
            store: Store::load(&store_path),
            store_path,
            store_dirty_since: None,
            settings: SettingsWatcher::new(),
            devices: device::discover(),
            written: HashMap::new(),
            errors: HashMap::new(),
            outputs_enabled: true,
            hold_until: None,
            lid_closed: platform::lid_closed(),
            lid_hold: false,
            battery: platform::read_battery(),
            battery_preview_until: None,
            full_charge_watts: platform::full_charge_watts(),
            awake: Awake::default(),
            sleep_helper: power_control() && platform::sleep_helper_installed(),
            next_keepalive: Instant::now() + KEEPALIVE,
            published: None,
            publisher,
            last_mode: None,
        }
    }

    pub fn run(mut self) {
        log(&format!(
            "started: {} device(s), sleep helper {}",
            self.devices.len(),
            if self.sleep_helper { "installed" } else { "missing" }
        ));
        loop {
            let wait = self.next_deadline().saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(wait) {
                Ok(event) => {
                    if !self.handle(event) {
                        break;
                    }
                    while let Ok(event) = self.rx.try_recv() {
                        if !self.handle(event) {
                            self.shutdown();
                            return;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
        self.shutdown();
    }

    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Request(request, reply) => {
                let stop = matches!(request, Request::Stop);
                let response = self.handle_request(request);
                if let Some(reply) = reply {
                    let _ = reply.try_send(response);
                }
                return !stop;
            }
            Event::Lid(closed) => self.on_lid(closed),
            Event::Power => self.on_power(),
            Event::VolumesChanged => self.rescan(),
            Event::SettingsChanged => {}
            Event::Shutdown => return false,
        }
        true
    }

    fn handle_request(&mut self, request: Request) -> Value {
        match request {
            Request::Report(report) => {
                if self.store.submit(*report, now_ms()) {
                    self.store_dirty_since.get_or_insert_with(Instant::now);
                }
                json!({"ok": true})
            }
            Request::GetState => {
                let state = self.ui_state(&self.current_snapshot());
                json!({
                    "ok": true,
                    "state": state,
                    "battery": self.battery,
                    "lid_closed": self.lid_closed,
                    "system_sleep_disabled": self.awake.system_disabled,
                })
            }
            Request::WriteLed { program, target, hold_seconds } => self.manual_write(&program, target.as_deref(), hold_seconds),
            Request::SetOutputs { enabled } => {
                self.outputs_enabled = enabled;
                self.written.clear();
                log(&format!("outputs {}", if enabled { "enabled" } else { "paused" }));
                json!({"ok": true})
            }
            Request::HerdrSync { socket, agents } => {
                if self.store.herdr_sync(&socket, agents, now_ms()) {
                    self.store_dirty_since.get_or_insert_with(Instant::now);
                }
                json!({"ok": true})
            }
            Request::ClearSession { scope } => {
                if self.store.remove(&scope) {
                    self.store_dirty_since.get_or_insert_with(Instant::now);
                }
                json!({"ok": true})
            }
            Request::OpenSettings => {
                #[cfg(target_os = "macos")]
                crate::settings_ui::request_open();
                json!({"ok": true})
            }
            Request::Stop => json!({"ok": true}),
        }
    }

    fn manual_write(&mut self, program: &str, target: Option<&str>, hold_seconds: f64) -> Value {
        let program = led::decode_escapes(program);
        if let Err(error) = led::validate(&program) {
            return json!({"ok": false, "error": error});
        }
        let targets: Vec<Device> = match target {
            Some(path) => match device::resolve(Some(path)) {
                Ok(device) => vec![device],
                Err(error) => return json!({"ok": false, "error": error}),
            },
            None => self.devices.clone(),
        };
        if targets.is_empty() {
            return json!({"ok": false, "error": "No SidePulse device connected"});
        }
        let mut written = Vec::new();
        for device in &targets {
            if let Err(error) = crate::ledsim::validate(&program, device.led_count) {
                return json!({"ok": false, "error": format!("invalid LED program: {error}")});
            }
            if let Err(error) = led::write_synced(&device.target(), &program) {
                return json!({"ok": false, "error": format!("{}: {error}", device.target().display())});
            }
            self.written.insert(device.root.clone(), program.clone());
            written.push(device.target());
        }
        if hold_seconds > 0.0 {
            self.hold_until = Some(Instant::now() + Duration::from_secs_f64(hold_seconds.min(3600.0)));
        }
        json!({"ok": true, "targets": written})
    }

    fn current_snapshot(&self) -> Snapshot {
        self.store.snapshot(now_ms(), self.timeout_ms())
    }

    fn timeout_ms(&self) -> u64 {
        (self.settings.current.session_timeout_seconds * 1000.0) as u64
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.settings.refresh() {
            log("settings reloaded");
            self.written.clear();
        }
        if self.store.prune(now_ms(), self.timeout_ms()) {
            self.store_dirty_since.get_or_insert(now);
        }
        let snapshot = self.current_snapshot();
        if self.last_mode != Some(snapshot.mode) {
            log(&format!("state={} active={}", snapshot.mode.as_str(), snapshot.active_count));
            self.last_mode = Some(snapshot.mode);
        }
        self.update_awake(snapshot.mode, now);
        if now >= self.next_keepalive {
            trim_log();
            self.keepalive();
            self.next_keepalive = now + KEEPALIVE;
        }
        self.sync_outputs(snapshot.mode, now);
        self.publish(&snapshot);
        if self.store_dirty_since.is_some_and(|since| now.duration_since(since) >= SAVE_DELAY) {
            self.save_store();
        }
    }

    fn next_deadline(&self) -> Instant {
        let now = Instant::now();
        let now_wall = now_ms();
        let mut deadline = (now + MAX_IDLE_WAIT).min(self.next_keepalive);
        if let Some(at) = self.store.next_change_ms(now_wall, self.timeout_ms()) {
            deadline = deadline.min(now + Duration::from_millis(at.saturating_sub(now_wall)));
        }
        for at in [self.hold_until, self.battery_preview_until, self.awake.grace_until].into_iter().flatten() {
            if at > now {
                deadline = deadline.min(at);
            }
        }
        for error in self.errors.values() {
            deadline = deadline.min(error.at + ERROR_RETRY);
        }
        if let Some(since) = self.store_dirty_since {
            deadline = deadline.min(since + SAVE_DELAY);
        }
        deadline
    }

    fn update_awake(&mut self, mode: Mode, now: Instant) {
        let awake = &mut self.awake;
        awake.requested = match mode {
            Mode::Working | Mode::ToolRunning | Mode::LongTaskProgress => {
                awake.grace_until = None;
                true
            }
            Mode::Completed | Mode::WaitingForInput | Mode::BlockedError => {
                if awake.last_mode != Some(mode) || awake.grace_until.is_none() {
                    awake.grace_until = Some(now + AWAKE_GRACE);
                }
                awake.grace_until.is_some_and(|until| now < until)
            }
            _ => awake.grace_until.is_some_and(|until| now < until),
        };
        awake.last_mode = Some(mode);

        let settings = &self.settings.current;
        let policy_hold = match settings.sleep_policy {
            SleepPolicy::Always => true,
            SleepPolicy::Agents => awake.requested,
            SleepPolicy::Never => false,
        };
        let safeguard = self.battery.as_ref().is_some_and(|b| {
            b.present && !b.plugged && settings.sleep_min_battery_percent > 0.0 && f64::from(b.percent) < settings.sleep_min_battery_percent
        });
        let keep_awake = policy_hold && !safeguard;

        #[cfg(target_os = "macos")]
        if power_control() && keep_awake != awake.assertion.is_some() {
            awake.assertion = if keep_awake { crate::macos::SleepAssertion::acquire("SidePulse: agents are working") } else { None };
            log(&format!("keep_awake={}", if keep_awake { "active" } else { "released" }));
        }

        if !self.sleep_helper {
            return;
        }
        let plugged = self.battery.as_ref().is_none_or(|b| b.plugged);
        let disable_sleep = keep_awake && !(plugged && platform::external_display_active());
        if awake.system_disabled != Some(disable_sleep) {
            match platform::set_system_sleep_disabled(disable_sleep) {
                Ok(()) => log(&format!("closed_lid_awake={}", if disable_sleep { "active" } else { "released" })),
                Err(error) => log(&format!("closed_lid_awake error: {error}")),
            }
            awake.system_disabled = Some(disable_sleep);
        }
        if self.lid_closed == Some(true) && disable_sleep && !awake.display_slept && !platform::external_display_active() {
            platform::display_sleep_now();
            awake.display_slept = true;
        }
    }

    fn lid_hold_should_continue(&self) -> bool {
        self.lid_closed == Some(true) && !self.awake.assertion_held()
    }

    fn on_lid(&mut self, closed: bool) {
        let previous = self.lid_closed.replace(closed);
        if previous.is_none() || previous == Some(closed) {
            return;
        }
        log(&format!("lid_state={}", if closed { "closed" } else { "open" }));
        self.awake.display_slept = false;
        if closed {
            let run = match self.settings.current.sleep_policy {
                SleepPolicy::Always => false,
                SleepPolicy::Agents => !self.awake.requested,
                SleepPolicy::Never => true,
            };
            if run {
                self.play_animation("lid_closed");
                self.lid_hold = self.lid_hold_should_continue();
            }
        } else {
            self.lid_hold = false;
            self.play_animation("lid_open");
        }
    }

    fn play_animation(&mut self, key: &str) {
        if !self.outputs_enabled {
            return;
        }
        let settings = &self.settings.current;
        let id = settings.animation_id(key);
        let duration = match id.as_str() {
            "lid-open" => 1.0,
            "lid-closed" => 1.3,
            _ if key == "lid_open" => settings.lid_open_seconds,
            _ => settings.lid_closed_seconds,
        };
        for device in self.devices.clone() {
            let Some((display, brightness)) = self.display_for(&device) else { continue };
            let _ = display;
            let program = led::apply_brightness(&settings.program_for_key(key, device.led_count), brightness);
            match led::write_synced(&device.target(), &program) {
                Ok(()) => {
                    log(&format!("animation={key} device={}", device.name));
                    self.written.insert(device.root.clone(), program);
                }
                Err(error) => log(&format!("animation error {}: {error}", device.name)),
            }
        }
        self.hold_until = Some(Instant::now() + Duration::from_secs_f64(duration + 0.15));
    }

    fn on_power(&mut self) {
        let battery = platform::read_battery();
        if let (Some(old), Some(new)) = (&self.battery, &battery)
            && old.plugged != new.plugged
        {
            log(&format!("power={}", if new.plugged { "plugged" } else { "battery" }));
            let settings = &self.settings.current;
            if settings.show_on_power_change && settings.power_change_preview_seconds > 0.0 {
                self.battery_preview_until = Some(Instant::now() + Duration::from_secs_f64(settings.power_change_preview_seconds));
            }
        }
        self.battery = battery;
        if let Some(closed) = platform::lid_closed() {
            self.on_lid(closed);
        }
    }

    fn rescan(&mut self) {
        let devices = device::discover();
        if devices != self.devices {
            for gone in self.devices.iter().filter(|d| !devices.contains(d)) {
                log(&format!("device removed: {}", gone.root.display()));
                self.written.remove(&gone.root);
                self.errors.remove(&gone.root);
            }
            for new in devices.iter().filter(|d| !self.devices.contains(d)) {
                log(&format!("device added: {} ({})", new.root.display(), new.model));
                self.written.remove(&new.root);
            }
            self.devices = devices;
        }
    }

    fn keepalive(&mut self) {
        let mut missing = false;
        for device in &self.devices {
            match led::read_uncached(&device.target(), led::MAX_BYTES) {
                Ok(bytes) => {
                    if let Some(expected) = self.written.get(&device.root)
                        && bytes != expected.as_bytes()
                    {
                        log(&format!("device {} lost its program; rewriting", device.name));
                        self.written.remove(&device.root);
                    }
                }
                Err(_) => missing = true,
            }
        }
        if missing || self.devices.is_empty() {
            self.rescan();
        }
        if let Some(closed) = platform::lid_closed() {
            self.on_lid(closed);
        }
    }

    fn display_for(&self, device: &Device) -> Option<(Display, u8)> {
        let settings = &self.settings.current;
        let setting = settings.device(&device.root);
        let display = setting.map_or(settings.default_display, |s| s.display);
        (display != Display::Custom).then(|| (display, setting.map_or(255, |s| s.brightness)))
    }

    fn sync_outputs(&mut self, mode: Mode, now: Instant) {
        if !self.outputs_enabled || self.hold_until.is_some_and(|until| now < until) {
            return;
        }
        self.hold_until = None;
        if self.lid_hold {
            if self.lid_hold_should_continue() {
                return;
            }
            self.lid_hold = false;
        }
        if self.battery_preview_until.is_some_and(|until| now >= until) {
            self.battery_preview_until = None;
        }
        let preview = self.battery_preview_until.is_some();
        for device in self.devices.clone() {
            let Some((display, brightness)) = self.display_for(&device) else { continue };
            let program = match (&self.battery, preview || display == Display::Battery) {
                (Some(battery), true) => battery::program(battery, device.led_count, self.full_charge_watts, brightness),
                _ => led::apply_brightness(&self.settings.current.program_for_mode(mode, device.led_count), brightness),
            };
            if self.written.get(&device.root) == Some(&program) {
                continue;
            }
            if let Some(error) = self.errors.get(&device.root)
                && error.program == program
                && now < error.at + ERROR_RETRY
            {
                continue;
            }
            if let Err(error) = led::validate(&program) {
                log(&format!("invalid program for {}: {error}", device.name));
                continue;
            }
            match led::write_synced(&device.target(), &program) {
                Ok(()) => {
                    log(&format!("leds={} device={}", mode.as_str(), device.name));
                    self.written.insert(device.root.clone(), program);
                    self.errors.remove(&device.root);
                }
                Err(error) => {
                    log(&format!("write error {}: {error}", device.name));
                    self.written.remove(&device.root);
                    self.errors.insert(device.root.clone(), WriteError { message: error.to_string(), program, at: now });
                }
            }
        }
    }

    fn ui_state(&self, snapshot: &Snapshot) -> UiState {
        let settings = &self.settings.current;
        UiState {
            snapshot: snapshot.clone(),
            devices: self
                .devices
                .iter()
                .map(|d| DeviceView {
                    name: d.name.clone(),
                    root: d.root.clone(),
                    model: d.model.clone(),
                    display: match self.display_for(d) {
                        Some((Display::Battery, _)) => "battery",
                        Some(_) => "agent",
                        None => "manual",
                    }
                    .into(),
                    brightness: settings.device(&d.root).map_or(255, |s| s.brightness),
                    led_count: d.led_count,
                    error: self.errors.get(&d.root).map(|e| e.message.clone()),
                })
                .collect(),
            outputs_enabled: self.outputs_enabled,
            sleep_policy: match settings.sleep_policy {
                SleepPolicy::Never => "never",
                SleepPolicy::Agents => "agents",
                SleepPolicy::Always => "always",
            }
            .into(),
            keep_awake: self.awake.assertion_held(),
        }
    }

    fn publish(&mut self, snapshot: &Snapshot) {
        if self.publisher.is_none() {
            return;
        }
        let state = self.ui_state(snapshot);
        if self.published.as_ref() == Some(&state) {
            return;
        }
        self.published = Some(state.clone());
        if let Some(publisher) = &self.publisher {
            publisher(state);
        }
    }

    fn save_store(&mut self) {
        if let Err(error) = self.store.save(&self.store_path) {
            log(&format!("could not save sessions: {error}"));
        }
        self.store_dirty_since = None;
    }

    fn shutdown(&mut self) {
        log("stopping");
        self.save_store();
        if self.sleep_helper && self.awake.system_disabled == Some(true) {
            let _ = platform::set_system_sleep_disabled(false);
        }
        #[cfg(target_os = "macos")]
        {
            self.awake.assertion = None;
        }
        let _ = fs::remove_file(paths::socket());
        thread::sleep(Duration::from_millis(50));
        std::process::exit(0);
    }
}

impl Awake {
    fn assertion_held(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.assertion.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::macos::{display_sleep_now, external_display_active, full_charge_watts, lid_closed, read_battery, set_system_sleep_disabled, sleep_helper_installed};
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use crate::battery::Battery;
    pub fn lid_closed() -> Option<bool> {
        None
    }
    pub fn read_battery() -> Option<Battery> {
        None
    }
    pub fn full_charge_watts() -> f64 {
        100.0
    }
    pub fn sleep_helper_installed() -> bool {
        false
    }
    pub fn external_display_active() -> bool {
        false
    }
    pub fn set_system_sleep_disabled(_disabled: bool) -> Result<(), String> {
        Ok(())
    }
    pub fn display_sleep_now() {}
}

pub fn bind_socket() -> Result<UnixListener, String> {
    let path = paths::socket();
    let dir = paths::state_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err("daemon already running".into());
        }
        let _ = fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(listener)
}

pub fn serve(listener: UnixListener, tx: Sender<Event>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let mut data = Vec::new();
        let response = match (&mut stream).take(MAX_MESSAGE_BYTES + 1).read_to_end(&mut data) {
            Err(error) => json!({"ok": false, "error": error.to_string()}),
            Ok(_) if data.len() as u64 > MAX_MESSAGE_BYTES => json!({"ok": false, "error": "request too large"}),
            Ok(_) => match serde_json::from_slice::<Request>(&data) {
                Err(error) => json!({"ok": false, "error": error.to_string()}),
                Ok(request) => {
                    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                    if tx.send(Event::Request(request, Some(reply_tx))).is_err() {
                        return;
                    }
                    reply_rx
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(|_| json!({"ok": false, "error": "daemon busy"}))
                }
            },
        };
        let _ = stream.write_all(&serde_json::to_vec(&response).unwrap_or_default());
    }
}

#[cfg(target_os = "macos")]
pub fn watch_volumes(tx: Sender<Event>) {
    use std::os::fd::AsRawFd;
    let Ok(dir) = fs::File::open("/Volumes") else { return };
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            return;
        }
        let change = libc::kevent {
            ident: dir.as_raw_fd() as usize,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_LINK,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut event: libc::kevent = std::mem::zeroed();
        if libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) < 0 {
            return;
        }
        loop {
            let n = libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, std::ptr::null());
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            thread::sleep(Duration::from_millis(750));
            if tx.send(Event::VolumesChanged).is_err() {
                return;
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn watch_volumes(_tx: Sender<Event>) {}

pub fn trim_log() {
    let path = paths::log_file();
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::copy(&path, path.with_extension("log.old"));
        if let Ok(file) = OpenOptions::new().write(true).open(&path) {
            let _ = file.set_len(0);
        }
    }
}

pub fn block_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

pub fn wait_for_signals(set: libc::sigset_t, tx: Sender<Event>) {
    let mut signal = 0;
    unsafe { libc::sigwait(&set, &mut signal) };
    log(&format!("received signal {signal}"));
    let _ = tx.send(Event::Shutdown);
}
