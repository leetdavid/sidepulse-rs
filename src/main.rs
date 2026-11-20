mod battery;
mod daemon;
mod device;
mod herdr;
mod hook;
mod install;
mod led;
mod ledsim;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod menubar;
#[cfg(target_os = "macos")]
mod settings_ui;
mod paths;
mod protocol;
mod settings;
mod state;
mod status;

use std::io::Read;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use protocol::Request;

use lexopt::prelude::*;

const HELP: &str = "\
sidepulse - lean SidePulse companion

USAGE:
  sidepulse <command> [options]

COMMANDS:
  hook --provider <claude|codex|cursor> [--event <name>]
                              Read an agent hook payload on stdin and report it
  write [PROGRAM|-] [--device PATH]
                              Write an LED program (\\n escapes allowed; stdin if '-')
  list [--json]               List connected SidePulse devices
  status [--json]             Show agent sessions tracked by the daemon
  pause | resume              Stop or restart automatic LED updates
  settings                    Open the settings window
  report --json               Submit a program_status report on stdin (used by the OpenCode plugin)
  herdr sync                  Push Herdr pane agent states to the daemon (used by the Herdr plugin)
  daemon [--no-menubar]       Run the background daemon (launchd runs this)
  install [--dry-run]         Install the LaunchAgent, move agent hooks here, park the Python agents
  uninstall [--dry-run]       Undo install and restore the Python hooks and agents
  help                        Show this help
";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    };
    let rest: Vec<_> = args.collect();
    let result = match command.to_string_lossy().as_ref() {
        "hook" => run_hook(rest),
        "write" => run_write(rest),
        "list" | "ls" => run_list(rest),
        "status" => run_status(rest),
        "settings" => daemon_request(&Request::OpenSettings).map(|_| ()),
        "pause" => set_outputs(false),
        "resume" => set_outputs(true),
        "daemon" => run_daemon(rest),
        "classify" => run_classify(rest),
        "report" => run_report(),
        "herdr" => match rest.first().map(|a| a.to_string_lossy().into_owned()).as_deref() {
            Some("sync") => herdr::sync(),
            _ => Err("usage: sidepulse herdr sync".into()),
        },
        "install" => install::install(rest.iter().any(|a| a == "--dry-run")),
        "uninstall" => install::uninstall(rest.iter().any(|a| a == "--dry-run")),
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{HELP}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sidepulse: {message}");
            ExitCode::FAILURE
        }
    }
}

fn parser(args: Vec<std::ffi::OsString>) -> lexopt::Parser {
    lexopt::Parser::from_args(args)
}

fn run_hook(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut provider = None;
    let mut event = None;
    let mut parser = parser(args);
    while let Ok(Some(arg)) = parser.next() {
        match arg {
            Long("provider") => provider = parser.value().ok().map(|v| v.to_string_lossy().into_owned()),
            Long("event") => event = parser.value().ok().map(|v| v.to_string_lossy().into_owned()),
            Long("log") => {
                let _ = parser.value();
            }
            _ => {}
        }
    }
    let provider = provider.unwrap_or_else(|| "claude".into());
    hook::run(&provider, event.as_deref());
    Ok(())
}

fn run_write(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let mut program = None;
    let mut device = None;
    let mut parser = parser(args);
    while let Some(arg) = parser.next().map_err(|e| e.to_string())? {
        match arg {
            Long("device") | Short('d') => device = Some(parser.value().map_err(|e| e.to_string())?.string().map_err(|_| "invalid --device")?),
            Value(value) if program.is_none() => program = Some(value.string().map_err(|_| "program must be UTF-8")?),
            other => return Err(other.unexpected().to_string()),
        }
    }
    let program = match program.as_deref() {
        Some("-") | None => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text).map_err(|e| e.to_string())?;
            text
        }
        Some(text) => text.to_string(),
    };
    let program = led::decode_escapes(&program);
    led::validate(&program)?;
    let device = device::resolve(device.as_deref())?;
    ledsim::validate(&program, device.led_count).map_err(|e| format!("invalid LED program: {e}"))?;
    led::write_synced(&device.target(), &program).map_err(|e| format!("{}: {e}", device.target().display()))?;
    println!("Wrote {}", device.target().display());
    Ok(())
}

fn run_list(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let json = args.iter().any(|a| a == "--json");
    let devices = device::discover();
    if json {
        println!("{}", serde_json::to_string_pretty(&devices).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if devices.is_empty() {
        println!("No SidePulse devices found.");
    }
    for d in devices {
        println!(
            "{}  {}  {} LEDs  {}",
            d.root.display(),
            d.model,
            d.led_count,
            d.serial.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn daemon_request(request: &Request) -> Result<serde_json::Value, String> {
    protocol::send(&paths::socket(), request, Duration::from_secs(3))
        .map_err(|e| format!("daemon unavailable ({e}); is the LaunchAgent running?"))
}

fn set_outputs(enabled: bool) -> Result<(), String> {
    daemon_request(&Request::SetOutputs { enabled })?;
    println!("LED updates {}", if enabled { "resumed" } else { "paused" });
    Ok(())
}

fn run_status(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let reply = daemon_request(&Request::GetState)?;
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&reply).map_err(|e| e.to_string())?);
        return Ok(());
    }
    let state: daemon::UiState = serde_json::from_value(reply["state"].clone()).map_err(|e| e.to_string())?;
    let snapshot = &state.snapshot;
    println!("Aggregate: {} ({} active)", snapshot.mode.label(), snapshot.active_count);
    if !state.outputs_enabled {
        println!("LED updates: paused");
    }
    let now = daemon::now_ms();
    for session in snapshot.sessions.iter().chain(&snapshot.recent) {
        let age = now.saturating_sub(session.updated_ms) / 1000;
        println!("  {:<18} {:>5}s  {}", session.mode.label(), age, session.display_name);
    }
    for device in &state.devices {
        let error = device.error.as_deref().map(|e| format!("  error: {e}")).unwrap_or_default();
        println!("Device: {} ({}, {}){error}", device.root.display(), device.model, device.display);
    }
    Ok(())
}

fn run_daemon(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let headless = args.iter().any(|a| a == "--no-menubar");
    daemon::trim_log();
    let signals = daemon::block_signals();
    let listener = daemon::bind_socket()?;
    let (tx, rx) = mpsc::channel();

    let signal_tx = tx.clone();
    thread::spawn(move || daemon::wait_for_signals(signals, signal_tx));
    let ipc_tx = tx.clone();
    thread::spawn(move || daemon::serve(listener, ipc_tx));
    let volume_tx = tx.clone();
    thread::spawn(move || daemon::watch_volumes(volume_tx));

    #[cfg(target_os = "macos")]
    {
        let publisher = if headless { None } else { menubar::publisher() };
        let engine = daemon::Engine::new(rx, publisher);
        thread::spawn(move || engine.run());
        macos::register_notifications(tx.clone());
        if headless {
            macos::run_main_loop();
        } else {
            menubar::run(tx, settings::Settings::load().show_menu_bar_icon);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = headless;
        drop(tx);
        daemon::Engine::new(rx, None).run();
    }
    Ok(())
}

fn run_classify(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let provider = args.get(1).map(|a| a.to_string_lossy().into_owned()).unwrap_or_else(|| "claude".into());
    let empty = std::collections::HashMap::new();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
    for line in input.lines() {
        let report = hook::build_report(&provider, None, line, &empty);
        let value = report.map(|r| {
            serde_json::json!({"mode": r.mode, "title": r.title, "kind": r.kind, "msg": r.message, "suppress": r.suppress})
        });
        println!("{}", serde_json::to_string(&value).unwrap_or_default());
    }
    Ok(())
}

fn run_report() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin().take(protocol::MAX_MESSAGE_BYTES).read_to_string(&mut input).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&input).map_err(|e| format!("invalid report JSON: {e}"))?;
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let report = protocol::report_from_program_status(&value, &env)?;
    daemon_request(&Request::Report(Box::new(report)))?;
    Ok(())
}
