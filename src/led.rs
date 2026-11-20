use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub const LED_FILE: &str = "LEDS.LED";
pub const STATUS_FILE: &str = "STATUS.TXT";
pub const MAX_BYTES: usize = 512;
pub const MAX_LINES: usize = 20;

pub fn decode_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            _ => {
                out.push('\\');
                continue;
            }
        }
        chars.next();
    }
    out
}

pub fn validate(program: &str) -> Result<(), String> {
    if program.is_empty() {
        return Err("LED program is empty.".into());
    }
    if program.len() > MAX_BYTES {
        return Err(format!("LED program is {} bytes; max is {MAX_BYTES}.", program.len()));
    }
    let lines = program.lines().count().max(1);
    if lines > MAX_LINES {
        return Err(format!("LED program has {lines} lines; max is {MAX_LINES}."));
    }
    Ok(())
}

pub fn apply_brightness(program: &str, brightness: u8) -> String {
    let mut found = false;
    let lines: Vec<String> = program
        .lines()
        .map(|line| match authored_brightness(line) {
            Some(authored) => {
                found = true;
                let scaled = (f64::from(authored) * f64::from(brightness) / 255.0).round() as u32;
                format!("brightness {scaled}")
            }
            None => line.to_string(),
        })
        .collect();
    let joined = lines.join("\n");
    if found || brightness == 255 {
        joined
    } else {
        format!("brightness {brightness}\n{joined}")
    }
}

fn authored_brightness(line: &str) -> Option<u8> {
    let mut parts = line.split_whitespace();
    let keyword = parts.next()?;
    let value = parts.next()?;
    if parts.next().is_some() || !keyword.eq_ignore_ascii_case("brightness") {
        return None;
    }
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(value.parse::<u64>().ok()?.min(255) as u8)
}

macro_rules! builtins {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../resources/animations/", $name, ".LED")))),*]
    };
}

const BUILTIN_FILES: &[(&str, &str)] = builtins!(
    "amber-pulse",
    "cyan-complete",
    "cyan-roll-2",
    "cyan-roll-8",
    "ember-attention",
    "ember-complete",
    "ember-idle-2",
    "ember-idle-8",
    "ember-lid-open-2",
    "ember-lid-open-8",
    "ember-tide-2",
    "ember-tide-8",
    "idle-pulse-2",
    "idle-pulse-8",
    "immediate-off",
    "kitt-2",
    "kitt-8",
    "kitt-red-2",
    "kitt-red-8",
    "lid-closed-2",
    "lid-closed-8",
    "lid-open-2",
    "lid-open-8",
    "night-rider-2",
    "night-rider-8",
    "off",
    "purple-attention",
    "purple-complete",
    "purple-idle-2",
    "purple-idle-8",
    "purple-lid-open-2",
    "purple-lid-open-8",
    "purple-tide-2",
    "purple-tide-8",
    "solid-green",
);

pub fn builtin(id: &str, led_count: u8) -> Option<String> {
    let suffix = if led_count == 2 { "-2" } else { "-8" };
    let sized = format!("{id}{suffix}");
    BUILTIN_FILES
        .iter()
        .find(|(name, _)| *name == sized)
        .or_else(|| BUILTIN_FILES.iter().find(|(name, _)| *name == id))
        .map(|(_, body)| decode_escapes(body).trim().to_string())
}

pub fn is_builtin(id: &str) -> bool {
    builtin(id, 8).is_some()
}

pub fn write_synced(target: &Path, program: &str) -> io::Result<()> {
    let is_new = !target.exists();
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(target)?;
    file.write_all(program.as_bytes())?;
    let _ = file.sync_all();
    if is_new && let Some(parent) = target.parent() && let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn read_uncached(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let file = File::open(path)?;
    let fd = file.as_raw_fd();
    let size = file.metadata()?.len() as usize;
    unsafe {
        libc::fcntl(fd, libc::F_NOCACHE, 1);
        libc::fcntl(fd, libc::F_RDAHEAD, 0);
        if size > 0 {
            let map = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
            if map != libc::MAP_FAILED {
                libc::msync(map, size, libc::MS_INVALIDATE);
                libc::munmap(map, size);
            }
        }
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let layout = std::alloc::Layout::from_size_align(page, page).map_err(io::Error::other)?;
    let buffer = unsafe { std::alloc::alloc(layout) };
    if buffer.is_null() {
        return Err(io::Error::other("allocation failed"));
    }
    let mut data = Vec::new();
    let mut offset = 0usize;
    let result = loop {
        if offset >= max_bytes {
            break Ok(());
        }
        let count = unsafe { libc::pread(fd, buffer.cast(), 512, offset as libc::off_t) };
        if count < 0 {
            break Err(io::Error::last_os_error());
        }
        let count = count as usize;
        let take = count.min(max_bytes - offset);
        data.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer, take) });
        if count < 512 {
            break Ok(());
        }
        offset += 512;
    };
    unsafe { std::alloc::dealloc(buffer, layout) };
    result.map(|()| data)
}

#[cfg(not(target_os = "macos"))]
pub fn read_uncached(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    File::open(path)?.take(max_bytes as u64).read_to_end(&mut data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_simple_escapes() {
        assert_eq!(decode_escapes(r"off\n#ff0000\trepeat\\x\q"), "off\n#ff0000\trepeat\\x\\q");
    }

    #[test]
    fn enforces_limits() {
        assert!(validate("").is_err());
        assert!(validate(&"a".repeat(513)).is_err());
        assert!(validate(&"x\n".repeat(21)).is_err());
        assert!(validate("#ff00ff").is_ok());
    }

    #[test]
    fn scales_brightness() {
        assert_eq!(apply_brightness("#fff", 255), "#fff");
        assert_eq!(apply_brightness("#fff", 128), "brightness 128\n#fff");
        assert_eq!(apply_brightness("brightness 200\n#fff", 128), "brightness 100\n#fff");
    }

    #[test]
    #[ignore = "needs SIDEPULSE_TEST_DEVICE pointing at a mounted device"]
    fn uncached_read_matches_device_file() {
        let root = std::path::PathBuf::from(std::env::var("SIDEPULSE_TEST_DEVICE").unwrap());
        let target = root.join(LED_FILE);
        let uncached = read_uncached(&target, MAX_BYTES).unwrap();
        let cached = std::fs::read(&target).unwrap();
        assert_eq!(uncached, cached[..cached.len().min(MAX_BYTES)]);
        let status = read_uncached(&root.join(STATUS_FILE), 4096).unwrap();
        assert!(String::from_utf8_lossy(&status).contains("serial "));
    }

    #[test]
    fn picks_sized_builtins() {
        assert!(builtin("cyan-roll", 8).unwrap().contains("7:#00E5FF"));
        assert!(!builtin("cyan-roll", 2).unwrap().contains("7:#00E5FF"));
        assert_eq!(builtin("solid-green", 2), builtin("solid-green", 8));
        assert!(builtin("nope", 8).is_none());
    }
}
