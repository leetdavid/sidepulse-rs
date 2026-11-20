use std::cell::RefCell;
use std::fs;
use std::process::{Command, Stdio};
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBezierPath, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSFontWeightRegular, NSFontWeightSemibold, NSMenuItem, NSPopUpButton, NSScrollView, NSTabView, NSTabViewItem, NSTextDelegate,
    NSTextField, NSTextView, NSTextViewDelegate, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSString, NSTimer};
use serde_json::{Value, json};

use crate::daemon::Event;
use crate::ledsim::LedEngine;
use crate::menubar;
use crate::paths;
use crate::protocol::Request;
use crate::settings::{self, BUILTIN_ANIMATIONS, Settings, WORKING_MODES};

const STATES: &[(&str, &str)] = &[
    ("working", "Working"),
    ("waiting_for_input", "Waiting for Input"),
    ("blocked_error", "Error"),
    ("completed", "Done"),
    ("idle_ready", "Idle"),
    ("lid_open", "Lid Opened"),
    ("lid_closed", "Lid Closed"),
];
const NEW_ANIMATION: &str = "new";
const TIMEOUTS: &[(f64, &str)] = &[(900.0, "15 minutes"), (1800.0, "30 minutes"), (3600.0, "1 hour"), (7200.0, "2 hours"), (14400.0, "4 hours"), (28800.0, "8 hours")];
const POLICIES: &[(&str, &str)] = &[("never", "Never"), ("agents", "When agents work"), ("always", "Always")];
const BATTERY_FLOORS: &[(f64, &str)] = &[(0.0, "No minimum"), (10.0, "10%"), (20.0, "20%"), (30.0, "30%"), (50.0, "50%")];
const PREVIEW_SECONDS: &[(f64, &str)] = &[(3.0, "3 seconds"), (5.0, "5 seconds"), (7.0, "7 seconds"), (10.0, "10 seconds"), (15.0, "15 seconds")];
const TERMINALS: &[(&str, &str)] = &[("terminal", "Terminal"), ("iterm", "iTerm"), ("warp", "Warp"), ("ghostty", "Ghostty")];

const TAG_TIMEOUT: isize = 1;
const TAG_POLICY: isize = 2;
const TAG_BATTERY_FLOOR: isize = 3;
const TAG_BATTERY_PREVIEW: isize = 4;
const TAG_PREVIEW_SECONDS: isize = 5;
const TAG_TERMINAL: isize = 6;
const TAG_MENU_ICON: isize = 7;

thread_local! {
    static WINDOW: RefCell<Option<SettingsWindow>> = const { RefCell::new(None) };
}

const ONE_SHOT_LOOP_MS: u32 = 2500;

struct Preview {
    view: Retained<LedView>,
    engine: Option<LedEngine>,
    program: String,
    seed: Option<String>,
    offset: u32,
    restart_at: Option<u32>,
}

impl Preview {
    fn new(mtm: MainThreadMarker, frame: CGRect, led_count: u8) -> Self {
        let view = LedView::new(mtm, frame);
        Self { view, engine: LedEngine::new(led_count).ok(), program: String::new(), seed: None, offset: 0, restart_at: None }
    }

    fn load(&mut self, program: &str, now: u32) -> Result<(), String> {
        let Some(engine) = self.engine.as_mut() else { return Err("LED engine unavailable".into()) };
        let mut time = now + self.offset;
        engine.reset_at(time);
        if let Some(seed) = &self.seed
            && engine.load(seed, time).is_ok()
        {
            engine.step(time + 1000);
            self.offset += 1000;
            time += 1000;
        }
        let result = engine.load(program, time).map_err(|e| e.to_string());
        let repeats = program.lines().any(|l| l.trim_start().to_lowercase().starts_with("repeat"));
        self.program = program.to_string();
        self.restart_at = (result.is_ok() && !repeats).then_some(now + ONE_SHOT_LOOP_MS);
        result
    }

    fn tick(&mut self, now: u32) {
        if self.restart_at.is_some_and(|at| now >= at) {
            let program = std::mem::take(&mut self.program);
            let _ = self.load(&program, now);
        }
        let Some(engine) = self.engine.as_mut() else { return };
        let colors = engine.step(now + self.offset);
        let mut current = self.view.ivars().colors.borrow_mut();
        if *current != colors {
            *current = colors;
            drop(current);
            self.view.setNeedsDisplay(true);
        }
    }
}

struct AnimationRow {
    key: &'static str,
    popup: Retained<NSPopUpButton>,
    choices: Vec<String>,
    preview: Preview,
}

struct Editor {
    window: Retained<NSWindow>,
    name: Retained<NSTextField>,
    text: Retained<NSTextView>,
    status: Retained<NSTextField>,
    preview: Preview,
    editing: Option<String>,
    for_state: Option<&'static str>,
    valid: bool,
}

struct SettingsWindow {
    window: Retained<NSWindow>,
    _controller: Retained<Controller>,
    rows: Vec<AnimationRow>,
    agent_labels: Vec<(Retained<NSTextField>, &'static str)>,
    editor: Option<Editor>,
    timer: Option<Retained<NSTimer>>,
    started: Instant,
    led_count: u8,
}

impl SettingsWindow {
    fn now(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
}

#[derive(Default)]
struct LedIvars {
    colors: RefCell<Vec<[u8; 3]>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SidePulseLedView"]
    #[ivars = LedIvars]
    struct LedView;

    impl LedView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: CGRect) {
            let bounds = self.bounds();
            NSColor::colorWithCalibratedWhite_alpha(0.1, 1.0).setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, 6.0, 6.0).fill();
            let colors = self.ivars().colors.borrow();
            let count = colors.len().max(1) as f64;
            let slot = (bounds.size.width - 8.0) / count;
            let diameter = (bounds.size.height - 10.0).min(slot - 4.0).max(4.0);
            for (index, color) in colors.iter().enumerate() {
                let x = bounds.origin.x + 4.0 + slot * index as f64 + (slot - diameter) / 2.0;
                let y = bounds.origin.y + (bounds.size.height - diameter) / 2.0;
                let rect = CGRect::new(CGPoint::new(x, y), CGSize::new(diameter, diameter));
                NSColor::colorWithCalibratedWhite_alpha(0.22, 1.0).setFill();
                NSBezierPath::bezierPathWithOvalInRect(rect).fill();
                let alpha = f64::from(*color.iter().max().unwrap_or(&0)) / 255.0;
                if alpha > 0.0 {
                    let scale = |c: u8| (f64::from(c) / 255.0 / alpha).min(1.0);
                    NSColor::colorWithSRGBRed_green_blue_alpha(scale(color[0]), scale(color[1]), scale(color[2]), alpha).setFill();
                    NSBezierPath::bezierPathWithOvalInRect(rect).fill();
                }
            }
        }
    }
);

impl LedView {
    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(LedIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SidePulseSettingsController"]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, notification: &NSNotification) {
            let closing = notification.object();
            let main = WINDOW.with(|w| w.try_borrow().ok().and_then(|w| w.as_ref().map(|w| w.window.clone())));
            let is_main = closing.as_deref().zip(main.as_deref()).is_some_and(|(a, b)| std::ptr::eq(a, b as &NSWindow as *const NSWindow as *const AnyObject));
            dispatch2::DispatchQueue::main().exec_async(move || {
                WINDOW.with(|w| {
                    let Ok(mut slot) = w.try_borrow_mut() else { return };
                    if is_main {
                        if let Some(state) = slot.take() {
                            if let Some(timer) = &state.timer {
                                timer.invalidate();
                            }
                            if let Some(editor) = &state.editor {
                                editor.window.orderOut(None);
                            }
                        }
                    } else if let Some(state) = slot.as_mut() {
                        state.editor = None;
                    }
                });
            });
        }
    }

    unsafe impl NSTextDelegate for Controller {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, _notification: &NSNotification) {
            with_window(|state| {
                let now = state.now();
                if let Some(editor) = state.editor.as_mut() {
                    reparse(editor, now);
                }
            });
        }
    }

    unsafe impl NSTextViewDelegate for Controller {}

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            with_window(|state| {
                let now = state.now();
                for row in &mut state.rows {
                    row.preview.tick(now);
                }
                if let Some(editor) = state.editor.as_mut() {
                    editor.preview.tick(now);
                }
            });
        }

        #[unsafe(method(animationChanged:))]
        fn animation_changed(&self, sender: &NSPopUpButton) {
            let index = sender.tag() as usize;
            let choice = with_window(|state| {
                let row = state.rows.get(index)?;
                Some((row.key, row.choices.get(sender.indexOfSelectedItem() as usize)?.clone()))
            })
            .flatten();
            let Some((key, id)) = choice else { return };
            if id == NEW_ANIMATION {
                refresh_rows();
                open_editor(self, None, Some(key), "My Animation", "off\n#ff00ff 1s pulse\nrepeat");
                return;
            }
            assign_animation(key, &id);
            refresh_rows();
        }

        #[unsafe(method(showAnimation:))]
        fn show_animation(&self, sender: &NSButton) {
            let key = STATES[sender.tag() as usize].0;
            let settings = Settings::load();
            let led_count = with_window(|s| s.led_count).unwrap_or(8);
            let hold = match key {
                "lid_open" => settings.lid_open_seconds + 0.5,
                "lid_closed" => settings.lid_closed_seconds + 0.5,
                _ => 4.0,
            };
            show_on_device(&settings.program_for_key(key, led_count), hold);
        }

        #[unsafe(method(editAnimation:))]
        fn edit_animation(&self, sender: &NSButton) {
            let key = STATES[sender.tag() as usize].0;
            let settings = Settings::load();
            let led_count = with_window(|s| s.led_count).unwrap_or(8);
            let id = settings.animation_id(key);
            let program = settings.program_for_key(key, led_count);
            match settings.custom.iter().find(|c| c.id == id) {
                Some(custom) => open_editor(self, Some(id.clone()), Some(key), &custom.name, &program),
                None => {
                    let name = BUILTIN_ANIMATIONS.iter().find(|(b, _)| *b == id).map_or("Animation", |(_, n)| n);
                    open_editor(self, None, Some(key), &format!("{name} Copy"), &program);
                }
            }
        }

        #[unsafe(method(editorSave:))]
        fn editor_save(&self, _sender: &NSButton) {
            let saved = with_window(|state| {
                let editor = state.editor.as_ref()?;
                if !editor.valid {
                    return None;
                }
                let name = editor.name.stringValue().to_string().trim().to_string();
                let program = editor.text.string().to_string().trim().replace("\r\n", "\n");
                Some((name, program, editor.editing.clone(), editor.for_state, editor.window.clone()))
            })
            .flatten();
            let Some((name, program, editing, for_state, window)) = saved else { return };
            let name = if name.is_empty() { "Custom Animation".to_string() } else { name };
            save_custom(&name, &program, editing.as_deref(), for_state);
            window.close();
            refresh_rows();
        }

        #[unsafe(method(editorDelete:))]
        fn editor_delete(&self, _sender: &NSButton) {
            let target = with_window(|state| state.editor.as_ref().map(|e| (e.editing.clone(), e.window.clone()))).flatten();
            let Some((Some(id), window)) = target else { return };
            delete_custom(&id);
            window.close();
            refresh_rows();
        }

        #[unsafe(method(editorCancel:))]
        fn editor_cancel(&self, _sender: &NSButton) {
            if let Some(window) = with_window(|state| state.editor.as_ref().map(|e| e.window.clone())).flatten() {
                window.close();
            }
        }

        #[unsafe(method(editorShow:))]
        fn editor_show(&self, _sender: &NSButton) {
            let program = with_window(|state| state.editor.as_ref().filter(|e| e.valid).map(|e| e.text.string().to_string())).flatten();
            if let Some(program) = program {
                show_on_device(program.trim(), 6.0);
            }
        }

        #[unsafe(method(generalChanged:))]
        fn general_changed(&self, sender: &NSObject) {
            let control: &NSView = unsafe { &*(sender as *const NSObject as *const NSView) };
            let tag: isize = unsafe { msg_send![control, tag] };
            let index = || -> usize { unsafe { msg_send![control, indexOfSelectedItem] } };
            let checked = || -> bool {
                let state: isize = unsafe { msg_send![control, state] };
                state == NSControlStateValueOn
            };
            settings::update_file(|root| match tag {
                TAG_TIMEOUT => set_path(root, &["agent_list", "session_timeout_seconds"], json!(TIMEOUTS[index()].0)),
                TAG_POLICY => set_path(root, &["sleep_prevention_policy"], json!(POLICIES[index()].0)),
                TAG_BATTERY_FLOOR => set_path(root, &["sleep_prevention", "min_battery_percent"], json!(BATTERY_FLOORS[index()].0)),
                TAG_BATTERY_PREVIEW => set_path(root, &["battery_monitoring", "show_on_power_change"], json!(checked())),
                TAG_PREVIEW_SECONDS => set_path(root, &["battery_monitoring", "power_change_preview_seconds"], json!(PREVIEW_SECONDS[index()].0)),
                TAG_TERMINAL => set_path(root, &["session_terminal", "app"], json!(TERMINALS[index()].0)),
                TAG_MENU_ICON => set_path(root, &["show_menu_bar_icon"], json!(checked())),
                _ => {}
            });
            if tag == TAG_TIMEOUT {
                remove_path(&["agent_list", "idle_timeout_seconds"]);
            }
            if tag == TAG_MENU_ICON {
                menubar::set_icon_visible(checked());
            }
            menubar::send(Event::SettingsChanged);
        }

        #[unsafe(method(openSettingsFile:))]
        fn open_settings_file(&self, _sender: &NSButton) {
            open_path(&paths::settings_file().to_string_lossy(), true);
        }

        #[unsafe(method(openAnimationsFolder:))]
        fn open_animations_folder(&self, _sender: &NSButton) {
            let dir = settings::animations_dir();
            let _ = fs::create_dir_all(&dir);
            open_path(&dir.to_string_lossy(), false);
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: &NSButton) {
            open_path(&paths::log_file().to_string_lossy(), true);
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }

    fn target(&self) -> &AnyObject {
        let object: &NSObject = self;
        object
    }
}

fn with_window<R>(f: impl FnOnce(&mut SettingsWindow) -> R) -> Option<R> {
    WINDOW.with(|w| w.try_borrow_mut().ok().and_then(|mut w| w.as_mut().map(f)))
}

fn set_path(root: &mut serde_json::Map<String, Value>, path: &[&str], value: Value) {
    let (last, parents) = path.split_last().expect("non-empty path");
    let mut current = root;
    for key in parents {
        let entry = current.entry(*key).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        current = entry.as_object_mut().expect("object");
    }
    current.insert((*last).to_string(), value);
}

fn remove_path(path: &[&str]) {
    settings::update_file(|root| {
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut current = root;
        for key in parents {
            let Some(Value::Object(next)) = current.get_mut(*key) else { return };
            current = next;
        }
        current.remove(*last);
    });
}

fn state_keys(key: &str) -> Vec<&str> {
    if WORKING_MODES.contains(&key) { WORKING_MODES.to_vec() } else { vec![key] }
}

fn assign_animation(key: &str, id: &str) {
    settings::update_file(|root| {
        for state in state_keys(key) {
            set_path(root, &["agent_animations", state], json!({"style": id, "custom_program": ""}));
        }
    });
    menubar::send(Event::SettingsChanged);
}

fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() { "animation".into() } else { slug }
}

fn save_custom(name: &str, program: &str, editing: Option<&str>, for_state: Option<&str>) {
    let settings = Settings::load();
    let existing = editing.and_then(|id| settings.custom.iter().find(|c| c.id == id));
    let (id, file) = match existing {
        Some(custom) => (custom.id.clone(), custom.file.clone()),
        None => {
            let base = slug(name);
            let mut candidate = base.clone();
            let mut n = 2;
            while settings.custom.iter().any(|c| c.id == format!("custom:{candidate}")) || settings::animations_dir().join(format!("{candidate}.LED")).exists() {
                candidate = format!("{base}-{n}");
                n += 1;
            }
            (format!("custom:{candidate}"), format!("{candidate}.LED"))
        }
    };
    let dir = settings::animations_dir();
    if fs::create_dir_all(&dir).is_err() || fs::write(dir.join(&file), format!("{program}\n")).is_err() {
        return;
    }
    settings::update_file(|root| {
        set_path(root, &["custom_agent_animations", &id], json!({"name": name, "file": file}));
        if let Some(key) = for_state {
            for state in state_keys(key) {
                set_path(root, &["agent_animations", state], json!({"style": id, "custom_program": ""}));
            }
        }
    });
    menubar::send(Event::SettingsChanged);
}

fn delete_custom(id: &str) {
    let settings = Settings::load();
    if let Some(custom) = settings.custom.iter().find(|c| c.id == id) {
        let _ = fs::remove_file(settings::animations_dir().join(&custom.file));
    }
    settings::update_file(|root| {
        if let Some(Value::Object(custom)) = root.get_mut("custom_agent_animations") {
            custom.remove(id);
        }
        if let Some(Value::Object(animations)) = root.get_mut("agent_animations") {
            animations.retain(|_, v| v.get("style").and_then(Value::as_str) != Some(id));
        }
    });
    menubar::send(Event::SettingsChanged);
}

fn show_on_device(program: &str, hold_seconds: f64) {
    menubar::send(Event::Request(Request::WriteLed { program: program.to_string(), target: None, hold_seconds }, None));
}

fn open_path(path: &str, text: bool) {
    let mut command = Command::new("/usr/bin/open");
    if text {
        command.arg("-t");
    }
    let _ = command.arg(path).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn label(mtm: MainThreadMarker, text: &str, frame: CGRect) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    field
}

fn heading(mtm: MainThreadMarker, text: &str, frame: CGRect) -> Retained<NSTextField> {
    let field = label(mtm, text, frame);
    field.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold })));
    field
}

fn button(mtm: MainThreadMarker, title: &str, frame: CGRect, target: &AnyObject, action: Sel, tag: isize) -> Retained<NSButton> {
    let button = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), Some(target), Some(action), mtm) };
    button.setFrame(frame);
    button.setTag(tag);
    button
}

fn popup(mtm: MainThreadMarker, frame: CGRect, items: &[&str], selected: usize, target: &AnyObject, action: Sel, tag: isize) -> Retained<NSPopUpButton> {
    let popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), frame, false);
    for item in items {
        popup.addItemWithTitle(&NSString::from_str(item));
    }
    popup.selectItemAtIndex(selected as isize);
    unsafe {
        popup.setTarget(Some(target));
        popup.setAction(Some(action));
    }
    popup.setTag(tag);
    popup
}

fn nearest<T: Copy + PartialOrd + Into<f64>>(options: &[(T, &str)], value: f64) -> usize {
    options
        .iter()
        .enumerate()
        .min_by(|a, b| {
            let da = (a.1.0.into() - value).abs();
            let db = (b.1.0.into() - value).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(0, |(i, _)| i)
}

fn fill_animation_popup(popup: &NSPopUpButton, settings: &Settings, key: &str) -> Vec<String> {
    popup.removeAllItems();
    let mut choices = Vec::new();
    let menu = popup.menu();
    let separator = |choices: &mut Vec<String>| {
        if let Some(menu) = &menu {
            menu.addItem(&NSMenuItem::separatorItem(popup.mtm()));
            choices.push(String::new());
        }
    };
    for (id, name) in BUILTIN_ANIMATIONS {
        popup.addItemWithTitle(&NSString::from_str(name));
        choices.push((*id).to_string());
    }
    if !settings.custom.is_empty() {
        separator(&mut choices);
        for custom in &settings.custom {
            popup.addItemWithTitle(&NSString::from_str(&custom.name));
            choices.push(custom.id.clone());
        }
    }
    separator(&mut choices);
    popup.addItemWithTitle(&NSString::from_str("New Animation…"));
    choices.push(NEW_ANIMATION.into());
    let current = settings.animation_id(key);
    if let Some(index) = choices.iter().position(|c| *c == current) {
        popup.selectItemAtIndex(index as isize);
    }
    choices
}

fn refresh_rows() {
    let settings = Settings::load();
    with_window(|state| {
        let now = state.now();
        let led_count = state.led_count;
        for row in &mut state.rows {
            row.choices = fill_animation_popup(&row.popup, &settings, row.key);
            let _ = row.preview.load(&settings.program_for_key(row.key, led_count), now);
        }
        for (field, name) in &state.agent_labels {
            let connected = crate::install::integration_status().into_iter().find(|(n, _)| n == name).is_some_and(|(_, c)| c);
            field.setStringValue(&NSString::from_str(if connected { "● Connected" } else { "○ Not connected" }));
        }
    });
}

fn reparse(editor: &mut Editor, now: u32) {
    let program = editor.text.string().to_string();
    let program = program.trim();
    let lines = program.lines().count();
    let result = crate::led::validate(program).and_then(|()| editor.preview.load(program, now));
    editor.valid = result.is_ok();
    let message = match result {
        Ok(()) => format!("✓ Valid · {} of 512 bytes · {lines} of 20 lines", program.len()),
        Err(error) => format!("✗ {error}"),
    };
    editor.status.setStringValue(&NSString::from_str(&message));
}

fn new_window(mtm: MainThreadMarker, title: &str, width: f64, height: f64) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect(0.0, 0.0, width, height), style, NSBackingStoreType::Buffered, false)
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(title));
    window.center();
    window
}

fn open_editor(controller: &Controller, editing: Option<String>, for_state: Option<&'static str>, name: &str, program: &str) {
    let mtm = controller.mtm();
    let target = controller.target();
    let led_count = with_window(|s| s.led_count).unwrap_or(8);
    if let Some(existing) = with_window(|s| s.editor.take().map(|e| e.window)).flatten() {
        existing.orderOut(None);
    }
    let window = new_window(mtm, "Edit Animation", 560.0, 440.0);
    let Some(content) = window.contentView() else { return };

    content.addSubview(&label(mtm, "Name", rect(20.0, 400.0, 60.0, 20.0)));
    let name_field = NSTextField::textFieldWithString(&NSString::from_str(name), mtm);
    name_field.setFrame(rect(80.0, 398.0, 460.0, 24.0));
    content.addSubview(&name_field);

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(20.0, 150.0, 520.0, 236.0));
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(objc2_app_kit::NSBorderType::BezelBorder);
    let text = NSTextView::initWithFrame(NSTextView::alloc(mtm), rect(0.0, 0.0, 516.0, 236.0));
    text.setRichText(false);
    text.setAutomaticQuoteSubstitutionEnabled(false);
    text.setAutomaticDashSubstitutionEnabled(false);
    text.setAutomaticTextReplacementEnabled(false);
    text.setAutomaticSpellingCorrectionEnabled(false);
    text.setContinuousSpellCheckingEnabled(false);
    text.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(13.0, unsafe { NSFontWeightRegular })));
    text.setString(&NSString::from_str(program));
    text.setDelegate(Some(ProtocolObject::from_ref(controller)));
    scroll.setDocumentView(Some(&text));
    content.addSubview(&scroll);

    let preview = Preview::new(mtm, rect(20.0, 96.0, 520.0, 40.0), led_count);
    content.addSubview(&preview.view);
    let status = label(mtm, "", rect(20.0, 66.0, 520.0, 20.0));
    content.addSubview(&status);

    content.addSubview(&button(mtm, "Show on Device", rect(16.0, 16.0, 140.0, 32.0), target, sel!(editorShow:), 0));
    if editing.is_some() {
        content.addSubview(&button(mtm, "Delete", rect(160.0, 16.0, 90.0, 32.0), target, sel!(editorDelete:), 0));
    }
    content.addSubview(&button(mtm, "Cancel", rect(350.0, 16.0, 90.0, 32.0), target, sel!(editorCancel:), 0));
    let save = button(mtm, "Save", rect(446.0, 16.0, 98.0, 32.0), target, sel!(editorSave:), 0);
    save.setKeyEquivalent(&NSString::from_str("\r"));
    content.addSubview(&save);

    window.setDelegate(Some(ProtocolObject::from_ref(controller)));
    let mut editor = Editor { window: window.clone(), name: name_field, text, status, preview, editing, for_state, valid: false };
    with_window(|state| {
        let now = state.now();
        reparse(&mut editor, now);
        state.editor = Some(editor);
    });
    bring_to_front(&window, &NSApplication::sharedApplication(mtm));
}

fn build_animations_tab(mtm: MainThreadMarker, controller: &Controller, settings: &Settings, led_count: u8) -> (Retained<NSView>, Vec<AnimationRow>) {
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 700.0, 470.0));
    let target = controller.target();
    view.addSubview(&label(mtm, "Pick what the LEDs play for each state. Previews run the device's own LED engine.", rect(20.0, 430.0, 660.0, 20.0)));
    let mut rows = Vec::new();
    for (index, (key, title)) in STATES.iter().enumerate() {
        let y = 384.0 - index as f64 * 46.0;
        view.addSubview(&label(mtm, title, rect(20.0, y + 4.0, 130.0, 20.0)));
        let popup = popup(mtm, rect(150.0, y, 200.0, 28.0), &[], 0, target, sel!(animationChanged:), index as isize);
        let choices = fill_animation_popup(&popup, settings, key);
        view.addSubview(&popup);
        let mut preview = Preview::new(mtm, rect(362.0, y + 1.0, 170.0, 26.0), led_count);
        if *key == "lid_closed" {
            preview.seed = Some(settings.program_for_key("idle_ready", led_count));
        }
        let _ = preview.load(&settings.program_for_key(key, led_count), 0);
        view.addSubview(&preview.view);
        view.addSubview(&button(mtm, "Show", rect(540.0, y - 2.0, 66.0, 32.0), target, sel!(showAnimation:), index as isize));
        view.addSubview(&button(mtm, "Edit…", rect(608.0, y - 2.0, 72.0, 32.0), target, sel!(editAnimation:), index as isize));
        rows.push(AnimationRow { key, popup, choices, preview });
    }
    view.addSubview(&button(mtm, "Open Animations Folder", rect(16.0, 16.0, 200.0, 32.0), target, sel!(openAnimationsFolder:), 0));
    (view, rows)
}

fn build_general_tab(mtm: MainThreadMarker, controller: &Controller, settings: &Settings) -> Retained<NSView> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 700.0, 470.0));
    let target = controller.target();
    let action = sel!(generalChanged:);
    fn names(options: &[(f64, &'static str)]) -> Vec<&'static str> {
        options.iter().map(|(_, n)| *n).collect()
    }
    let mut y = 410.0;
    let mut row = |title: &str, control: &NSView| {
        view.addSubview(&label(mtm, title, rect(20.0, y + 4.0, 270.0, 20.0)));
        control.setFrameOrigin(CGPoint::new(300.0, y));
        view.addSubview(control);
        y -= 44.0;
    };

    let timeout = popup(mtm, rect(0.0, 0.0, 200.0, 28.0), &names(TIMEOUTS), nearest(TIMEOUTS, settings.session_timeout_seconds), target, action, TAG_TIMEOUT);
    row("Forget sessions after", &timeout);
    let policy_index = POLICIES.iter().position(|(k, _)| {
        *k == match settings.sleep_policy {
            settings::SleepPolicy::Never => "never",
            settings::SleepPolicy::Agents => "agents",
            settings::SleepPolicy::Always => "always",
        }
    });
    let policy = popup(mtm, rect(0.0, 0.0, 200.0, 28.0), &POLICIES.iter().map(|(_, n)| *n).collect::<Vec<_>>(), policy_index.unwrap_or(1), target, action, TAG_POLICY);
    row("Keep Mac awake with lid closed", &policy);
    let floor = popup(mtm, rect(0.0, 0.0, 200.0, 28.0), &names(BATTERY_FLOORS), nearest(BATTERY_FLOORS, settings.sleep_min_battery_percent), target, action, TAG_BATTERY_FLOOR);
    row("Allow sleep below battery level", &floor);
    let battery = unsafe {
        NSButton::checkboxWithTitle_target_action(&NSString::from_str("Show battery level on plug/unplug"), Some(target), Some(action), mtm)
    };
    battery.setFrame(rect(0.0, 0.0, 300.0, 24.0));
    battery.setTag(TAG_BATTERY_PREVIEW);
    battery.setState(if settings.show_on_power_change { NSControlStateValueOn } else { NSControlStateValueOff });
    row("Battery", &battery);
    let seconds = popup(mtm, rect(0.0, 0.0, 200.0, 28.0), &names(PREVIEW_SECONDS), nearest(PREVIEW_SECONDS, settings.power_change_preview_seconds), target, action, TAG_PREVIEW_SECONDS);
    row("Show battery level for", &seconds);
    let terminal_index = TERMINALS.iter().position(|(k, _)| *k == settings.terminal_app).unwrap_or(0);
    let terminal = popup(mtm, rect(0.0, 0.0, 200.0, 28.0), &TERMINALS.iter().map(|(_, n)| *n).collect::<Vec<_>>(), terminal_index, target, action, TAG_TERMINAL);
    row("Resume sessions in (⌥-click)", &terminal);
    let icon = unsafe { NSButton::checkboxWithTitle_target_action(&NSString::from_str("Show menu bar icon"), Some(target), Some(action), mtm) };
    icon.setFrame(rect(0.0, 0.0, 300.0, 24.0));
    icon.setTag(TAG_MENU_ICON);
    icon.setState(if settings.show_menu_bar_icon { NSControlStateValueOn } else { NSControlStateValueOff });
    row("Menu bar", &icon);

    view.addSubview(&button(mtm, "Open settings.json", rect(16.0, 16.0, 170.0, 32.0), target, sel!(openSettingsFile:), 0));
    view.addSubview(&button(mtm, "Open Log", rect(190.0, 16.0, 110.0, 32.0), target, sel!(openLog:), 0));
    view
}

fn build_agents_tab(mtm: MainThreadMarker) -> (Retained<NSView>, Vec<(Retained<NSTextField>, &'static str)>) {
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 700.0, 470.0));
    view.addSubview(&heading(mtm, "Agent integrations", rect(20.0, 426.0, 400.0, 22.0)));
    let mut labels = Vec::new();
    for (index, (name, connected)) in crate::install::integration_status().into_iter().enumerate() {
        let y = 386.0 - index as f64 * 34.0;
        view.addSubview(&label(mtm, name, rect(30.0, y, 200.0, 20.0)));
        let status = label(mtm, if connected { "● Connected" } else { "○ Not connected" }, rect(240.0, y, 200.0, 20.0));
        view.addSubview(&status);
        labels.push((status, name));
    }
    let note = NSTextField::wrappingLabelWithString(
        &NSString::from_str("Run `sidepulse-rs install` in a terminal to connect agents that were installed after SidePulse. It updates the agent configs, links the Herdr plugin and installs the OpenCode plugin."),
        mtm,
    );
    note.setFrame(rect(30.0, 90.0, 640.0, 60.0));
    view.addSubview(&note);
    (view, labels)
}

pub fn open(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    if let Some(window) = with_window(|s| s.window.clone()) {
        bring_to_front(&window, &app);
        return;
    }
    let settings = Settings::load();
    let led_count = menubar::latest().and_then(|s| s.devices.first().map(|d| d.led_count)).filter(|c| *c > 0).unwrap_or(8);
    let controller = Controller::new(mtm);
    let window = new_window(mtm, "SidePulse Settings", 720.0, 540.0);
    let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), rect(10.0, 10.0, 700.0, 520.0));

    let (animations, rows) = build_animations_tab(mtm, &controller, &settings, led_count);
    let general = build_general_tab(mtm, &controller, &settings);
    let (agents, agent_labels) = build_agents_tab(mtm);
    for (title, view) in [("Animations", &animations), ("General", &general), ("Agents", &agents)] {
        let item = NSTabViewItem::new();
        item.setLabel(&NSString::from_str(title));
        item.setView(Some(view));
        tabs.addTabViewItem(&item);
    }
    if let Some(content) = window.contentView() {
        content.addSubview(&tabs);
    }
    window.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(1.0 / 24.0, controller.target(), sel!(tick:), None, true)
    };
    WINDOW.with(|w| {
        *w.borrow_mut() = Some(SettingsWindow {
            window: window.clone(),
            _controller: controller,
            rows,
            agent_labels,
            editor: None,
            timer: Some(timer),
            started: Instant::now(),
            led_count,
        });
    });
    bring_to_front(&window, &app);
}

fn bring_to_front(window: &NSWindow, app: &NSApplication) {
    app.activate();
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
}

pub fn state_changed() {}

pub fn request_open() {
    dispatch2::DispatchQueue::main().exec_async(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            open(mtm);
        }
    });
}
