use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSEventModifierFlags, NSImage, NSImageSymbolConfiguration, NSMenu, NSMenuDelegate, NSMenuItem, NSSlider,
    NSSquareStatusItemLength, NSStatusBar, NSStatusBarButton, NSStatusItem, NSView, NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSNumber, NSObjectProtocol, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CATransaction, kCAGravityResizeAspect,
    kCAMediaTimingFunctionEaseInEaseOut, kCAMediaTimingFunctionLinear,
};

use crate::daemon::{DeviceView, Event, Publisher, UiState, now_ms};
use crate::paths;
use crate::protocol::Request;
use crate::settings::{self, Settings};
use crate::state::Session;
use crate::status::{self, Mode};

const MAX_ROWS: usize = 10;
const ICON: f64 = 18.0;
const DISPLAYS: [(&str, &str); 3] = [("agent", "Agent Status"), ("battery", "Battery Level"), ("custom", "Manual")];

static LATEST: Mutex<Option<UiState>> = Mutex::new(None);
static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    static ICONS: RefCell<HashMap<String, Option<Retained<NSImage>>>> = RefCell::new(HashMap::new());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Motion {
    Still,
    Spin,
    Pulse,
}

struct Ui {
    item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _controller: Retained<Controller>,
    glyph_view: Retained<GlyphView>,
    glyph: Retained<CALayer>,
    shown: Option<(String, Motion, bool, String)>,
}

#[derive(Default)]
struct Ivars {
    rows: RefCell<Vec<Session>>,
    devices: RefCell<Vec<DeviceView>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SidePulseGlyphView"]
    struct GlyphView;

    impl GlyphView {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: CGPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SidePulseMenuController"]
    #[ivars = Ivars]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.rebuild(menu);
        }
    }

    impl Controller {
        #[unsafe(method(openSession:))]
        fn open_session(&self, sender: &NSMenuItem) {
            if let Some(session) = self.row(sender) {
                open_session(&session, &Settings::load());
            }
        }

        #[unsafe(method(resumeSession:))]
        fn resume_session(&self, sender: &NSMenuItem) {
            if let Some(session) = self.row(sender) {
                resume_in_terminal(&session, &Settings::load());
            }
        }

        #[unsafe(method(setPolicy:))]
        fn set_policy(&self, sender: &NSMenuItem) {
            let policy = match sender.tag() {
                0 => "never",
                2 => "always",
                _ => "agents",
            };
            settings::update_file(|root| {
                root.insert("sleep_prevention_policy".into(), policy.into());
            });
            send(Event::SettingsChanged);
        }

        #[unsafe(method(setDisplay:))]
        fn set_display(&self, sender: &NSMenuItem) {
            let tag = sender.tag();
            let Some(device) = self.ivars().devices.borrow().get((tag / 10) as usize).cloned() else { return };
            let display = DISPLAYS[(tag % 10) as usize].0;
            settings::update_device(&device.root.to_string_lossy(), &device.name, |entry| {
                entry.insert("led_display".into(), display.into());
            });
            send(Event::SettingsChanged);
        }

        #[unsafe(method(setBrightness:))]
        fn set_brightness(&self, sender: &NSSlider) {
            let Some(device) = self.ivars().devices.borrow().get(sender.tag() as usize).cloned() else { return };
            let value = sender.doubleValue().round().clamp(0.0, 255.0) as u64;
            settings::update_device(&device.root.to_string_lossy(), &device.name, |entry| {
                entry.insert("brightness".into(), value.into());
            });
            send(Event::SettingsChanged);
        }

        #[unsafe(method(toggleOutputs:))]
        fn toggle_outputs(&self, _sender: &NSMenuItem) {
            let enabled = LATEST.lock().ok().and_then(|s| s.as_ref().map(|s| s.outputs_enabled)).unwrap_or(true);
            send(Event::Request(Request::SetOutputs { enabled: !enabled }, None));
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: &NSMenuItem) {
            crate::settings_ui::open(self.mtm());
        }

        #[unsafe(method(hideIcon:))]
        fn hide_icon(&self, _sender: &NSMenuItem) {
            settings::update_file(|root| {
                root.insert("show_menu_bar_icon".into(), false.into());
            });
            set_icon_visible(false);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: &NSMenuItem) {
            send(Event::Shutdown);
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn row(&self, sender: &NSMenuItem) -> Option<Session> {
        usize::try_from(sender.tag()).ok().and_then(|i| self.ivars().rows.borrow().get(i).cloned())
    }

    fn target(&self) -> &AnyObject {
        let object: &NSObject = self;
        object
    }

    fn item(&self, title: &str, action: Option<Sel>, key: &str) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(title), action, &NSString::from_str(key))
        };
        if action.is_some() {
            unsafe { item.setTarget(Some(self.target())) };
        } else {
            item.setEnabled(false);
        }
        item
    }

    fn rebuild(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let state = LATEST.lock().ok().and_then(|s| s.clone());
        let Some(state) = state else {
            menu.addItem(&self.item("SidePulse is starting…", None, ""));
            return;
        };
        let snapshot = &state.snapshot;

        let headline = if snapshot.active_count > 0 {
            format!("SidePulse — {} · {} active", snapshot.mode.label(), snapshot.active_count)
        } else {
            format!("SidePulse — {}", snapshot.mode.label())
        };
        menu.addItem(&self.item(&headline, None, ""));
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(&NSString::from_str("Agents"), mtm));
        let mut rows: Vec<Session> = snapshot.sessions.iter().chain(&snapshot.recent).cloned().collect();
        rows.sort_by(|a, b| a.mode.priority().cmp(&b.mode.priority()).then(b.updated_ms.cmp(&a.updated_ms)));
        rows.truncate(MAX_ROWS);
        if rows.is_empty() {
            menu.addItem(&self.item("No recent sessions", None, ""));
        }
        let now = now_ms();
        let herdr_host = rows.iter().any(|s| s.herdr_pane.is_some() && s.terminal.is_none()).then(|| crate::macos::app_hosting("herdr")).flatten();
        for (index, session) in rows.iter().enumerate() {
            let title = row_title(session);
            let item = self.item(&title, Some(sel!(openSession:)), "");
            item.setTag(index as isize);
            if let Some(glyph) = symbol(row_symbol(session.mode), session.mode.label()) {
                unsafe { item.setOnStateImage(Some(&glyph)) };
                item.setState(NSControlStateValueOn);
            }
            if let Some(icon) = host_icon(session, herdr_host.as_deref()) {
                item.setImage(Some(&icon));
            }
            item.setToolTip(Some(&NSString::from_str(&row_tooltip(session, now))));
            menu.addItem(&item);

            let resume = self.item(&format!("Resume “{}” in Terminal", status::truncate(&title, 48)), Some(sel!(resumeSession:)), "");
            resume.setTag(index as isize);
            resume.setAlternate(true);
            resume.setKeyEquivalentModifierMask(NSEventModifierFlags::Option);
            menu.addItem(&resume);
        }
        *self.ivars().rows.borrow_mut() = rows;

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(&NSString::from_str("Devices"), mtm));
        if state.devices.is_empty() {
            menu.addItem(&self.item("No devices", None, ""));
        }
        for (index, device) in state.devices.iter().enumerate() {
            let label = match &device.error {
                Some(_) => format!("⚠ {} — {}", device.name, device.model),
                None => format!("{} — {}", device.name, device.model),
            };
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(&label), None, &NSString::new())
            };
            item.setSubmenu(Some(&self.device_menu(index, device)));
            menu.addItem(&item);
        }
        *self.ivars().devices.borrow_mut() = state.devices.clone();

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(&NSString::from_str("Closed-Lid Sleep Prevention"), mtm));
        for (tag, (key, label)) in [("never", "Never"), ("agents", "When Agents Work"), ("always", "Always")].into_iter().enumerate() {
            let item = self.item(label, Some(sel!(setPolicy:)), "");
            item.setTag(tag as isize);
            item.setState(if state.sleep_policy == key { NSControlStateValueOn } else { NSControlStateValueOff });
            menu.addItem(&item);
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let toggle = if state.outputs_enabled { "Pause LED Updates" } else { "Resume LED Updates" };
        menu.addItem(&self.item(toggle, Some(sel!(toggleOutputs:)), ""));
        menu.addItem(&self.item("Settings…", Some(sel!(openSettings:)), ","));
        menu.addItem(&self.item("Hide Menu Bar Icon", Some(sel!(hideIcon:)), ""));
        menu.addItem(&self.item("Quit SidePulse", Some(sel!(quit:)), "q"));
    }

    fn device_menu(&self, index: usize, device: &DeviceView) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        if let Some(error) = &device.error {
            menu.addItem(&self.item(&format!("⚠ {}", status::truncate(error, 60)), None, ""));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(&NSString::from_str("LEDs Show"), mtm));
        for (offset, (key, label)) in DISPLAYS.iter().enumerate() {
            let item = self.item(label, Some(sel!(setDisplay:)), "");
            item.setTag((index * 10 + offset) as isize);
            let selected = if *key == "custom" { device.display == "manual" } else { device.display == *key };
            item.setState(if selected { NSControlStateValueOn } else { NSControlStateValueOff });
            menu.addItem(&item);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&NSMenuItem::sectionHeaderWithTitle(&NSString::from_str("Brightness"), mtm));
        let container = NSView::initWithFrame(NSView::alloc(mtm), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(230.0, 28.0)));
        let slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(f64::from(device.brightness), 0.0, 255.0, Some(self.target()), Some(sel!(setBrightness:)), mtm)
        };
        slider.setFrame(CGRect::new(CGPoint::new(18.0, 4.0), CGSize::new(196.0, 20.0)));
        slider.setContinuous(false);
        slider.setTag(index as isize);
        container.addSubview(&slider);
        let slider_item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::new(), None, &NSString::new()) };
        slider_item.setView(Some(&container));
        menu.addItem(&slider_item);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.item(&format!("{} · {}", device.model, device.root.display()), None, ""));
        menu
    }
}

pub fn send(event: Event) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(event);
    }
}

pub fn latest() -> Option<UiState> {
    LATEST.lock().ok().and_then(|s| s.clone())
}

pub fn set_icon_visible(visible: bool) {
    UI.with(|ui| {
        if let Some(ui) = ui.borrow().as_ref() {
            ui.item.setVisible(visible);
        }
    });
}

fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), Some(&NSString::from_str(description)))?;
    image.setTemplate(true);
    Some(image)
}

fn tinted_symbol(name: &str, dark: bool) -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    let color = if dark { NSColor::whiteColor() } else { NSColor::colorWithCalibratedWhite_alpha(0.0, 0.85) };
    let config = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&[color]));
    let image = image.imageWithSymbolConfiguration(&config)?;
    image.setTemplate(false);
    image.setSize(CGSize::new(ICON - 3.0, ICON - 3.0));
    Some(image)
}

fn icon_for(state: &UiState) -> (&'static str, Motion) {
    if !state.outputs_enabled {
        return ("pause.circle", Motion::Still);
    }
    match state.snapshot.mode {
        Mode::WaitingForInput | Mode::BlockedError => ("questionmark.circle", Motion::Pulse),
        Mode::Working | Mode::ToolRunning | Mode::LongTaskProgress => ("arrow.triangle.2.circlepath", Motion::Spin),
        Mode::Completed => ("checkmark.circle", Motion::Still),
        Mode::IdleReady | Mode::Unknown => ("circle", Motion::Still),
    }
}

fn row_symbol(mode: Mode) -> &'static str {
    match mode {
        Mode::WaitingForInput => "questionmark.circle.fill",
        Mode::BlockedError => "exclamationmark.triangle.fill",
        Mode::Working | Mode::ToolRunning | Mode::LongTaskProgress => "arrow.triangle.2.circlepath",
        Mode::Completed => "checkmark.circle",
        Mode::IdleReady | Mode::Unknown => "circle",
    }
}

fn host_bundle(session: &Session) -> Option<&'static str> {
    let kind = session.origin_kind.as_deref().unwrap_or("");
    if kind.ends_with("_vscode") {
        return Some("com.microsoft.VSCode");
    }
    if kind.ends_with("_cursor") || kind == "cursor_app" {
        return Some("com.todesktop.230313mzl4w4u92");
    }
    if kind.ends_with("_app") {
        return provider_bundle(&session.provider);
    }
    session.terminal.as_deref().and_then(terminal_bundle).or_else(|| provider_bundle(&session.provider))
}

fn provider_bundle(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("com.anthropic.claudefordesktop"),
        "codex" => Some("com.openai.codex"),
        "cursor" => Some("com.todesktop.230313mzl4w4u92"),
        _ => None,
    }
}

fn host_icon(session: &Session, herdr_host: Option<&str>) -> Option<Retained<NSImage>> {
    let path = match (session.herdr_pane.is_some() && session.terminal.is_none(), herdr_host) {
        (true, Some(app)) => Some(app.to_string()),
        _ => host_bundle(session).and_then(app_path),
    }?;
    ICONS.with(|cache| {
        cache
            .borrow_mut()
            .entry(path.clone())
            .or_insert_with(|| {
                let icon = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&path));
                icon.setSize(CGSize::new(16.0, 16.0));
                Some(icon)
            })
            .clone()
    })
}

fn app_path(bundle: &str) -> Option<String> {
    let url = NSWorkspace::sharedWorkspace().URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle))?;
    url.path().map(|p| p.to_string())
}

fn row_title(session: &Session) -> String {
    let project = session.cwd.as_deref().and_then(status::project_name);
    let mut title = session.title.clone().unwrap_or_else(|| session.display_name.clone());
    if session.mode == Mode::WaitingForInput
        && let Some(message) = session.message.as_deref().filter(|m| !m.trim().is_empty())
    {
        title = status::truncate(&message.split_whitespace().collect::<Vec<_>>().join(" "), 120);
    }
    match project {
        Some(project) if !title.to_lowercase().contains(&project.to_lowercase()) => format!("{title}  {project}"),
        _ => title,
    }
}

fn row_tooltip(session: &Session, now: u64) -> String {
    let age = now.saturating_sub(session.updated_ms) / 1000;
    let age = if age < 60 { format!("{age}s ago") } else if age < 3600 { format!("{}m ago", age / 60) } else { format!("{}h ago", age / 3600) };
    let mut lines = vec![format!("{} · {} · {age}", session.mode.label(), session.origin.as_deref().unwrap_or(&session.provider))];
    if let Some(message) = session.message.as_deref().filter(|m| !m.is_empty()) {
        lines.push(status::truncate(message, 300));
    }
    lines.join("\n")
}

fn is_dark(button: &NSStatusBarButton) -> bool {
    let names = NSArray::from_slice(&[unsafe { NSAppearanceNameAqua }, unsafe { NSAppearanceNameDarkAqua }]);
    button
        .effectiveAppearance()
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|name| &*name == unsafe { NSAppearanceNameDarkAqua })
}

fn animation(key_path: &str, from: f64, to: f64, duration: f64, timing: &objc2_quartz_core::CAMediaTimingFunctionName, reverse: bool) -> Retained<CABasicAnimation> {
    let animation = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(key_path)));
    unsafe {
        animation.setFromValue(Some(&NSNumber::numberWithDouble(from)));
        animation.setToValue(Some(&NSNumber::numberWithDouble(to)));
    }
    animation.setDuration(duration);
    animation.setRepeatCount(f32::INFINITY);
    animation.setAutoreverses(reverse);
    animation.setTimingFunction(Some(&CAMediaTimingFunction::functionWithName(timing)));
    animation
}

fn refresh_icon() {
    let Some(state) = latest() else { return };
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let Some(ui) = ui.as_mut() else { return };
        let mtm = MainThreadMarker::new().expect("main thread");
        let Some(button) = ui.item.button(mtm) else { return };
        let (name, mut motion) = icon_for(&state);
        if NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            motion = Motion::Still;
        }
        let dark = is_dark(&button);
        let mut tooltip = format!("SidePulse: {}", state.snapshot.mode.label());
        if state.snapshot.active_count > 0 {
            tooltip.push_str(&format!(" ({} active)", state.snapshot.active_count));
        }
        if state.devices.iter().any(|d| d.error.is_some()) {
            tooltip.push_str(" — LED output failed");
        }
        let key = (name.to_string(), motion, dark, tooltip.clone());
        if ui.shown.as_ref() == Some(&key) {
            return;
        }
        button.setToolTip(Some(&NSString::from_str(&tooltip)));
        ui.glyph.removeAllAnimations();
        if motion == Motion::Still {
            ui.glyph_view.setHidden(true);
            button.setImage(symbol(name, &tooltip).as_deref());
        } else {
            button.setImage(None);
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            if let Some(image) = tinted_symbol(name, dark) {
                unsafe { ui.glyph.setContents(Some(&image)) };
            }
            ui.glyph.setOpacity(1.0);
            CATransaction::commit();
            ui.glyph_view.setHidden(false);
            if motion == Motion::Spin {
                let spin = animation("transform.rotation.z", 0.0, -2.0 * PI, 1.6, unsafe { kCAMediaTimingFunctionLinear }, false);
                ui.glyph.addAnimation_forKey(&spin, Some(&NSString::from_str("spin")));
            } else {
                let ease = unsafe { kCAMediaTimingFunctionEaseInEaseOut };
                ui.glyph.addAnimation_forKey(&animation("opacity", 1.0, 0.45, 0.8, ease, true), Some(&NSString::from_str("fade")));
                ui.glyph.addAnimation_forKey(&animation("transform.scale", 1.0, 0.82, 0.8, ease, true), Some(&NSString::from_str("scale")));
            }
        }
        ui.shown = Some(key);
    });
}

pub fn publisher() -> Option<Publisher> {
    Some(Box::new(|state: UiState| {
        if let Ok(mut latest) = LATEST.lock() {
            *latest = Some(state);
        }
        DispatchQueue::main().exec_async(|| {
            refresh_icon();
            crate::settings_ui::state_changed();
        });
    }))
}

pub fn run(tx: Sender<Event>, show_icon: bool) {
    let _ = EVENTS.set(tx);
    let mtm = MainThreadMarker::new().expect("menu bar must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let controller = Controller::new(mtm);
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSSquareStatusItemLength);
    item.setMenu(Some(&menu));
    item.setVisible(show_icon);

    let glyph_view = GlyphView::alloc(mtm).set_ivars(());
    let glyph_view: Retained<GlyphView> = unsafe { msg_send![super(glyph_view), initWithFrame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(ICON, ICON))] };
    glyph_view.setWantsLayer(true);
    glyph_view.setHidden(true);
    let glyph = CALayer::new();
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    glyph.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(ICON, ICON)));
    glyph.setAnchorPoint(CGPoint::new(0.5, 0.5));
    glyph.setPosition(CGPoint::new(ICON / 2.0, ICON / 2.0));
    glyph.setContentsGravity(unsafe { kCAGravityResizeAspect });
    glyph.setContentsScale(2.0);
    if let Some(layer) = glyph_view.layer() {
        layer.addSublayer(&glyph);
    }
    CATransaction::commit();
    if let Some(button) = item.button(mtm) {
        let bounds = button.bounds();
        glyph_view.setFrameOrigin(CGPoint::new((bounds.size.width - ICON) / 2.0, (bounds.size.height - ICON) / 2.0));
        button.addSubview(&glyph_view);
        button.setImage(symbol("circle", "SidePulse").as_deref());
    }
    UI.with(|ui| *ui.borrow_mut() = Some(Ui { item, _menu: menu, _controller: controller, glyph_view, glyph, shown: None }));
    refresh_icon();
    app.run();
}

fn open_url(url: &str) {
    let _ = Command::new("/usr/bin/open").arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

fn open_bundle(bundle: &str) {
    let _ = Command::new("/usr/bin/open").args(["-b", bundle]).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

fn terminal_bundle(term_program: &str) -> Option<&'static str> {
    match term_program.to_lowercase().as_str() {
        "warpterminal" | "warp" => Some("dev.warp.Warp-Stable"),
        "ghostty" => Some("com.mitchellh.ghostty"),
        "apple_terminal" | "terminal" => Some("com.apple.Terminal"),
        "iterm.app" | "iterm" | "iterm2" => Some("com.googlecode.iterm2"),
        "wezterm" => Some("com.github.wez.wezterm"),
        "kitty" => Some("net.kovidgoyal.kitty"),
        "alacritty" => Some("org.alacritty"),
        "zed" => Some("dev.zed.Zed"),
        _ => None,
    }
}

fn open_provider_app(session: &Session) {
    match (session.provider.as_str(), session.session_id.as_deref()) {
        ("codex", Some(id)) => open_url(&format!("codex://threads/{id}")),
        ("claude", _) => open_url("claude://"),
        ("cursor", _) => open_bundle("com.todesktop.230313mzl4w4u92"),
        _ => {}
    }
}

fn open_session(session: &Session, settings: &Settings) {
    if let (Some(socket), Some(pane)) = (session.herdr_socket.clone(), session.herdr_pane.clone()) {
        let terminal = session.terminal.clone();
        std::thread::spawn(move || {
            let _ = crate::herdr::focus_pane(&socket, &pane);
            match terminal.as_deref().and_then(terminal_bundle) {
                Some(bundle) => open_bundle(bundle),
                None => {
                    if let Some(app) = crate::macos::app_hosting("herdr") {
                        let _ = Command::new("/usr/bin/open").arg("-a").arg(app).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
                    }
                }
            }
        });
        return;
    }
    let kind = session.origin_kind.as_deref().unwrap_or("");
    if kind.ends_with("_vscode") {
        match (session.provider.as_str(), session.session_id.as_deref()) {
            ("claude", Some(id)) => open_url(&format!("vscode://anthropic.claude-code/open?session={id}")),
            _ => open_bundle("com.microsoft.VSCode"),
        }
        return;
    }
    if kind.ends_with("_cursor") || kind == "cursor_app" {
        open_bundle("com.todesktop.230313mzl4w4u92");
        return;
    }
    if kind.ends_with("_app") {
        open_provider_app(session);
        return;
    }
    if let Some(bundle) = session.terminal.as_deref().and_then(terminal_bundle) {
        open_bundle(bundle);
        return;
    }
    match settings.session_open.get(&session.provider).map(String::as_str) {
        Some("terminal") => resume_in_terminal(session, settings),
        _ => open_provider_app(session),
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn resume_in_terminal(session: &Session, settings: &Settings) {
    let Some(id) = session.session_id.as_deref() else { return };
    let resume = match session.provider.as_str() {
        "claude" => format!("claude --resume {}", shell_quote(id)),
        "codex" => format!("codex resume {}", shell_quote(id)),
        "opencode" => format!("opencode --session {}", shell_quote(id)),
        _ => return,
    };
    let cwd = session.cwd.clone().unwrap_or_else(|| paths::home().to_string_lossy().into_owned());
    let script = paths::state_dir().join("resume-session.command");
    let body = format!("#!/bin/zsh -l\ncd {} && {resume}\n", shell_quote(&cwd));
    if fs::write(&script, body).is_err() {
        return;
    }
    let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
    let app = match settings.terminal_app.as_str() {
        "warp" => "Warp",
        "ghostty" => "Ghostty",
        "iterm" | "iterm2" => "iTerm",
        _ => "Terminal",
    };
    let _ = Command::new("/usr/bin/open").args(["-a", app]).arg(&script).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}
