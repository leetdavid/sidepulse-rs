use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    IdleReady,
    Working,
    ToolRunning,
    WaitingForInput,
    LongTaskProgress,
    BlockedError,
    Completed,
    Unknown,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::IdleReady => "idle_ready",
            Mode::Working => "working",
            Mode::ToolRunning => "tool_running",
            Mode::WaitingForInput => "waiting_for_input",
            Mode::LongTaskProgress => "long_task_progress",
            Mode::BlockedError => "blocked_error",
            Mode::Completed => "completed",
            Mode::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::IdleReady => "Idle",
            Mode::Working => "Working",
            Mode::ToolRunning => "Tool Running",
            Mode::WaitingForInput => "Waiting for Input",
            Mode::LongTaskProgress => "Long Task",
            Mode::BlockedError => "Blocked / Error",
            Mode::Completed => "Done",
            Mode::Unknown => "Unknown",
        }
    }

    pub fn priority(self) -> u8 {
        match self {
            Mode::BlockedError => 1,
            Mode::WaitingForInput => 2,
            Mode::ToolRunning => 3,
            Mode::LongTaskProgress => 4,
            Mode::Working => 5,
            Mode::Completed => 6,
            Mode::IdleReady => 7,
            Mode::Unknown => 99,
        }
    }

    pub fn is_active(self) -> bool {
        !matches!(self, Mode::Completed | Mode::IdleReady)
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub provider: String,
    pub name: String,
    pub raw: Map<String, Value>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub message: Option<String>,
}

const KNOWN_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionDenied",
    "Notification",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "Interrupt",
    "PreInvocation",
];

pub fn canonical_event_name(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    if KNOWN_EVENTS.contains(&text) {
        return Some(text.to_string());
    }
    let normalized = snake_case(text);
    if normalized == "subagent_end" {
        return Some("SubagentStop".into());
    }
    KNOWN_EVENTS
        .iter()
        .find(|known| snake_case(known) == normalized)
        .map(|known| known.to_string())
}

fn snake_case(text: &str) -> String {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && prev_lower_or_digit {
                out.push('_');
            }
            prev_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
            out.push(c.to_ascii_lowercase());
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_lower_or_digit = false;
        }
    }
    out.trim_matches('_').to_string()
}

const ALIASES: &[(&str, &str)] = &[
    ("sessionId", "session_id"),
    ("projectPath", "project_path"),
    ("turnId", "turn_id"),
    ("agentId", "agent_id"),
    ("workspaceRoot", "cwd"),
    ("toolName", "tool_name"),
    ("toolInput", "tool_input"),
    ("toolUseId", "tool_use_id"),
    ("toolResponse", "tool_response"),
    ("lastAssistantMessage", "last_assistant_message"),
    ("notificationType", "notification_type"),
    ("agentOrigin", "agent_origin"),
];

pub fn str_field<'a>(raw: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    raw.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn first_string(raw: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match raw.get(*key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

impl Event {
    pub fn parse(provider: &str, mut raw: Map<String, Value>) -> Option<Self> {
        let name = ["hook_event_name", "hookEventName", "event_name", "eventName"]
            .iter()
            .find_map(|key| raw.get(*key).and_then(Value::as_str))
            .and_then(canonical_event_name)?;
        for (source, target) in ALIASES {
            if !raw.contains_key(*target) && let Some(value) = raw.get(*source).cloned() {
                raw.insert((*target).to_string(), value);
            }
        }
        raw.entry("hook_event_name").or_insert_with(|| Value::String(name.clone()));
        Some(Self {
            provider: provider.to_string(),
            session_id: first_string(&raw, &["session_id"]),
            agent_id: first_string(&raw, &["agent_id"]),
            cwd: first_string(&raw, &["cwd"]),
            tool_name: first_string(&raw, &["tool_name"]),
            message: first_string(&raw, &["message", "last_assistant_message", "error_details"]),
            name,
            raw,
        })
    }

    pub fn status_key(&self) -> String {
        if let Some(agent) = &self.agent_id {
            format!("{}:agent:{agent}", self.provider)
        } else if let Some(session) = &self.session_id {
            format!("{}:session:{session}", self.provider)
        } else {
            format!("{}:unknown", self.provider)
        }
    }

    pub fn codex_question_tool(&self) -> Option<&str> {
        if self.provider != "codex" {
            return None;
        }
        let name = self.tool_name.as_deref()?.rsplit('.').next()?;
        matches!(name, "request_user_input" | "request_user_input_async").then_some(name)
    }
}

pub fn mode_for_event(event: &Event) -> Option<Mode> {
    let name = event.name.as_str();
    if name == "Interrupt" {
        return Some(Mode::IdleReady);
    }
    if let Some(mode) = explicit_mode(event) {
        return Some(mode);
    }
    let codex = event.provider == "codex";
    match name {
        "PostToolUseFailure" | "PermissionDenied" if codex => Some(Mode::Working),
        "PostToolUseFailure" | "PermissionDenied" | "StopFailure" => Some(Mode::BlockedError),
        "PermissionRequest" => Some(Mode::WaitingForInput),
        "Notification" => {
            let kind = str_field(&event.raw, "notification_type").unwrap_or("").trim().to_lowercase();
            let message = str_field(&event.raw, "message").unwrap_or("").trim().to_lowercase();
            if notification_indicates_completion(&kind, &message) {
                Some(Mode::Completed)
            } else if notification_indicates_input(&format!("{kind} {message}")) {
                Some(Mode::WaitingForInput)
            } else {
                Some(Mode::Working)
            }
        }
        "PreToolUse" => {
            if event.codex_question_tool().is_some() {
                Some(Mode::WaitingForInput)
            } else {
                Some(Mode::ToolRunning)
            }
        }
        "PostToolUse" => {
            if !codex && tool_response_failed(event.raw.get("tool_response")) {
                return Some(Mode::BlockedError);
            }
            if event.codex_question_tool() == Some("request_user_input_async") && async_question_accepted(event) {
                return Some(Mode::WaitingForInput);
            }
            Some(Mode::Working)
        }
        "UserPromptSubmit" | "PreInvocation" | "PreCompact" | "PostCompact" | "SubagentStart" => Some(Mode::Working),
        "Stop" | "SubagentStop" => {
            if assistant_asks_question(str_field(&event.raw, "last_assistant_message")) {
                Some(Mode::WaitingForInput)
            } else {
                Some(Mode::Completed)
            }
        }
        "SessionEnd" => Some(Mode::Completed),
        "SessionStart" => Some(Mode::IdleReady),
        _ => None,
    }
}

fn async_question_accepted(event: &Event) -> bool {
    let response = match event.raw.get("tool_response") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        other => other.cloned(),
    };
    response.and_then(|r| r.get("accepted").and_then(Value::as_bool)) == Some(true)
}

pub fn explicit_mode(event: &Event) -> Option<Mode> {
    for key in ["sidepulse_status", "sidepulse_mode"] {
        if let Some(mode) = event.raw.get(key).and_then(Value::as_str).and_then(mode_from_word) {
            return Some(mode);
        }
    }
    let message = str_field(&event.raw, "last_assistant_message").or_else(|| str_field(&event.raw, "message"))?;
    explicit_mode_from_message(message)
}

fn mode_from_word(value: &str) -> Option<Mode> {
    let mut normalized = String::new();
    for c in value.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            normalized.push(c);
        } else if !normalized.ends_with('_') {
            normalized.push('_');
        }
    }
    match normalized.trim_matches('_') {
        "ask" | "question" | "waiting" | "waiting_for_input" | "input" => Some(Mode::WaitingForInput),
        "blocked" | "error" | "blocked_error" => Some(Mode::BlockedError),
        "working" => Some(Mode::Working),
        "tool_running" => Some(Mode::ToolRunning),
        "progress" | "long_task_progress" => Some(Mode::LongTaskProgress),
        "done" | "complete" | "completed" => Some(Mode::Completed),
        "idle" | "ready" | "idle_ready" => Some(Mode::IdleReady),
        _ => None,
    }
}

fn explicit_mode_from_message(message: &str) -> Option<Mode> {
    let text = strip_code_blocks(message);
    for line in text.lines() {
        let lowered = line.trim().to_lowercase();
        let value = if let Some(inner) = lowered.strip_prefix("<!--").and_then(|l| l.strip_suffix("-->")) {
            let inner = inner.trim();
            ["sidepulse", "agent-monitor", "agent_monitor", "agent monitor"]
                .iter()
                .find_map(|prefix| inner.strip_prefix(prefix))
                .and_then(|rest| {
                    let rest = rest.trim_start();
                    rest.strip_prefix(':')
                        .or_else(|| rest.strip_prefix("status").and_then(|r| r.trim_start().strip_prefix(':')))
                        .or_else(|| rest.strip_prefix("mode").and_then(|r| r.trim_start().strip_prefix(':')))
                })
        } else if let Some(inner) = lowered.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            ["sidepulse", "agent-monitor", "agent_monitor", "agent monitor"]
                .iter()
                .find_map(|prefix| inner.strip_prefix(prefix))
                .and_then(|rest| {
                    let rest = rest.trim_start();
                    rest.strip_prefix("status")
                        .or_else(|| rest.strip_prefix("mode"))
                        .and_then(|r| r.trim_start().strip_prefix(':'))
                })
        } else {
            None
        };
        if let Some(mode) = value.and_then(mode_from_word) {
            return Some(mode);
        }
    }
    None
}

fn notification_indicates_completion(kind: &str, message: &str) -> bool {
    let text = format!("{kind} {message}");
    let text = text.trim();
    const PHRASES: &[&str] = &[
        "turn complete",
        "turn completed",
        "task complete",
        "task completed",
        "completed successfully",
        "work complete",
        "work completed",
    ];
    PHRASES.iter().any(|p| text.contains(p))
        || (kind == "idle_prompt" && matches!(message, "done" | "complete" | "completed"))
}

fn notification_indicates_input(text: &str) -> bool {
    const PHRASES: &[&str] = &[
        "waiting for your input",
        "waiting for input",
        "needs your input",
        "needs input",
        "permission",
        "approval",
        "confirm",
    ];
    PHRASES.iter().any(|p| text.contains(p))
}

fn tool_response_failed(response: Option<&Value>) -> bool {
    match response {
        Some(Value::Object(map)) => {
            map.get("interrupted") == Some(&Value::Bool(true))
                || map.get("success") == Some(&Value::Bool(false))
                || matches!(map.get("exit_code"), Some(code) if !code.is_null() && code.as_i64() != Some(0))
        }
        Some(Value::String(text)) => {
            let text = text.to_lowercase();
            text.contains("exit code: 1") || text.contains("traceback")
        }
        _ => false,
    }
}

pub fn strip_code_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        out.push_str(&rest[..start]);
        match rest[start + 3..].find("```") {
            Some(end) => rest = &rest[start + 3 + end + 3..],
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn strip_inline_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            match rest[start + 1..].find('`') {
                Some(end) if !rest[start + 1..start + 1 + end].contains('\n') => {
                    out.push_str(&rest[..start]);
                    rest = &rest[start + 1 + end + 1..];
                }
                _ => break,
            }
        }
        out.push_str(rest);
    }
    out
}

pub fn assistant_asks_question(message: Option<&str>) -> bool {
    let Some(message) = message else { return false };
    let text = strip_inline_code(&strip_code_blocks(message));
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(8)..];
    tail.iter().rev().filter(|line| !is_status_line(line)).any(|line| line_asks_question(line))
}

fn is_status_line(line: &str) -> bool {
    let lowered = line.to_lowercase();
    ["* cogitated ", "* recap:", "※ recap:", "recap:"].iter().any(|p| lowered.starts_with(p))
}

const QUESTION_PREFIXES: &[&str] = &[
    "which ",
    "what ",
    "where ",
    "when ",
    "who ",
    "why ",
    "how ",
    "can you ",
    "could you ",
    "please confirm",
    "please choose",
    "choose ",
    "need me to ",
    "want me to ",
    "should i ",
    "should we ",
    "do you want me to ",
];

const REQUEST_PREFIXES: &[&str] = &[
    "please confirm",
    "please choose",
    "choose ",
    "need me to ",
    "want me to ",
    "should i ",
    "should we ",
    "do you want me to ",
];

const CASUAL_CLOSINGS: &[&str] = &[
    "anything else",
    "any other",
    "all good",
    "need anything else",
    "want anything else",
    "anything you want",
    "anything you'd like",
];

fn line_asks_question(line: &str) -> bool {
    let lowered = line.to_lowercase();
    if line.ends_with(':') || CASUAL_CLOSINGS.iter().any(|p| lowered.starts_with(p)) {
        return false;
    }
    if contains_offer(&lowered) {
        return true;
    }
    if line.ends_with('?') {
        return QUESTION_PREFIXES.iter().any(|p| lowered.starts_with(p));
    }
    REQUEST_PREFIXES.iter().any(|p| lowered.starts_with(p))
}

fn contains_offer(lowered: &str) -> bool {
    const OFFERS: &[&str] = &["want me to", "need me to", "should i", "should we", "do you want me to"];
    OFFERS.iter().any(|offer| {
        lowered.match_indices(offer).any(|(index, _)| {
            let end = index + offer.len();
            let word_ends = lowered[end..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let before = &lowered[..index];
            let starts_sentence = before.is_empty() || {
                let trimmed = before.trim_end();
                trimmed.len() < before.len() && trimmed.ends_with(['.', '!', '?'])
            };
            word_ends && starts_sentence
        })
    })
}

pub fn summarize_prompt(prompt: &str, max_len: usize) -> Option<String> {
    let text = prompt.trim();
    if text.is_empty() || text.starts_with("<task-notification>") {
        return None;
    }
    let text = after_request_marker(text).unwrap_or(text);
    let text = replace_code_blocks(text, " ");
    let text = unwrap_inline_code(&text);
    let text = scrub_quoted_paths(&text);
    let text = scrub_paths(&text);
    let text = replace_tags(&text);
    let text = replace_hashes(&text);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.trim_matches(|c: char| matches!(c, ' ' | '-' | ':' | '\n' | '\t'));
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate(collapsed, max_len))
}

fn after_request_marker(text: &str) -> Option<&str> {
    let lowered = text.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lowered[search..].find("##") {
        let start = search + found;
        let rest = &lowered[start + 2..];
        let spaces = rest.len() - rest.trim_start().len();
        if spaces > 0 && rest[spaces..].starts_with("my request for ") {
            let label_start = start + 2 + spaces + "my request for ".len();
            let label_len = lowered[label_start..].find([':', '\n']).unwrap_or(0);
            if label_len > 0 && lowered[label_start + label_len..].starts_with(':') {
                return Some(text[label_start + label_len + 1..].trim_start());
            }
        }
        search = start + 2;
    }
    None
}

fn replace_code_blocks(text: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let Some(end) = rest[start + 3..].find("```") else { break };
        out.push_str(&rest[..start]);
        out.push_str(with);
        rest = &rest[start + 3 + end + 3..];
    }
    out.push_str(rest);
    out
}

fn unwrap_inline_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        match rest[start + 1..].find('`') {
            Some(len) if len > 0 => {
                out.push_str(&rest[..start]);
                out.push_str(&rest[start + 1..start + 1 + len]);
                rest = &rest[start + 1 + len + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

const PATH_PREFIXES: &[&str] = &["~", "/Users", "/var", "/private", "/tmp"];

fn path_prefix_len(text: &str) -> Option<usize> {
    PATH_PREFIXES.iter().find(|p| text.starts_with(**p)).map(|p| p.len())
}

fn scrub_quoted_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        let quote = rest.chars().next().unwrap_or(' ');
        if matches!(quote, '\'' | '"')
            && let Some(prefix) = path_prefix_len(&rest[1..])
        {
            let body = &rest[1 + prefix..];
            let stop = body.find(['\'', '"']);
            if let Some(stop) = stop.filter(|s| *s > 0 && body[*s..].starts_with(quote)) {
                out.push(quote);
                out.push_str("...");
                out.push(quote);
                index += 1 + prefix + stop + 1;
                continue;
            }
        }
        out.push(quote);
        index += quote.len_utf8();
    }
    out
}

fn scrub_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        if let Some(prefix) = path_prefix_len(rest)
            && rest[prefix..].starts_with('/')
        {
            let body = &rest[prefix + 1..];
            let len = body.find(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ')' | '\'' | '"' | '`')).unwrap_or(body.len());
            if len > 0 {
                out.push_str("...");
                index += prefix + 1 + len;
                continue;
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        index += c.len_utf8();
    }
    out
}

fn replace_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        match rest[start + 1..].find('>') {
            Some(len) if len > 0 => {
                out.push_str(&rest[..start]);
                out.push(' ');
                rest = &rest[start + 1 + len + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn replace_hashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            while chars.peek().is_some_and(|n| *n == '#' || n.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let trimmed: Vec<char> = text.chars().take(max_chars.saturating_sub(1)).collect::<String>().trim_end().chars().collect();
    let boundary = trimmed.iter().rposition(|c| matches!(c, ' ' | ',' | ';'));
    let cut = match boundary {
        Some(index) if index >= max_chars / 2 => index,
        _ => trimmed.len(),
    };
    let mut out: String = trimmed[..cut].iter().collect::<String>().trim_end().to_string();
    out.push_str("...");
    out
}

pub fn clean_text(value: &str, limit: usize) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if (c as u32) < 32 || (127..=159).contains(&(c as u32)) { ' ' } else { c })
        .collect();
    if cleaned.len() <= limit {
        return cleaned;
    }
    let mut end = limit;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].to_string()
}

pub fn project_name(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let path = Path::new(cwd);
    for candidate in path.ancestors() {
        if candidate.join(".git").exists() {
            return candidate.file_name().map(|n| n.to_string_lossy().into_owned()).or_else(|| Some(cwd.into()));
        }
    }
    path.file_name().map(|n| n.to_string_lossy().into_owned()).or_else(|| Some(cwd.into()))
}

pub fn display_name(project: Option<&str>, title: Option<&str>, short_id: &str, fallback: &str) -> String {
    let suffix = if short_id.is_empty() { String::new() } else { format!(" ({short_id})") };
    let normalized = |s: &str| s.replace(['_', '-'], " ").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let name = match (project, title) {
        (Some(p), Some(t)) if normalized(p) == normalized(t) => format!("{t}{suffix}"),
        (Some(p), Some(t)) => format!("{p}: {t}{suffix}"),
        (None, Some(t)) => format!("{t}{suffix}"),
        (Some(p), None) => format!("{p}{suffix}"),
        (None, None) => return fallback.to_string(),
    };
    truncate(&name, 96)
}

pub fn provider_label(provider: &str) -> String {
    match provider {
        "codex" => "Codex".into(),
        "claude" => "Claude".into(),
        "grok" => "Grok".into(),
        "opencode" => "opencode".into(),
        other => {
            let mut chars = other.chars();
            chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(provider: &str, value: Value) -> Event {
        Event::parse(provider, value.as_object().unwrap().clone()).unwrap()
    }

    #[test]
    fn canonicalizes_event_names() {
        assert_eq!(canonical_event_name("pre_tool_use").as_deref(), Some("PreToolUse"));
        assert_eq!(canonical_event_name("preToolUse").as_deref(), Some("PreToolUse"));
        assert_eq!(canonical_event_name("subagent-end").as_deref(), Some("SubagentStop"));
        assert_eq!(canonical_event_name("Stop").as_deref(), Some("Stop"));
        assert_eq!(canonical_event_name("whatever"), None);
    }

    #[test]
    fn maps_claude_events() {
        let e = |v: Value| mode_for_event(&event("claude", v));
        assert_eq!(e(json!({"hook_event_name": "UserPromptSubmit", "prompt": "hi"})), Some(Mode::Working));
        assert_eq!(e(json!({"hook_event_name": "PreToolUse", "tool_name": "Bash"})), Some(Mode::ToolRunning));
        assert_eq!(e(json!({"hook_event_name": "PostToolUse", "tool_response": {"exit_code": 2}})), Some(Mode::BlockedError));
        assert_eq!(e(json!({"hook_event_name": "PostToolUse", "tool_response": {"stdout": "ok"}})), Some(Mode::Working));
        assert_eq!(e(json!({"hook_event_name": "PermissionRequest"})), Some(Mode::WaitingForInput));
        assert_eq!(e(json!({"hook_event_name": "Stop", "last_assistant_message": "All done."})), Some(Mode::Completed));
        assert_eq!(
            e(json!({"hook_event_name": "Stop", "last_assistant_message": "Fixed it.\nWant me to also add tests?"})),
            Some(Mode::WaitingForInput)
        );
        assert_eq!(
            e(json!({"hook_event_name": "Stop", "last_assistant_message": "Anything else you need?"})),
            Some(Mode::Completed)
        );
        assert_eq!(
            e(json!({"hook_event_name": "Notification", "message": "Claude is waiting for your input", "notification_type": "idle_prompt"})),
            Some(Mode::WaitingForInput)
        );
        assert_eq!(e(json!({"hook_event_name": "SessionStart"})), Some(Mode::IdleReady));
        assert_eq!(e(json!({"hook_event_name": "Stop", "last_assistant_message": "x\n<!-- sidepulse: error -->"})), Some(Mode::BlockedError));
    }

    #[test]
    fn maps_codex_specifics() {
        let e = |v: Value| mode_for_event(&event("codex", v));
        assert_eq!(e(json!({"hook_event_name": "PostToolUseFailure"})), Some(Mode::Working));
        assert_eq!(e(json!({"hook_event_name": "PreToolUse", "tool_name": "functions.request_user_input"})), Some(Mode::WaitingForInput));
        assert_eq!(e(json!({"hook_event_name": "PostToolUse", "tool_response": {"exit_code": 1}})), Some(Mode::Working));
    }

    #[test]
    fn summarizes_prompts() {
        assert_eq!(
            summarize_prompt("Fix the bug in `/Users/me/x.rs` please\n```\ncode\n```", 72).as_deref(),
            Some("Fix the bug in ... please")
        );
        assert_eq!(summarize_prompt("<task-notification>x", 72), None);
        assert_eq!(summarize_prompt("## Title\nDo <b>this</b>", 72).as_deref(), Some("Title Do this"));
        assert_eq!(summarize_prompt(&"a".repeat(100), 10).unwrap(), "aaaaaaaaa...");
    }

    #[test]
    fn builds_display_names() {
        assert_eq!(display_name(Some("app"), Some("Fix it"), "abcd1234", ""), "app: Fix it (abcd1234)");
        assert_eq!(display_name(Some("my-app"), Some("My App"), "", ""), "My App");
        assert_eq!(display_name(None, None, "x", "Claude session x"), "Claude session x");
    }
}
