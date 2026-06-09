//! Per-session, mutable permission state.
//!
//! This lives inside [`AgentSession`](crate::session::AgentSession), which is
//! already passed `&mut` into every turn. Keeping the *mutable* grants here —
//! rather than on the shared, `Clone` + `&self` runner — gives per-session
//! isolation with no lock and no cross-session leak (see
//! `docs/s03/permission-system.md` §3).

use super::rule::Rule;

/// How strict the permission gate is for this session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Per-action defaults apply (reads free, writes/exec ask). The safe norm.
    #[default]
    Default,
    /// Writes are auto-allowed; reads stay free; execute still asks. Explicit
    /// `deny`/`ask` rules still win (they are checked first).
    AcceptEdits,
    /// Skip all prompts. Still honors `deny` — deny is unbypassable in normal
    /// modes (diverges from Claude Code by design; see the threat model).
    BypassPermissions,
}

impl PermissionMode {
    /// Parses a mode name from settings or a CLI flag. Accepts both the
    /// Claude-Code-style camelCase and a kebab/short form.
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim() {
            "default" => Some(Self::Default),
            "acceptEdits" | "accept-edits" => Some(Self::AcceptEdits),
            "bypassPermissions" | "bypass-permissions" | "bypass" => Some(Self::BypassPermissions),
            _ => None,
        }
    }
}

/// Session-scoped permission state: the active mode plus grants the user added
/// mid-session by choosing "Always allow/deny" at a prompt.
#[derive(Clone, Debug, Default)]
pub struct PermissionSessionState {
    mode: PermissionMode,
    allow_grants: Vec<Rule>,
    deny_grants: Vec<Rule>,
}

impl PermissionSessionState {
    /// Creates session state starting in `mode` with no grants.
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            allow_grants: Vec::new(),
            deny_grants: Vec::new(),
        }
    }

    /// The active mode.
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Switches the active mode (e.g. a future `/accept-edits` toggle).
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Session allow-grants, newest last.
    pub fn allow_grants(&self) -> &[Rule] {
        &self.allow_grants
    }

    /// Session deny-grants, newest last.
    pub fn deny_grants(&self) -> &[Rule] {
        &self.deny_grants
    }

    /// Records an "Always allow" grant for the rest of the session.
    pub fn grant_allow(&mut self, rule: Rule) {
        self.allow_grants.push(rule);
    }

    /// Records an "Always deny" grant for the rest of the session.
    pub fn grant_deny(&mut self, rule: Rule) {
        self.deny_grants.push(rule);
    }
}
