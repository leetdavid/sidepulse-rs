use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::protocol::{HerdrAgent, Report};
use crate::status::{self, Mode};

pub const MAX_SESSIONS: usize = 64;
pub const DONE_VISIBLE_MS: u64 = 20 * 60 * 1000;
const HERDR_PREFIX: &str = "herdr:";
const RECENT_REQUESTS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub scope: String,
    pub provider: String,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub event: String,
    pub updated_ms: u64,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_socket: Option<String>,
}

impl Session {
    pub fn is_herdr_only(&self) -> bool {
        self.scope.starts_with(HERDR_PREFIX)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub mode: Mode,
    pub active_count: usize,
    pub sessions: Vec<Session>,
    pub recent: Vec<Session>,
}

#[derive(Default)]
pub struct Store {
    sessions: HashMap<String, Session>,
    suppressed: HashSet<String>,
    project_names: HashMap<String, Option<String>>,
    requests: VecDeque<String>,
}

impl Store {
    pub fn load(path: &Path) -> Self {
        let mut store = Self::default();
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(sessions) = serde_json::from_str::<Vec<Session>>(&text)
        {
            store.sessions = sessions.into_iter().map(|s| (s.scope.clone(), s)).collect();
        }
        store
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let sessions: Vec<&Session> = self.sessions.values().collect();
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(&sessions)?)?;
        fs::rename(tmp, path)
    }

    pub fn submit(&mut self, report: Report, now_ms: u64) -> bool {
        if let Some(id) = &report.request_id {
            if self.requests.contains(id) {
                return false;
            }
            self.requests.push_back(id.clone());
            if self.requests.len() > RECENT_REQUESTS {
                self.requests.pop_front();
            }
        }
        if report.clear {
            return self.sessions.remove(&report.scope).is_some();
        }
        if report.suppress || self.suppressed.contains(&report.scope) {
            self.suppressed.insert(report.scope.clone());
            return self.sessions.remove(&report.scope).is_some();
        }
        let Some(mode) = report.mode else { return false };
        let previous = self
            .sessions
            .get(&report.scope)
            .filter(|p| !(p.mode == Mode::Completed && now_ms.saturating_sub(p.updated_ms) > DONE_VISIBLE_MS));

        if let Some(prev) = previous {
            let recently_done = prev.mode == Mode::Completed;
            if report.unless_done && recently_done {
                return false;
            }
            if report.unless_question && prev.mode == Mode::WaitingForInput && prev.kind.as_deref() == Some("question") {
                return false;
            }
        }

        let inherit = |field: Option<String>, old: Option<&String>| field.or_else(|| old.cloned());
        let title = match previous.and_then(|p| p.title.as_ref()) {
            Some(old) if !report.title_from_session => Some(old.clone()),
            old => inherit(report.title, old),
        };
        let cwd = inherit(report.cwd, previous.and_then(|p| p.cwd.as_ref()));
        let origin = inherit(report.origin, previous.and_then(|p| p.origin.as_ref()));
        let origin_kind = inherit(report.origin_kind, previous.and_then(|p| p.origin_kind.as_ref()));
        let terminal = inherit(report.terminal, previous.and_then(|p| p.terminal.as_ref()));
        let session_id = inherit(report.session_id, previous.and_then(|p| p.session_id.as_ref()));
        let herdr_pane = inherit(report.herdr_pane, previous.and_then(|p| p.herdr_pane.as_ref()));
        let herdr_socket = inherit(report.herdr_socket, previous.and_then(|p| p.herdr_socket.as_ref()));

        let short_id: String = report.agent_id.as_deref().or(session_id.as_deref()).unwrap_or("").chars().take(8).collect();
        let fallback = if short_id.is_empty() {
            status::provider_label(&report.provider)
        } else {
            format!("{} session {short_id}", status::provider_label(&report.provider))
        };
        let project = cwd.as_deref().and_then(|c| self.project_name(c));
        let display_name = status::display_name(project.as_deref(), title.as_deref(), &short_id, &fallback);

        let session = Session {
            scope: report.scope.clone(),
            provider: report.provider,
            mode,
            kind: report.kind,
            event: report.event,
            updated_ms: now_ms,
            display_name,
            session_id,
            agent_id: report.agent_id,
            cwd,
            title,
            tool_name: report.tool_name,
            message: report.message,
            origin,
            origin_kind,
            terminal,
            herdr_pane,
            herdr_socket,
        };
        let duplicate_herdr = self
            .sessions
            .values()
            .filter(|s| s.is_herdr_only() && s.provider == session.provider && s.session_id.is_some() && s.session_id == session.session_id)
            .map(|s| s.scope.clone())
            .collect::<Vec<_>>();
        for scope in duplicate_herdr {
            self.sessions.remove(&scope);
        }
        self.sessions.insert(report.scope, session);
        self.evict();
        true
    }

    pub fn herdr_sync(&mut self, socket: &str, agents: Vec<HerdrAgent>, now_ms: u64) -> bool {
        let mut changed = false;
        let mut live = HashSet::new();
        for agent in agents {
            let hooked = agent.session_id.as_ref().and_then(|sid| {
                self.sessions.values_mut().find(|s| !s.is_herdr_only() && s.provider == agent.provider && s.session_id.as_ref() == Some(sid))
            });
            if let Some(session) = hooked {
                if session.herdr_pane.as_deref() != Some(agent.pane_id.as_str()) {
                    session.herdr_pane = Some(agent.pane_id.clone());
                    session.herdr_socket = Some(socket.to_string());
                    changed = true;
                }
                continue;
            }
            let scope = format!("{HERDR_PREFIX}{}", agent.terminal_id);
            live.insert(scope.clone());
            let title = agent.title.as_deref().map(|t| status::clean_text(t, 192)).filter(|t| !t.trim().is_empty());
            if let Some(existing) = self.sessions.get_mut(&scope)
                && existing.mode == agent.mode
                && existing.title == title
            {
                existing.herdr_pane = Some(agent.pane_id.clone());
                continue;
            }
            let short_id: String = agent.session_id.as_deref().unwrap_or("").chars().take(8).collect();
            let project = agent.cwd.as_deref().and_then(|c| self.project_name(c));
            let fallback = format!("{} in Herdr", status::provider_label(&agent.provider));
            let display_name = status::display_name(project.as_deref(), title.as_deref(), &short_id, &fallback);
            self.sessions.insert(
                scope.clone(),
                Session {
                    scope,
                    provider: agent.provider,
                    mode: agent.mode,
                    kind: (agent.mode == Mode::WaitingForInput).then(|| "question".into()),
                    event: "HerdrStatus".into(),
                    updated_ms: now_ms,
                    display_name,
                    session_id: agent.session_id,
                    agent_id: None,
                    cwd: agent.cwd,
                    title,
                    tool_name: None,
                    message: None,
                    origin: Some("Herdr".into()),
                    origin_kind: Some("herdr".into()),
                    terminal: None,
                    herdr_pane: Some(agent.pane_id),
                    herdr_socket: Some(socket.to_string()),
                },
            );
            changed = true;
        }
        let gone: Vec<String> = self
            .sessions
            .values()
            .filter(|s| s.is_herdr_only() && s.herdr_socket.as_deref() == Some(socket) && !live.contains(&s.scope))
            .map(|s| s.scope.clone())
            .collect();
        for scope in gone {
            self.sessions.remove(&scope);
            changed = true;
        }
        self.evict();
        changed
    }

    pub fn remove(&mut self, scope: &str) -> bool {
        self.sessions.remove(scope).is_some()
    }

    fn evict(&mut self) {
        while self.sessions.len() > MAX_SESSIONS {
            let Some(oldest) = self.sessions.values().min_by_key(|s| s.updated_ms).map(|s| s.scope.clone()) else {
                return;
            };
            self.sessions.remove(&oldest);
        }
        if self.suppressed.len() > MAX_SESSIONS * 4 {
            self.suppressed.clear();
        }
        if self.project_names.len() > MAX_SESSIONS * 4 {
            self.project_names.clear();
        }
    }

    fn project_name(&mut self, cwd: &str) -> Option<String> {
        self.project_names.entry(cwd.to_string()).or_insert_with(|| status::project_name(cwd)).clone()
    }

    pub fn prune(&mut self, now_ms: u64, timeout_ms: u64) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| now_ms.saturating_sub(s.updated_ms) <= timeout_ms);
        before != self.sessions.len()
    }

    pub fn snapshot(&self, now_ms: u64, timeout_ms: u64) -> Snapshot {
        let live: Vec<&Session> =
            self.sessions.values().filter(|s| now_ms.saturating_sub(s.updated_ms) <= timeout_ms).collect();
        let has_active = live.iter().any(|s| s.mode.is_active());
        let mut sessions = Vec::new();
        let mut recent = Vec::new();
        for session in live {
            let age = now_ms.saturating_sub(session.updated_ms);
            if session.mode != Mode::Completed || (!has_active && age <= DONE_VISIBLE_MS) {
                sessions.push(session.clone());
            } else {
                recent.push(session.clone());
            }
        }
        let order = |a: &Session, b: &Session| {
            a.mode
                .priority()
                .cmp(&b.mode.priority())
                .then(b.updated_ms.cmp(&a.updated_ms))
                .then(a.scope.cmp(&b.scope))
        };
        sessions.sort_by(order);
        recent.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        Snapshot {
            mode: sessions.first().map_or(Mode::IdleReady, |s| s.mode),
            active_count: sessions.iter().filter(|s| s.mode.is_active()).count(),
            sessions,
            recent,
        }
    }

    pub fn next_change_ms(&self, now_ms: u64, timeout_ms: u64) -> Option<u64> {
        self.sessions
            .values()
            .flat_map(|s| {
                let done_edge = (s.mode == Mode::Completed).then_some(s.updated_ms + DONE_VISIBLE_MS + 1);
                [done_edge, Some(s.updated_ms + timeout_ms + 1)]
            })
            .flatten()
            .filter(|at| *at > now_ms)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    fn report(scope: &str, mode: Mode, event: &str) -> Report {
        Report {
            scope: scope.into(),
            provider: "claude".into(),
            mode: Some(mode),
            event: event.into(),
            session_id: Some(scope.trim_start_matches("claude:").into()),
            ..Default::default()
        }
    }

    #[test]
    fn highest_priority_session_drives_aggregate() {
        let mut store = Store::default();
        store.submit(report("claude:a", Mode::Working, "UserPromptSubmit"), 1_000);
        store.submit(report("claude:b", Mode::WaitingForInput, "PermissionRequest"), 2_000);
        let snap = store.snapshot(3_000, HOUR);
        assert_eq!(snap.mode, Mode::WaitingForInput);
        assert_eq!(snap.active_count, 2);
    }

    #[test]
    fn done_is_visible_only_while_nothing_is_active() {
        let mut store = Store::default();
        store.submit(report("claude:a", Mode::Completed, "Stop"), 1_000);
        assert_eq!(store.snapshot(2_000, HOUR).mode, Mode::Completed);
        store.submit(report("claude:b", Mode::Working, "UserPromptSubmit"), 3_000);
        let snap = store.snapshot(4_000, HOUR);
        assert_eq!(snap.mode, Mode::Working);
        assert_eq!(snap.recent.len(), 1);
        store.remove("claude:b");
        assert_eq!(store.snapshot(1_000 + DONE_VISIBLE_MS + 5, HOUR).mode, Mode::IdleReady);
    }

    #[test]
    fn notification_does_not_override_recent_done() {
        let mut store = Store::default();
        store.submit(report("claude:a", Mode::Completed, "Stop"), 1_000);
        let mut notification = report("claude:a", Mode::WaitingForInput, "Notification");
        notification.unless_done = true;
        assert!(!store.submit(notification.clone(), 60_000));
        assert!(store.submit(notification, 1_000 + DONE_VISIBLE_MS + 10));
    }

    #[test]
    fn codex_tool_keeps_pending_question() {
        let mut store = Store::default();
        let mut question = report("codex:s", Mode::WaitingForInput, "PreToolUse");
        question.kind = Some("question".into());
        store.submit(question, 1_000);
        let mut tool = report("codex:s", Mode::ToolRunning, "PreToolUse");
        tool.unless_question = true;
        assert!(!store.submit(tool, 2_000));
    }

    #[test]
    fn first_prompt_title_sticks() {
        let mut store = Store::default();
        let mut first = report("claude:a", Mode::Working, "UserPromptSubmit");
        first.title = Some("First".into());
        store.submit(first, 1_000);
        let mut second = report("claude:a", Mode::Working, "UserPromptSubmit");
        second.title = Some("Second".into());
        store.submit(second, 2_000);
        assert_eq!(store.snapshot(3_000, HOUR).sessions[0].title.as_deref(), Some("First"));
    }

    #[test]
    fn stale_sessions_expire() {
        let mut store = Store::default();
        store.submit(report("claude:a", Mode::Working, "PreToolUse"), 1_000);
        assert_eq!(store.next_change_ms(2_000, HOUR), Some(1_000 + HOUR + 1));
        assert_eq!(store.snapshot(1_000 + HOUR + 1, HOUR).mode, Mode::IdleReady);
        assert!(store.prune(1_000 + HOUR + 1, HOUR));
    }

    fn herdr(terminal: &str, session: Option<&str>, mode: Mode) -> HerdrAgent {
        HerdrAgent {
            terminal_id: terminal.into(),
            pane_id: format!("w1:{terminal}"),
            provider: "claude".into(),
            mode,
            session_id: session.map(String::from),
            title: Some("Herdr title".into()),
            cwd: Some("/tmp".into()),
        }
    }

    #[test]
    fn herdr_fills_in_unhooked_agents_and_removes_closed_panes() {
        let mut store = Store::default();
        assert!(store.herdr_sync("/s", vec![herdr("t1", Some("x"), Mode::Working)], 1_000));
        assert_eq!(store.snapshot(2_000, HOUR).mode, Mode::Working);
        assert!(!store.herdr_sync("/s", vec![herdr("t1", Some("x"), Mode::Working)], 5_000));
        assert_eq!(store.snapshot(6_000, HOUR).sessions[0].updated_ms, 1_000);
        assert!(store.herdr_sync("/s", vec![], 7_000));
        assert!(store.snapshot(8_000, HOUR).sessions.is_empty());
    }

    #[test]
    fn hooks_take_precedence_over_herdr() {
        let mut store = Store::default();
        store.herdr_sync("/s", vec![herdr("t1", Some("a"), Mode::IdleReady)], 1_000);
        store.submit(report("claude:a", Mode::Working, "UserPromptSubmit"), 2_000);
        let snap = store.snapshot(3_000, HOUR);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].scope, "claude:a");
        store.herdr_sync("/s", vec![herdr("t1", Some("a"), Mode::IdleReady)], 4_000);
        let snap = store.snapshot(5_000, HOUR);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].herdr_pane.as_deref(), Some("w1:t1"));
        assert_eq!(snap.mode, Mode::Working);
    }

    #[test]
    fn repeated_request_ids_and_clears() {
        let mut store = Store::default();
        let mut first = report("opencode:s", Mode::Working, "session.execution.started");
        first.request_id = Some("opencode-1".into());
        assert!(store.submit(first.clone(), 1_000));
        assert!(!store.submit(first, 2_000));
        let mut clear = report("opencode:s", Mode::Working, "session.deleted");
        clear.clear = true;
        assert!(store.submit(clear, 3_000));
        assert!(store.snapshot(4_000, HOUR).sessions.is_empty());
    }

    #[test]
    fn suppressed_scopes_are_cleared() {
        let mut store = Store::default();
        store.submit(report("codex:s", Mode::Working, "UserPromptSubmit"), 1_000);
        let mut clear = report("codex:s", Mode::Working, "UserPromptSubmit");
        clear.suppress = true;
        assert!(store.submit(clear, 2_000));
        assert!(!store.submit(report("codex:s", Mode::Working, "PreToolUse"), 3_000));
        assert!(store.snapshot(4_000, HOUR).sessions.is_empty());
    }
}
