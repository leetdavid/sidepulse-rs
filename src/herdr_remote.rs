use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::daemon::{Event, log};
use crate::herdr::agents_from_snapshot;
use crate::protocol::{HerdrAgent, Request};
use crate::settings::Settings;
use crate::state::REMOTE_SOURCE_PREFIX;

const MACHINE_REFRESH: Duration = Duration::from_secs(120);
const SETTINGS_CHECK: Duration = Duration::from_secs(10);
const RESYNC: Duration = Duration::from_secs(120);
const DEBOUNCE: Duration = Duration::from_millis(300);
const STOP_CHECK: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BACKOFF_SECS: u64 = 300;
const SUBSCRIBE_EVENTS: &[&str] = &["pane.agent_detected", "pane.created", "pane.exited", "pane.closed", "tab.closed", "workspace.closed"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub id: String,
    pub label: String,
    pub target: String,
    pub session: String,
}

pub fn parse_machines(json: &str) -> Vec<Machine> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    rows.iter()
        .filter(|m| m.get("enabled").and_then(Value::as_bool) == Some(true))
        .filter_map(|m| {
            let text = |key: &str| m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            Some(Machine {
                id: text("id")?,
                label: text("label").or_else(|| text("id"))?,
                target: text("target")?,
                session: text("session").unwrap_or_else(|| "default".into()),
            })
        })
        .collect()
}

pub fn bridge_script(session: &str) -> String {
    let session_flag = if session == "default" || session.is_empty() {
        String::new()
    } else {
        format!(" --session '{}'", session.replace('\'', ""))
    };
    format!(
        concat!(
            "h=$(command -v herdr 2>/dev/null || echo \"$HOME/.local/bin/herdr\"); ",
            "s=$(\"$h\"{} status server --json 2>/dev/null | sed -n 's/.*\"socket\":\"\\([^\"]*\\)\".*/\\1/p'); ",
            "[ -S \"$s\" ] || s=\"$HOME/.config/herdr/herdr.sock\"; ",
            "if command -v socat >/dev/null 2>&1; then exec socat - \"UNIX-CONNECT:$s\"; ",
            "elif command -v nc >/dev/null 2>&1 && nc -h 2>&1 | grep -q -e '-U'; then exec nc -U \"$s\"; ",
            "else exec python3 -c 'import os,socket,sys,threading;c=socket.socket(socket.AF_UNIX);c.connect(sys.argv[1]);",
            "threading.Thread(target=lambda:[c.sendall(l) for l in sys.stdin.buffer],daemon=True).start();",
            "[os.write(1,b) for b in iter(lambda:c.recv(65536),b\"\")]' \"$s\"; fi"
        ),
        session_flag
    )
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<String, String> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let out = reader.join().unwrap_or_default();
                return if status.success() { Ok(out) } else { Err(format!("exited with {status}")) };
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn list_machines(herdr: &Path) -> Result<Vec<Machine>, String> {
    let mut command = Command::new(herdr);
    command.args(["machine", "list", "--json"]).env_remove("HERDR_SOCKET_PATH");
    Ok(parse_machines(&run_with_timeout(command, Duration::from_secs(10))?))
}

fn snapshot(herdr: &Path, machine: &Machine) -> Result<Vec<HerdrAgent>, String> {
    let mut command = Command::new(herdr);
    command.args(["--machine", &machine.id, "api", "snapshot"]).env_remove("HERDR_SOCKET_PATH");
    let text = run_with_timeout(command, COMMAND_TIMEOUT)?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("snapshot: {e}"))?;
    let result = value.get("result").unwrap_or(&value);
    let snapshot = result.get("snapshot").unwrap_or(result);
    if !snapshot.get("agents").is_some_and(Value::is_array) {
        return Err("snapshot has no agents".into());
    }
    Ok(agents_from_snapshot(snapshot))
}

fn spawn_bridge(machine: &Machine) -> Result<Child, String> {
    Command::new("/usr/bin/ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=3", "-T"])
        .arg(&machine.target)
        .arg(bridge_script(&machine.session))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))
}

fn subscriptions(agents: &[HerdrAgent]) -> Value {
    let mut subs: Vec<Value> = SUBSCRIBE_EVENTS.iter().map(|t| json!({"type": t})).collect();
    subs.extend(agents.iter().map(|a| json!({"type": "pane.agent_status_changed", "pane_id": a.pane_id})));
    json!({"id": "sidepulse", "method": "events.subscribe", "params": {"subscriptions": subs}})
}

fn panes(agents: &[HerdrAgent]) -> Vec<String> {
    let mut panes: Vec<String> = agents.iter().map(|a| a.pane_id.clone()).collect();
    panes.sort();
    panes
}

struct Worker {
    herdr: PathBuf,
    machine: Machine,
    stop: Arc<AtomicBool>,
    tx: Sender<Event>,
}

impl Worker {
    fn source(&self) -> String {
        format!("{REMOTE_SOURCE_PREFIX}{}", self.machine.id)
    }

    fn publish(&self, agents: Vec<HerdrAgent>, connected: bool) {
        let request = Request::HerdrSync { socket: self.source(), agents, label: Some(self.machine.label.clone()), connected };
        let _ = self.tx.send(Event::Request(request, None));
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn run(self) {
        let mut failures = 0u32;
        while !self.stopped() {
            match self.follow() {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures += 1;
                    if failures == 1 {
                        log(&format!("herdr {}: {error}", self.machine.label));
                    }
                    match snapshot(&self.herdr, &self.machine) {
                        Ok(agents) => self.publish(agents, true),
                        Err(_) => self.publish(Vec::new(), false),
                    }
                    let wait = Duration::from_secs((5u64 << failures.min(6)).min(MAX_BACKOFF_SECS));
                    let until = Instant::now() + wait;
                    while Instant::now() < until && !self.stopped() {
                        thread::sleep(Duration::from_millis(500).min(until.saturating_duration_since(Instant::now())));
                    }
                }
            }
        }
        self.publish(Vec::new(), true);
    }

    fn follow(&self) -> Result<(), String> {
        let agents = snapshot(&self.herdr, &self.machine)?;
        let subscribed = panes(&agents);
        let request = subscriptions(&agents);
        self.publish(agents, true);

        let mut child = spawn_bridge(&self.machine)?;
        let result = self.stream(&mut child, &request, &subscribed);
        let _ = child.kill();
        let _ = child.wait();
        result
    }

    fn stream(&self, child: &mut Child, request: &Value, subscribed: &[String]) -> Result<(), String> {
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        writeln!(stdin, "{request}").map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;
        let (line_tx, lines) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line_tx.send(line).is_err() {
                    return;
                }
            }
        });
        let ack = lines.recv_timeout(Duration::from_secs(30)).map_err(|_| "no subscription acknowledgement".to_string())?;
        if !ack.contains("subscription_started") {
            return Err(format!("subscription rejected: {}", crate::status::truncate(&ack, 160)));
        }
        let mut last_sync = Instant::now();
        loop {
            if self.stopped() {
                return Ok(());
            }
            let resync = match lines.recv_timeout(STOP_CHECK) {
                Ok(_) => {
                    thread::sleep(DEBOUNCE);
                    while lines.try_recv().is_ok() {}
                    true
                }
                Err(RecvTimeoutError::Timeout) => last_sync.elapsed() >= RESYNC,
                Err(RecvTimeoutError::Disconnected) => return Err("connection closed".into()),
            };
            if resync {
                let agents = snapshot(&self.herdr, &self.machine)?;
                let changed_panes = panes(&agents) != subscribed;
                self.publish(agents, true);
                last_sync = Instant::now();
                if changed_panes {
                    return Ok(());
                }
            }
        }
    }
}

pub fn supervise(herdr: PathBuf, tx: Sender<Event>) {
    let mut workers: HashMap<String, (Machine, Arc<AtomicBool>)> = HashMap::new();
    let mut next_refresh = Instant::now();
    loop {
        let enabled = Settings::load().herdr_remote_enabled;
        if !enabled {
            for (_, (_, stop)) in workers.drain() {
                stop.store(true, Ordering::Relaxed);
            }
        } else if Instant::now() >= next_refresh {
            next_refresh = Instant::now() + MACHINE_REFRESH;
            if let Ok(machines) = list_machines(&herdr) {
                workers.retain(|id, (machine, stop)| {
                    let keep = machines.iter().any(|m| m.id == *id && m == machine);
                    if !keep {
                        stop.store(true, Ordering::Relaxed);
                    }
                    keep
                });
                for machine in machines {
                    if workers.contains_key(&machine.id) {
                        continue;
                    }
                    let stop = Arc::new(AtomicBool::new(false));
                    let worker = Worker { herdr: herdr.clone(), machine: machine.clone(), stop: stop.clone(), tx: tx.clone() };
                    thread::spawn(move || worker.run());
                    workers.insert(machine.id.clone(), (machine, stop));
                }
            }
        }
        if !enabled {
            next_refresh = Instant::now();
        }
        thread::sleep(SETTINGS_CHECK);
    }
}

pub fn check() -> Result<(), String> {
    let herdr = crate::herdr::executable().ok_or("herdr not found")?;
    let machines = list_machines(&herdr)?;
    if machines.is_empty() {
        println!("No enabled Herdr machines.");
    }
    for machine in machines {
        let snapshot = snapshot(&herdr, &machine).map(|a| format!("{} agents", a.len()));
        let stream = spawn_bridge(&machine).and_then(|mut child| {
            let mut stdin = child.stdin.take().ok_or("no stdin")?;
            let stdout = child.stdout.take().ok_or("no stdout")?;
            writeln!(stdin, "{}", subscriptions(&[])).map_err(|e| e.to_string())?;
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let mut line = String::new();
                let _ = BufReader::new(stdout).read_line(&mut line);
                let _ = tx.send(line);
            });
            let reply = rx.recv_timeout(Duration::from_secs(20)).unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            if reply.contains("subscription_started") { Ok("live events".to_string()) } else { Err("event stream unavailable".to_string()) }
        });
        let show = |r: Result<String, String>| r.unwrap_or_else(|e| format!("failed ({e})"));
        println!("{:<14} {:<22} snapshot: {:<14} stream: {}", machine.label, machine.target, show(snapshot), show(stream));
    }
    Ok(())
}

pub fn focus(herdr: &Path, machine_id: &str, pane_id: &str) {
    let mut command = Command::new(herdr);
    command.args(["--machine", machine_id, "agent", "focus", pane_id]).env_remove("HERDR_SOCKET_PATH");
    let _ = run_with_timeout(command, Duration::from_secs(10));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enabled_machines_only() {
        let machines = parse_machines(
            r#"[{"id":"a1","label":"mac","target":"mac","session":"default","enabled":true},
                {"id":"b2","label":"Off","target":"x","session":"default","enabled":false},
                {"id":"c3","target":"me@ws","enabled":true}]"#,
        );
        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].label, "mac");
        assert_eq!(machines[1].label, "c3");
        assert_eq!(machines[1].session, "default");
        assert!(parse_machines("not json").is_empty());
    }

    #[test]
    fn bridge_script_is_valid_shell() {
        for session in ["default", "work"] {
            let script = bridge_script(session);
            let status = Command::new("/bin/sh").args(["-n", "-c", &script]).status().unwrap();
            assert!(status.success(), "{script}");
        }
        assert!(bridge_script("work").contains("--session 'work'"));
        assert!(!bridge_script("default").contains("--session"));
    }

    #[test]
    fn subscribes_per_agent_pane() {
        let agent = HerdrAgent {
            terminal_id: "t".into(),
            pane_id: "w1:p2".into(),
            provider: "claude".into(),
            mode: crate::status::Mode::IdleReady,
            session_id: None,
            title: None,
            cwd: None,
        };
        let request = subscriptions(&[agent]);
        let subs = request["params"]["subscriptions"].as_array().unwrap();
        assert!(subs.iter().any(|s| s["type"] == "pane.agent_status_changed" && s["pane_id"] == "w1:p2"));
        assert_eq!(subs.len(), SUBSCRIBE_EVENTS.len() + 1);
    }
}
