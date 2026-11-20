#![allow(non_upper_case_globals, non_camel_case_types)]

use std::ffi::{CString, c_char, c_void};
use std::path::Path;
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use crate::battery::Battery;
use crate::daemon::Event;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFAllocatorRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFTypeID = usize;
type io_object_t = u32;
type kern_return_t = i32;
type IONotificationPortRef = *mut c_void;

const kCFStringEncodingUTF8: u32 = 0x0800_0100;
const kCFNumberSInt64Type: isize = 4;
const kCFNumberDoubleType: isize = 13;
const kIOPMAssertionLevelOn: u32 = 255;
const kIOPMMessageClamshellStateChange: u32 = 0xE003_4100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFDataGetTypeID() -> CFTypeID;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> u8;
    fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CFDictionaryRef) -> io_object_t;
    fn IORegistryEntryFromPath(main_port: u32, path: *const c_char) -> io_object_t;
    fn IORegistryEntryCreateCFProperty(entry: io_object_t, key: CFStringRef, alloc: CFAllocatorRef, options: u32) -> CFTypeRef;
    fn IORegistryEntryCreateCFProperties(entry: io_object_t, props: *mut CFMutableDictionaryRef, alloc: CFAllocatorRef, options: u32) -> kern_return_t;
    fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    fn IONotificationPortCreate(main_port: u32) -> IONotificationPortRef;
    fn IONotificationPortGetRunLoopSource(port: IONotificationPortRef) -> CFRunLoopSourceRef;
    fn IOServiceAddInterestNotification(
        port: IONotificationPortRef,
        service: io_object_t,
        interest: *const c_char,
        callback: extern "C" fn(*mut c_void, io_object_t, u32, *mut c_void),
        refcon: *mut c_void,
        notification: *mut io_object_t,
    ) -> kern_return_t;
    fn IOPSNotificationCreateRunLoopSource(callback: extern "C" fn(*mut c_void), context: *mut c_void) -> CFRunLoopSourceRef;
    fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
}

struct CfString(CFStringRef);

impl CfString {
    fn new(text: &str) -> Self {
        let c = CString::new(text).unwrap_or_default();
        Self(unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) })
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cf_bool(value: CFTypeRef) -> Option<bool> {
    (!value.is_null() && unsafe { CFGetTypeID(value) == CFBooleanGetTypeID() }).then(|| unsafe { CFBooleanGetValue(value) != 0 })
}

fn cf_f64(value: CFTypeRef) -> Option<f64> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut out = 0f64;
    (unsafe { CFNumberGetValue(value, kCFNumberDoubleType, (&raw mut out).cast()) } != 0).then_some(out)
}

fn cf_i64(value: CFTypeRef) -> Option<i64> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut out = 0i64;
    (unsafe { CFNumberGetValue(value, kCFNumberSInt64Type, (&raw mut out).cast()) } != 0).then_some(out)
}

fn dict_get(dict: CFDictionaryRef, key: &str) -> CFTypeRef {
    let key = CfString::new(key);
    unsafe { CFDictionaryGetValue(dict, key.0) }
}

fn matching_service(class: &str) -> Option<io_object_t> {
    let name = CString::new(class).ok()?;
    let service = unsafe { IOServiceGetMatchingService(0, IOServiceMatching(name.as_ptr())) };
    (service != 0).then_some(service)
}

pub fn lid_closed() -> Option<bool> {
    let root = matching_service("IOPMrootDomain")?;
    let key = CfString::new("AppleClamshellState");
    let value = Owned(unsafe { IORegistryEntryCreateCFProperty(root, key.0, ptr::null(), 0) });
    unsafe { IOObjectRelease(root) };
    cf_bool(value.0)
}

pub fn read_battery() -> Option<Battery> {
    let service = matching_service("AppleSmartBattery")?;
    let mut props: CFMutableDictionaryRef = ptr::null_mut();
    let result = unsafe { IORegistryEntryCreateCFProperties(service, &raw mut props, ptr::null(), 0) };
    unsafe { IOObjectRelease(service) };
    if result != 0 || props.is_null() {
        return None;
    }
    let props = Owned(props.cast_const());
    let dict = props.0;
    let mut percent = cf_i64(dict_get(dict, "CurrentCapacity")).unwrap_or(-1);
    if !(0..=100).contains(&percent) {
        percent = cf_i64(dict_get(dict, "StateOfCharge")).unwrap_or(0);
    }
    let adapter = dict_get(dict, "AdapterDetails");
    let adapter_watts = if !adapter.is_null() && unsafe { CFGetTypeID(adapter) == CFDictionaryGetTypeID() } {
        cf_f64(dict_get(adapter, "Watts")).unwrap_or(0.0)
    } else {
        0.0
    };
    Some(Battery {
        present: cf_bool(dict_get(dict, "BatteryInstalled")).unwrap_or(true),
        percent: percent.clamp(0, 100) as u8,
        plugged: cf_bool(dict_get(dict, "ExternalConnected")).unwrap_or(false),
        charging: cf_bool(dict_get(dict, "IsCharging")).unwrap_or(false),
        charged: cf_bool(dict_get(dict, "FullyCharged")).unwrap_or(false),
        adapter_watts,
    })
}

fn product_property(key: &str) -> String {
    let path = c"IODeviceTree:/product";
    let entry = unsafe { IORegistryEntryFromPath(0, path.as_ptr()) };
    if entry == 0 {
        return String::new();
    }
    let key = CfString::new(key);
    let value = Owned(unsafe { IORegistryEntryCreateCFProperty(entry, key.0, ptr::null(), 0) });
    unsafe { IOObjectRelease(entry) };
    if value.0.is_null() || unsafe { CFGetTypeID(value.0) != CFDataGetTypeID() } {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(value.0), CFDataGetLength(value.0).max(0) as usize) };
    String::from_utf8_lossy(bytes).trim_end_matches('\0').to_lowercase()
}

pub fn full_charge_watts() -> f64 {
    let product = product_property("product-name");
    let chip = product_property("product-soc-name");
    if product.contains("macbook pro") {
        if product.contains("16-inch") {
            return 140.0;
        }
        if product.contains("14-inch") {
            return if chip.contains("pro") || chip.contains("max") { 96.0 } else { 70.0 };
        }
    }
    100.0
}

pub fn external_display_active() -> bool {
    let mut displays = [0u32; 16];
    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(16, displays.as_mut_ptr(), &raw mut count) } != 0 {
        return false;
    }
    displays[..count as usize].iter().any(|d| unsafe { CGDisplayIsBuiltin(*d) } == 0)
}

pub struct SleepAssertion(Vec<u32>);

impl SleepAssertion {
    pub fn acquire(reason: &str) -> Option<Self> {
        let name = CfString::new(reason);
        let mut ids = Vec::new();
        for kind in ["PreventUserIdleSystemSleep", "PreventSystemSleep"] {
            let kind = CfString::new(kind);
            let mut id = 0u32;
            if unsafe { IOPMAssertionCreateWithName(kind.0, kIOPMAssertionLevelOn, name.0, &raw mut id) } == 0 {
                ids.push(id);
            }
        }
        (!ids.is_empty()).then_some(Self(ids))
    }
}

impl Drop for SleepAssertion {
    fn drop(&mut self) {
        for id in &self.0 {
            unsafe { IOPMAssertionRelease(*id) };
        }
    }
}

pub fn app_hosting(process_name: &str) -> Option<String> {
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    let mut pids = vec![0i32; count as usize + 64];
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    pids.truncate(filled.max(0) as usize);
    let path_of = |pid: i32| {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        (len > 0).then(|| String::from_utf8_lossy(&buffer[..len as usize]).into_owned())
    };
    let parent_of = |pid: i32| {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (got == size).then_some(info.pbi_ppid as i32)
    };
    for pid in pids {
        let Some(path) = path_of(pid) else { continue };
        if !path.ends_with(&format!("/{process_name}")) {
            continue;
        }
        let mut current = parent_of(pid);
        for _ in 0..12 {
            let Some(parent) = current.filter(|p| *p > 1) else { break };
            if let Some(app) = path_of(parent).and_then(|p| p.find(".app/").map(|i| p[..i + 4].to_string())) {
                return Some(app);
            }
            current = parent_of(parent);
        }
    }
    None
}

pub fn sleep_helper_installed() -> bool {
    Path::new("/etc/sudoers.d/sidepulse-disablesleep").exists()
}

pub fn set_system_sleep_disabled(disabled: bool) -> Result<(), String> {
    let status = Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/pmset", "-a", "disablesleep", if disabled { "1" } else { "0" }])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| format!("pmset disablesleep exited with {status}"))
}

pub fn display_sleep_now() {
    let _ = Command::new("/usr/bin/pmset").arg("displaysleepnow").stdout(Stdio::null()).stderr(Stdio::null()).status();
}

static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

extern "C" fn clamshell_changed(_refcon: *mut c_void, _service: io_object_t, message: u32, argument: *mut c_void) {
    if message == kIOPMMessageClamshellStateChange && let Some(tx) = EVENTS.get() {
        let closed = (argument as usize) & 1 == 1;
        let _ = tx.send(Event::Lid(closed));
    }
}

extern "C" fn power_changed(_context: *mut c_void) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(Event::Power);
    }
}

pub fn register_notifications(tx: Sender<Event>) {
    let _ = EVENTS.set(tx);
    unsafe {
        let run_loop = CFRunLoopGetMain();
        let power = IOPSNotificationCreateRunLoopSource(power_changed, ptr::null_mut());
        if !power.is_null() {
            CFRunLoopAddSource(run_loop, power, kCFRunLoopCommonModes);
        }
        let Some(root) = matching_service("IOPMrootDomain") else { return };
        let port = IONotificationPortCreate(0);
        if port.is_null() {
            return;
        }
        CFRunLoopAddSource(run_loop, IONotificationPortGetRunLoopSource(port), kCFRunLoopCommonModes);
        let mut notification: io_object_t = 0;
        let interest = c"IOGeneralInterest";
        IOServiceAddInterestNotification(port, root, interest.as_ptr(), clamshell_changed, ptr::null_mut(), &raw mut notification);
    }
}

pub fn run_main_loop() {
    unsafe { CFRunLoopRun() };
}
