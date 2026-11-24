use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::daemon::now_ms;
use crate::paths;

const LABEL: &str = "io.sidepulse.rs";
const PYTHON_AGENTS: &[&str] = &["io.sidepulse.service", "io.sidepulse.agentstatus"];
const HOOK_MARKER: &str = "hook_entry.py";

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    hooks: Vec<HookChange>,
    disabled_agents: Vec<String>,
    #[serde(default)]
    herdr_plugin: bool,
    #[serde(default)]
    opencode_plugin: Option<PathBuf>,
}

const HERDR_PLUGIN_ID: &str = "sidepulse.status";
const HERDR_MANIFEST: &str = include_str!("../resources/herdr/herdr-plugin.toml");
const OPENCODE_PLUGIN: &str = include_str!("../resources/opencode/sidepulse.ts");
const OPENCODE_MARKER: &str = "Managed by sidepulse-rs";

fn herdr_executable() -> Option<PathBuf> {
    crate::herdr::executable()
}

fn herdr(args: &[&str]) -> Result<(), String> {
    let exe = herdr_executable().ok_or("herdr not found")?;
    let output = Command::new(exe).args(args).env_remove("HERDR_SOCKET_PATH").stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn herdr_plugin_dir() -> PathBuf {
    paths::install_dir().join("herdr-plugin")
}

fn opencode_plugin_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| paths::home().join(".config"));
    let dir = base.join("opencode");
    dir.is_dir().then(|| dir.join("plugins").join("sidepulse.ts"))
}

fn install_integrations(manifest: &mut Manifest, binary: &Path, dry_run: bool) -> Result<(), String> {
    let prefix = if dry_run { "would " } else { "" };
    if herdr_executable().is_some() {
        println!("{prefix}link Herdr plugin {HERDR_PLUGIN_ID} from {}", herdr_plugin_dir().display());
        if !dry_run {
            let dir = herdr_plugin_dir();
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            fs::write(dir.join("herdr-plugin.toml"), HERDR_MANIFEST).map_err(|e| e.to_string())?;
            fs::write(dir.join("sync.sh"), format!("#!/bin/sh\nexec '{}' herdr sync\n", binary.display())).map_err(|e| e.to_string())?;
            let _ = herdr(&["plugin", "unlink", HERDR_PLUGIN_ID]);
            match herdr(&["plugin", "link", &dir.to_string_lossy()]) {
                Ok(()) => manifest.herdr_plugin = true,
                Err(error) => println!("    Herdr plugin link failed: {error}"),
            }
        }
    }
    if let Some(path) = opencode_plugin_path() {
        let ours = fs::read_to_string(&path).map_or(true, |t| t.contains(OPENCODE_MARKER));
        if !ours {
            println!("skip OpenCode plugin: {} exists and is not managed by sidepulse-rs", path.display());
        } else {
            println!("{prefix}install OpenCode plugin {}", path.display());
            if !dry_run {
                fs::create_dir_all(path.parent().unwrap_or(Path::new("/"))).map_err(|e| e.to_string())?;
                fs::write(&path, OPENCODE_PLUGIN).map_err(|e| e.to_string())?;
                manifest.opencode_plugin = Some(path);
            }
        }
    }
    Ok(())
}

fn uninstall_integrations(manifest: &Manifest, dry_run: bool) {
    let prefix = if dry_run { "would " } else { "" };
    if manifest.herdr_plugin {
        println!("{prefix}unlink Herdr plugin {HERDR_PLUGIN_ID}");
        if !dry_run {
            let _ = herdr(&["plugin", "unlink", HERDR_PLUGIN_ID]);
            let _ = fs::remove_dir_all(herdr_plugin_dir());
        }
    }
    if let Some(path) = &manifest.opencode_plugin
        && fs::read_to_string(path).is_ok_and(|t| t.contains(OPENCODE_MARKER))
    {
        println!("{prefix}remove OpenCode plugin {}", path.display());
        if !dry_run {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct HookChange {
    file: PathBuf,
    old: String,
    new: String,
}

fn manifest_path() -> PathBuf {
    paths::install_dir().join("manifest.json")
}

fn load_manifest() -> Manifest {
    fs::read_to_string(manifest_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_manifest(manifest: &Manifest) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest).map_err(|e| e.to_string())?;
    fs::write(manifest_path(), text).map_err(|e| e.to_string())
}

fn hook_files() -> Vec<PathBuf> {
    let home = paths::home();
    [".claude/settings.json", ".codex/config.toml", ".cursor/hooks.json", ".grok/hooks/sidepulse.json", ".junie/config.json"]
        .iter()
        .map(|p| home.join(p))
        .filter(|p| p.exists())
        .collect()
}

fn launch_agents_dir() -> PathBuf {
    paths::home().join("Library").join("LaunchAgents")
}

fn disabled_agents_dir() -> PathBuf {
    paths::install_dir().join("disabled-launchagents")
}

fn agent_plist() -> PathBuf {
    launch_agents_dir().join(format!("{LABEL}.plist"))
}

fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> bool {
    if std::env::var_os("SIDEPULSE_RS_SKIP_LAUNCHCTL").is_some() {
        println!("    (skipped) launchctl {}", args.join(" "));
        return true;
    }
    Command::new("/bin/launchctl").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn python_hook_commands(text: &str, binary: &Path) -> Vec<(String, String)> {
    let mut changes: Vec<(String, String)> = Vec::new();
    let mut search = 0;
    while let Some(found) = text[search..].find(HOOK_MARKER) {
        let at = search + found;
        search = at + HOOK_MARKER.len();
        let start = text[..at].rfind(['"', '\'']).map_or(0, |i| i + 1);
        let Some(len) = text[at..].find(['"', '\'']) else { continue };
        let old = &text[start..at + len];
        let tokens: Vec<&str> = old.split_whitespace().collect();
        let value = |flag: &str| tokens.iter().position(|t| *t == flag).and_then(|i| tokens.get(i + 1)).copied();
        let Some(provider) = value("--provider") else { continue };
        let event = value("--event").map(|e| format!(" --event {e}")).unwrap_or_default();
        let new = format!("{} hook --provider {provider}{event} ; true", binary.display());
        if !changes.iter().any(|(o, _)| o == old) {
            changes.push((old.to_string(), new));
        }
    }
    changes
}

fn backup(file: &Path) -> Result<PathBuf, String> {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target = file.with_file_name(format!("{name}.sidepulse-rs-backup-{}", now_ms() / 1000));
    fs::copy(file, &target).map_err(|e| format!("backup {}: {e}", file.display()))?;
    Ok(target)
}

fn replace_in_file(file: &Path, pairs: &[(String, String)], dry_run: bool) -> Result<usize, String> {
    let mut text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut count = 0;
    for (from, to) in pairs {
        count += text.matches(from.as_str()).count();
        text = text.replace(from.as_str(), to);
    }
    if count > 0 && !dry_run {
        backup(file)?;
        let tmp = file.with_extension("sidepulse-rs-tmp");
        fs::write(&tmp, text).map_err(|e| e.to_string())?;
        fs::rename(&tmp, file).map_err(|e| e.to_string())?;
    }
    Ok(count)
}

fn plist(binary: &Path) -> String {
    let log = paths::log_file();
    let home = paths::home();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
		<string>daemon</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>WorkingDirectory</key>
	<string>{}</string>
	<key>StandardOutPath</key>
	<string>{}</string>
	<key>StandardErrorPath</key>
	<string>{}</string>
</dict>
</plist>
"#,
        binary.display(),
        home.display(),
        log.display(),
        log.display()
    )
}

pub fn install(dry_run: bool) -> Result<(), String> {
    let binary = paths::installed_binary();
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut manifest = load_manifest();
    let prefix = if dry_run { "would " } else { "" };

    println!("{prefix}install binary: {}", binary.display());
    if !dry_run {
        fs::create_dir_all(binary.parent().unwrap_or(Path::new("/"))).map_err(|e| e.to_string())?;
        fs::create_dir_all(paths::state_dir()).map_err(|e| e.to_string())?;
        if current != binary {
            let tmp = binary.with_extension("new");
            fs::copy(&current, &tmp).map_err(|e| format!("copy binary: {e}"))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            fs::rename(&tmp, &binary).map_err(|e| e.to_string())?;
        }
        let link = paths::home().join(".local/bin/sidepulse-rs");
        let _ = fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink(&binary, &link);
    }

    for file in hook_files() {
        let text = fs::read_to_string(&file).unwrap_or_default();
        let pairs = python_hook_commands(&text, &binary);
        if pairs.is_empty() {
            continue;
        }
        let count = replace_in_file(&file, &pairs, dry_run)?;
        println!("{prefix}rewrite {count} hook command(s) in {}", file.display());
        for (old, new) in &pairs {
            if dry_run {
                println!("    {old}\n -> {new}");
            }
            let change = HookChange { file: file.clone(), old: old.clone(), new: new.clone() };
            if !manifest.hooks.contains(&change) {
                manifest.hooks.push(change);
            }
        }
    }

    for label in PYTHON_AGENTS {
        let plist = launch_agents_dir().join(format!("{label}.plist"));
        if !plist.exists() {
            continue;
        }
        println!("{prefix}stop and park Python LaunchAgent {label}");
        if !dry_run {
            launchctl(&["bootout", &gui_domain(), &plist.to_string_lossy()]);
            fs::create_dir_all(disabled_agents_dir()).map_err(|e| e.to_string())?;
            fs::rename(&plist, disabled_agents_dir().join(format!("{label}.plist"))).map_err(|e| e.to_string())?;
            if !manifest.disabled_agents.iter().any(|l| l == label) {
                manifest.disabled_agents.push(label.to_string());
            }
        }
    }

    install_integrations(&mut manifest, &binary, dry_run)?;

    println!("{prefix}install and start LaunchAgent {LABEL}");
    if !dry_run {
        let path = agent_plist();
        launchctl(&["bootout", &gui_domain(), &path.to_string_lossy()]);
        fs::write(&path, plist(&binary)).map_err(|e| e.to_string())?;
        if !launchctl(&["bootstrap", &gui_domain(), &path.to_string_lossy()]) {
            return Err(format!("launchctl bootstrap failed for {}", path.display()));
        }
        save_manifest(&manifest)?;
        if manifest.herdr_plugin {
            std::thread::sleep(std::time::Duration::from_millis(800));
            if let Err(error) = herdr(&["plugin", "action", "invoke", &format!("{HERDR_PLUGIN_ID}.sync")]) {
                println!("    Herdr initial sync skipped: {error}");
            }
        }
        println!("Installed. Logs: {}", paths::log_file().display());
    }
    Ok(())
}

pub fn uninstall(dry_run: bool) -> Result<(), String> {
    let manifest = load_manifest();
    let prefix = if dry_run { "would " } else { "" };

    println!("{prefix}stop and remove LaunchAgent {LABEL}");
    if !dry_run {
        launchctl(&["bootout", &gui_domain(), &agent_plist().to_string_lossy()]);
        let _ = fs::remove_file(agent_plist());
    }

    uninstall_integrations(&manifest, dry_run);

    let mut files: Vec<&PathBuf> = manifest.hooks.iter().map(|h| &h.file).collect();
    files.dedup();
    for file in files {
        let pairs: Vec<(String, String)> =
            manifest.hooks.iter().filter(|h| &h.file == file).map(|h| (h.new.clone(), h.old.clone())).collect();
        if file.exists() {
            let count = replace_in_file(file, &pairs, dry_run)?;
            println!("{prefix}restore {count} Python hook command(s) in {}", file.display());
        }
    }

    for label in &manifest.disabled_agents {
        let parked = disabled_agents_dir().join(format!("{label}.plist"));
        let plist = launch_agents_dir().join(format!("{label}.plist"));
        if !parked.exists() {
            continue;
        }
        println!("{prefix}restore and start Python LaunchAgent {label}");
        if !dry_run {
            fs::rename(&parked, &plist).map_err(|e| e.to_string())?;
            launchctl(&["bootstrap", &gui_domain(), &plist.to_string_lossy()]);
        }
    }

    if !dry_run {
        let _ = fs::remove_file(manifest_path());
        let _ = fs::remove_file(paths::home().join(".local/bin/sidepulse-rs"));
        println!("Uninstalled. The binary remains at {}", paths::installed_binary().display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_json_and_toml_hook_commands() {
        let bin = Path::new("/opt/sp/sidepulse");
        let json = r#"{"command": "/v/bin/python /v/sidepulse/hook_entry.py --provider cursor --event stop --log /s/cursor.jsonl ; true"}"#;
        let pairs = python_hook_commands(json, bin);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1, "/opt/sp/sidepulse hook --provider cursor --event stop ; true");
        assert!(json.replace(&pairs[0].0, &pairs[0].1).contains(r#""command": "/opt/sp/sidepulse hook"#));

        let toml = "command = '''/v/bin/python /v/sidepulse/hook_entry.py --provider codex --log /s/codex.jsonl ; true'''\n";
        let pairs = python_hook_commands(toml, bin);
        assert_eq!(toml.replace(&pairs[0].0, &pairs[0].1), "command = '''/opt/sp/sidepulse hook --provider codex ; true'''\n");
    }
}

pub fn integration_status() -> Vec<(&'static str, bool)> {
    let home = paths::home();
    let binary = paths::installed_binary().display().to_string();
    let hooked = |relative: &str, provider: &str| {
        fs::read_to_string(home.join(relative)).is_ok_and(|t| t.contains(&format!("{binary} hook --provider {provider}")))
    };
    let manifest = load_manifest();
    vec![
        ("Claude Code", hooked(".claude/settings.json", "claude")),
        ("Codex", hooked(".codex/config.toml", "codex")),
        ("Cursor", hooked(".cursor/hooks.json", "cursor")),
        ("Grok", hooked(".grok/hooks/sidepulse.json", "grok")),
        ("Junie", hooked(".junie/config.json", "junie")),
        ("OpenCode", opencode_plugin_path().is_some_and(|p| fs::read_to_string(p).is_ok_and(|t| t.contains(OPENCODE_MARKER)))),
        ("Herdr", manifest.herdr_plugin && herdr_plugin_dir().join("herdr-plugin.toml").exists()),
    ]
}
