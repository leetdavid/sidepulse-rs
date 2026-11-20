use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};

use crate::paths;
use crate::protocol::{self, HerdrAgent, Request};
use crate::status::Mode;

const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

pub fn request(socket: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("Herdr socket {socket}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let mut line = serde_json::to_vec(&json!({"id": "sidepulse", "method": method, "params": params})).map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream.take_limited()).read_line(&mut reply).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&reply).map_err(|e| format!("Herdr {method}: {e}"))?;
    match value.get("result") {
        Some(result) if result.is_object() => Ok(result.clone()),
        _ => Err(format!("Herdr {method} failed: {}", value.get("error").unwrap_or(&value))),
    }
}

trait TakeLimited {
    fn take_limited(self) -> std::io::Take<UnixStream>;
}

impl TakeLimited for UnixStream {
    fn take_limited(self) -> std::io::Take<UnixStream> {
        std::io::Read::take(self, MAX_REPLY_BYTES as u64)
    }
}

fn mode(status: &str) -> Option<Mode> {
    match status {
        "working" => Some(Mode::Working),
        "blocked" => Some(Mode::WaitingForInput),
        "idle" => Some(Mode::IdleReady),
        "done" => Some(Mode::Completed),
        _ => None,
    }
}

pub fn agents_from_snapshot(snapshot: &Value) -> Vec<HerdrAgent> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    snapshot
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| {
            let session = agent.get("agent_session").filter(|s| s.get("kind").and_then(Value::as_str) == Some("id"));
            Some(HerdrAgent {
                terminal_id: text(agent, "terminal_id")?,
                pane_id: text(agent, "pane_id")?,
                provider: text(agent, "agent")?,
                mode: mode(agent.get("agent_status")?.as_str()?)?,
                session_id: session.and_then(|s| text(s, "value")),
                title: text(agent, "title").or_else(|| text(agent, "terminal_title_stripped")),
                cwd: text(agent, "foreground_cwd").or_else(|| text(agent, "cwd")),
            })
        })
        .collect()
}

pub fn sync() -> Result<(), String> {
    let socket = std::env::var("HERDR_SOCKET_PATH").map_err(|_| "HERDR_SOCKET_PATH is not set; run this from the Herdr plugin")?;
    let result = request(&socket, "session.snapshot", json!({}), Duration::from_secs(2))?;
    let snapshot = result.get("snapshot").ok_or("Herdr returned no snapshot")?;
    let agents = agents_from_snapshot(snapshot);
    protocol::send(&paths::socket(), &Request::HerdrSync { socket, agents }, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn focus_pane(socket: &str, pane_id: &str) -> Result<(), String> {
    request(socket, "pane.focus", json!({"pane_id": pane_id}), Duration::from_millis(500)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_snapshot_agents() {
        let snapshot = json!({"agents": [
            {"agent": "claude", "agent_session": {"kind": "id", "value": "abc"}, "agent_status": "blocked",
             "pane_id": "w1:p1", "terminal_id": "term_1", "terminal_title_stripped": "Fix it", "foreground_cwd": "/x"},
            {"agent": "codex", "agent_status": "weird", "pane_id": "w1:p2", "terminal_id": "term_2"},
            {"agent": "opencode", "agent_status": "working", "pane_id": "w1:p3", "terminal_id": "term_3", "title": "T"}
        ]});
        let agents = agents_from_snapshot(&snapshot);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].mode, Mode::WaitingForInput);
        assert_eq!(agents[0].session_id.as_deref(), Some("abc"));
        assert_eq!(agents[0].title.as_deref(), Some("Fix it"));
        assert_eq!(agents[1].provider, "opencode");
        assert_eq!(agents[1].session_id, None);
    }
}
