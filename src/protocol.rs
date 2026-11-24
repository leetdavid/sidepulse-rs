use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::status::Mode;

pub const MAX_MESSAGE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub scope: String,
    pub provider: String,
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub title_from_session: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(default)]
    pub unless_done: bool,
    #[serde(default)]
    pub unless_question: bool,
    #[serde(default)]
    pub suppress: bool,
    #[serde(default)]
    pub clear: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_socket: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HerdrAgent {
    pub terminal_id: String,
    pub pane_id: String,
    pub provider: String,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Report(Box<Report>),
    GetState,
    WriteLed {
        program: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default = "default_hold")]
        hold_seconds: f64,
    },
    SetOutputs {
        enabled: bool,
    },
    ClearSession {
        scope: String,
    },
    HerdrSync {
        socket: String,
        agents: Vec<HerdrAgent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default = "connected")]
        connected: bool,
    },
    OpenSettings,
    Stop,
}

fn connected() -> bool {
    true
}

fn default_hold() -> f64 {
    0.0
}

pub fn send(socket: &Path, request: &Request, timeout: Duration) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(&serde_json::to_vec(request)?)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = Vec::new();
    stream.take(MAX_MESSAGE_BYTES * 8).read_to_end(&mut reply)?;
    let value: Value = serde_json::from_slice(&reply)?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value)
    } else {
        let error = value.get("error").and_then(Value::as_str).unwrap_or("request rejected");
        Err(std::io::Error::other(error.to_string()))
    }
}

pub fn report_from_program_status(value: &Value, env: &std::collections::HashMap<String, String>) -> Result<Report, String> {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let status = value.get("status").ok_or("report has no status")?;
    let context = value.get("context").cloned().unwrap_or(Value::Null);
    let state = text(status.get("state")).ok_or("report has no status.state")?;
    let provider = text(context.get("provider")).or_else(|| text(status.get("app"))).unwrap_or_else(|| "program".into());
    let session_id = text(context.get("session_id"));
    let scope = match &session_id {
        Some(id) => format!("{provider}:{id}"),
        None => text(value.get("scope")).ok_or("report has no scope or session_id")?,
    };
    let mode = match state.as_str() {
        "idle" => Some(Mode::IdleReady),
        "working" => Some(Mode::Working),
        "blocked" => Some(Mode::WaitingForInput),
        "done" => Some(Mode::Completed),
        "error" => Some(Mode::BlockedError),
        "clear" => None,
        other => return Err(format!("unknown state '{other}'")),
    };
    let origin = match provider.as_str() {
        "opencode" => "OpenCode".to_string(),
        other => crate::status::provider_label(other),
    };
    Ok(Report {
        scope,
        mode,
        clear: mode.is_none(),
        kind: text(status.get("kind")),
        event: text(context.get("event_name")).unwrap_or_else(|| "ProgramStatus".into()),
        session_id,
        cwd: text(context.get("cwd")),
        title: text(context.get("title")).or_else(|| text(status.get("title"))),
        title_from_session: text(context.get("title_source")).as_deref() == Some("session"),
        message: text(status.get("msg")).map(|m| crate::status::clean_text(&m, 1024)),
        origin: Some(origin),
        origin_kind: Some(format!("{provider}_plugin")),
        terminal: env.get("TERM_PROGRAM").cloned(),
        request_id: text(value.get("request_id")),
        herdr_pane: env.get("HERDR_PANE_ID").cloned(),
        herdr_socket: env.get("HERDR_SOCKET_PATH").cloned(),
        provider,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_opencode_reports() {
        let value: Value = serde_json::from_str(
            r#"{"v":1,"type":"program_status","scope":"opencode-ses_1","request_id":"opencode-evt_1",
                "status":{"state":"blocked","app":"opencode","kind":"permission","msg":"Run bash","title":"Fix tests"},
                "context":{"provider":"opencode","session_id":"ses_1","event_name":"permission.asked","cwd":"/w","title":"Fix tests","title_source":"session"}}"#,
        )
        .unwrap();
        let report = report_from_program_status(&value, &Default::default()).unwrap();
        assert_eq!(report.scope, "opencode:ses_1");
        assert_eq!(report.mode, Some(Mode::WaitingForInput));
        assert_eq!(report.kind.as_deref(), Some("permission"));
        assert!(report.title_from_session);
        let clear: Value = serde_json::from_str(r#"{"status":{"state":"clear"},"context":{"provider":"opencode","session_id":"ses_1"}}"#).unwrap();
        assert!(report_from_program_status(&clear, &Default::default()).unwrap().clear);
    }
}
