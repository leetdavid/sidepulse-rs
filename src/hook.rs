use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::paths;
use crate::protocol::{self, Report, Request};
use crate::status::{self, Event, Mode, str_field};

const MAX_STDIN_BYTES: u64 = 64 * 1024 * 1024;
const MESSAGE_LIMIT: usize = 1024;

pub fn run(provider: &str, event: Option<&str>) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_STDIN_BYTES).read_to_string(&mut input);
    if env::var("SIDEPULSE_DISABLE_EVENT_SOCKET").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes")) {
        return 0;
    }
    let Some(report) = build_report(provider, event, &input, &env_map()) else {
        return 0;
    };
    let _ = protocol::send(&paths::socket(), &Request::Report(Box::new(report)), Duration::from_millis(250));
    0
}

fn env_map() -> HashMap<String, String> {
    env::vars().collect()
}

pub fn build_report(provider: &str, cli_event: Option<&str>, input: &str, env: &HashMap<String, String>) -> Option<Report> {
    let mut raw = match serde_json::from_str::<Value>(if input.trim().is_empty() { "{}" } else { input }) {
        Ok(Value::Object(map)) => map,
        _ => return None,
    };
    if provider == "cursor" && let Some(name) = cli_event {
        raw = normalize_cursor(name, raw);
    }
    let event = Event::parse(provider, raw)?;
    let mode = status::mode_for_event(&event);
    let scope = format!("{}:{}", provider, event.session_id.clone().unwrap_or_else(|| event.status_key()));

    if codex_internal(&event) {
        return Some(Report { scope, provider: provider.into(), event: event.name, suppress: true, ..Default::default() });
    }
    let mode = mode?;

    let session_title = if provider == "codex" { event.session_id.as_deref().and_then(codex_session_title) } else { None };
    let title = session_title.clone().or_else(|| prompt_title(&event));
    let mut message = event.message.clone();
    if mode == Mode::WaitingForInput && let Some(question) = question_text(event.raw.get("tool_input")) {
        message = Some(question);
    }
    let kind = (mode == Mode::WaitingForInput).then(|| {
        if event.name.to_lowercase().contains("permission") { "permission" } else { "question" }.to_string()
    });
    let origin = detect_origin(provider, &event.raw, env);
    let ordinary_codex_tool = provider == "codex"
        && event.codex_question_tool().is_none()
        && !matches!(event.name.as_str(), "UserPromptSubmit" | "Stop" | "SubagentStop" | "Interrupt" | "SessionEnd" | "SessionStart")
        && mode != Mode::BlockedError
        && status::explicit_mode(&event).is_none();

    Some(Report {
        scope,
        provider: provider.into(),
        mode: Some(mode),
        kind,
        session_id: event.session_id.clone(),
        agent_id: event.agent_id.clone(),
        cwd: event.cwd.clone(),
        title: title.map(|t| status::clean_text(&t, 192)),
        title_from_session: session_title.is_some(),
        tool_name: event.tool_name.clone().map(|t| status::clean_text(&t, 256)),
        message: message.map(|m| status::clean_text(&m, MESSAGE_LIMIT)),
        origin: origin.as_ref().map(|o| o.0.clone()),
        origin_kind: origin.map(|o| o.1),
        terminal: env.get("TERM_PROGRAM").cloned(),
        unless_done: event.name == "Notification",
        unless_question: ordinary_codex_tool,
        herdr_pane: env.get("HERDR_PANE_ID").cloned(),
        herdr_socket: env.get("HERDR_SOCKET_PATH").cloned(),
        event: event.name,
        ..Default::default()
    })
}

fn prompt_title(event: &Event) -> Option<String> {
    if !matches!(event.name.as_str(), "UserPromptSubmit" | "PreInvocation") {
        return None;
    }
    status::summarize_prompt(str_field(&event.raw, "prompt")?, 72)
}

fn question_text(input: Option<&Value>) -> Option<String> {
    input?.get("questions")?.as_array()?.iter().find_map(|q| {
        ["question", "title", "header"].iter().find_map(|field| {
            let text = q.get(*field)?.as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
            (!text.is_empty()).then(|| status::clean_text(&text, MESSAGE_LIMIT))
        })
    })
}

fn normalize_cursor(event_name: &str, payload: Map<String, Value>) -> Map<String, Value> {
    let canonical = match event_name {
        "sessionStart" => "SessionStart",
        "sessionEnd" => "SessionEnd",
        "beforeSubmitPrompt" => "UserPromptSubmit",
        "preToolUse" | "beforeShellExecution" => "PreToolUse",
        "afterShellExecution" | "afterFileEdit" | "postToolUse" => "PostToolUse",
        "postToolUseFailure" => "PostToolUseFailure",
        "stop" => "Stop",
        other => other,
    };
    let mut out = payload.clone();
    out.insert("hook_event_name".into(), canonical.into());
    let conversation = payload
        .get("conversation_id")
        .or_else(|| payload.get("conversationId"))
        .and_then(Value::as_str)
        .unwrap_or("");
    out.insert("session_id".into(), conversation.into());
    out.insert("agent_origin".into(), "Cursor".into());
    out.insert("agent_origin_kind".into(), "cursor_app".into());
    let roots = payload.get("workspace_roots").or_else(|| payload.get("workspaceRoots"));
    if let Some(first) = roots.and_then(Value::as_array).and_then(|r| r.first()).and_then(Value::as_str) {
        out.insert("cwd".into(), first.into());
    } else if let Some(root) = payload.get("workspace_root").and_then(Value::as_str) {
        out.insert("cwd".into(), root.into());
    }
    match event_name {
        "beforeShellExecution" | "preToolUse" => {
            let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or("shell");
            out.insert("tool_name".into(), tool.into());
            let input = payload
                .get("tool_input")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"command": payload.get("command")}));
            out.insert("tool_input".into(), input);
        }
        "afterShellExecution" | "afterFileEdit" | "postToolUse" | "postToolUseFailure" => {
            let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or(event_name);
            out.insert("tool_name".into(), tool.into());
            out.insert(
                "tool_response".into(),
                serde_json::json!({
                    "status": payload.get("status"),
                    "exit_code": payload.get("exit_code"),
                    "error": payload.get("error_message"),
                }),
            );
        }
        _ => {}
    }
    if event_name == "postToolUseFailure" && let Some(error) = payload.get("error_message").and_then(Value::as_str) {
        out.insert("message".into(), error.into());
    }
    if event_name == "stop" && let Some(status) = payload.get("status").and_then(Value::as_str) {
        out.insert("message".into(), format!("Cursor stopped: {status}").into());
    }
    out
}

fn codex_internal(event: &Event) -> bool {
    if event.provider != "codex" {
        return false;
    }
    let raw = &event.raw;
    if event.name == "UserPromptSubmit"
        && raw.get("transcript_path").is_none_or(Value::is_null)
        && str_field(raw, "prompt").is_some_and(|p| {
            p.lines().next() == Some("This is an app-generated request for a suggested next user message. The user did not write this message.")
        })
    {
        return true;
    }
    if codex_memory_consolidation(event) {
        return true;
    }
    let text = ["prompt", "message", "last_assistant_message"]
        .iter()
        .filter_map(|k| str_field(raw, k))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    ["generate 0 to 3 hyperpersonalized suggestions", "you are an expert at upholding safety and compliance standards"]
        .iter()
        .any(|p| text.contains(p))
}

fn codex_memory_consolidation(event: &Event) -> bool {
    let (Some(session), Some(path)) = (event.session_id.as_deref(), str_field(&event.raw, "transcript_path")) else {
        return false;
    };
    let Ok(file) = File::open(path) else { return false };
    let mut header = String::new();
    if file.take(256 * 1024).read_to_string(&mut header).is_err() && header.is_empty() {
        return false;
    }
    let Some(first) = header.lines().next() else { return false };
    let Ok(row) = serde_json::from_str::<Value>(first) else { return false };
    if row.get("type").and_then(Value::as_str) != Some("session_meta") {
        return false;
    }
    let payload = &row["payload"];
    let source = &payload["source"];
    (source["internal"] == "memory_consolidation" || source["subagent"] == "memory_consolidation")
        && payload["id"].as_str() == Some(session)
}

fn codex_session_title(session_id: &str) -> Option<String> {
    let path = paths::home().join(".codex").join("session_index.jsonl");
    let tail = read_tail(&path, 512 * 1024)?;
    tail.lines().rev().find_map(|line| {
        let row: Value = serde_json::from_str(line).ok()?;
        (row.get("id")?.as_str()? == session_id)
            .then(|| row.get("thread_name")?.as_str().map(|t| status::truncate(t.trim(), 72)))
            .flatten()
    })
}

fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(if start > 0 { text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default() } else { text })
}

fn detect_origin(provider: &str, raw: &Map<String, Value>, env: &HashMap<String, String>) -> Option<(String, String)> {
    if let Some(label) = str_field(raw, "agent_origin") {
        let kind = str_field(raw, "agent_origin_kind").unwrap_or("explicit");
        return Some((label.to_string(), kind.to_string()));
    }
    if let Some(label) = env.get("SIDEPULSE_AGENT_ORIGIN").filter(|l| !l.trim().is_empty()) {
        let kind = env.get("SIDEPULSE_AGENT_ORIGIN_KIND").cloned().unwrap_or_else(|| "explicit".into());
        return Some((label.trim().to_string(), kind));
    }
    let term = env.get("TERM_PROGRAM").map(|t| t.to_lowercase()).unwrap_or_default();
    if term == "vscode" || env.keys().any(|k| k.starts_with("VSCODE_")) {
        return Some(surface(provider, "vscode"));
    }
    let bundle = env.get("__CFBundleIdentifier").map(|b| b.to_lowercase()).unwrap_or_default();
    if (provider == "claude" && bundle.contains("anthropic"))
        || (provider == "codex" && ["openai", "chatgpt", "codex"].iter().any(|p| bundle.contains(p)))
    {
        return Some(surface(provider, "app"));
    }
    if let Some(origin) = origin_from_ancestry(provider) {
        return Some(origin);
    }
    if !term.is_empty() {
        return Some(surface(provider, "cli"));
    }
    None
}

fn surface(provider: &str, surface: &str) -> (String, String) {
    let label = match (provider, surface) {
        ("claude", "app") => "Claude App".into(),
        ("claude", "cli") => "Claude Code CLI".into(),
        ("codex", "app") => "Codex UI".into(),
        ("codex", "cli") => "Codex CLI".into(),
        (_, "vscode") => format!("{} in VS Code", status::provider_label(provider)),
        (_, "cursor") => format!("{} in Cursor", status::provider_label(provider)),
        (_, "windsurf") => format!("{} in Windsurf", status::provider_label(provider)),
        _ => status::provider_label(provider),
    };
    (label, format!("{provider}_{surface}"))
}

fn origin_from_ancestry(provider: &str) -> Option<(String, String)> {
    let paths = process_ancestry(10);
    let haystack = paths.join("\n").to_lowercase();
    if ["visual studio code.app", "code helper"].iter().any(|t| haystack.contains(t)) {
        return Some(surface(provider, "vscode"));
    }
    if haystack.contains("cursor.app") || haystack.contains("cursor helper") {
        return Some(surface(provider, "cursor"));
    }
    if haystack.contains("windsurf.app") {
        return Some(surface(provider, "windsurf"));
    }
    if provider == "claude" && haystack.contains("claude.app") {
        return Some(surface(provider, "app"));
    }
    if provider == "codex" && (haystack.contains("codex.app") || haystack.contains("chatgpt.app")) {
        return Some(surface(provider, "app"));
    }
    let cli = paths.iter().any(|p| {
        let base = Path::new(p).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        match provider {
            "claude" => base == "claude" || base == "claude-code",
            other => base == other,
        }
    });
    cli.then(|| surface(provider, "cli"))
}

#[cfg(target_os = "macos")]
fn process_ancestry(limit: usize) -> Vec<String> {
    let mut paths = Vec::new();
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..limit {
        if pid <= 1 {
            break;
        }
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if len > 0 {
            paths.push(String::from_utf8_lossy(&buffer[..len as usize]).into_owned());
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if got != size {
            break;
        }
        pid = info.pbi_ppid as i32;
    }
    paths
}

#[cfg(not(target_os = "macos"))]
fn process_ancestry(_limit: usize) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(provider: &str, event: Option<&str>, json: &str) -> Option<Report> {
        build_report(provider, event, json, &HashMap::new())
    }

    #[test]
    fn claude_prompt_sets_title_and_working() {
        let r = report(
            "claude",
            None,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"abc","cwd":"/tmp/x","prompt":"Rewrite it in Rust"}"#,
        )
        .unwrap();
        assert_eq!(r.mode, Some(Mode::Working));
        assert_eq!(r.scope, "claude:abc");
        assert_eq!(r.title.as_deref(), Some("Rewrite it in Rust"));
        assert!(!r.unless_done);
    }

    #[test]
    fn permission_request_is_a_permission_wait() {
        let r = report("claude", None, r#"{"hook_event_name":"PermissionRequest","session_id":"abc","tool_name":"Bash"}"#).unwrap();
        assert_eq!(r.mode, Some(Mode::WaitingForInput));
        assert_eq!(r.kind.as_deref(), Some("permission"));
    }

    #[test]
    fn ask_user_question_uses_question_text() {
        let r = report(
            "claude",
            None,
            r#"{"hook_event_name":"PermissionRequest","session_id":"a","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Pick  one?"}]}}"#,
        )
        .unwrap();
        assert_eq!(r.message.as_deref(), Some("Pick one?"));
    }

    #[test]
    fn notification_is_guarded_by_done() {
        let r = report("claude", None, r#"{"hook_event_name":"Notification","session_id":"a","message":"Claude is waiting for your input"}"#).unwrap();
        assert!(r.unless_done);
        assert_eq!(r.mode, Some(Mode::WaitingForInput));
    }

    #[test]
    fn cursor_events_are_normalized() {
        let r = report("cursor", Some("beforeShellExecution"), r#"{"conversation_id":"c1","command":"ls","workspace_roots":["/w"]}"#).unwrap();
        assert_eq!(r.mode, Some(Mode::ToolRunning));
        assert_eq!(r.scope, "cursor:c1");
        assert_eq!(r.cwd.as_deref(), Some("/w"));
        assert_eq!(r.origin.as_deref(), Some("Cursor"));
    }

    #[test]
    fn codex_prediction_is_suppressed() {
        let r = report(
            "codex",
            None,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"This is an app-generated request for a suggested next user message. The user did not write this message.\nmore"}"#,
        )
        .unwrap();
        assert!(r.suppress);
    }

    #[test]
    fn codex_tools_preserve_questions() {
        let r = report("codex", None, r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"shell"}"#).unwrap();
        assert!(r.unless_question);
    }

    #[test]
    fn garbage_input_is_ignored() {
        assert!(report("claude", None, "not json").is_none());
        assert!(report("claude", None, r#"{"hook_event_name":"Bogus"}"#).is_none());
    }
}
