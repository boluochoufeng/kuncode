//! Per-session, mutable permission state.
//!
//! This lives inside [`AgentSession`](crate::session::AgentSession), which is
//! already passed `&mut` into every turn. Keeping the *mutable* grants here —
//! rather than on the shared, `Clone` + `&self` runner — gives per-session
//! isolation with no lock and no cross-session leak.

use std::sync::atomic::{AtomicU64, Ordering};

use super::rule::PermissionRule;

static NEXT_SESSION_OVERLAY_REVISION: AtomicU64 = AtomicU64::new(1);

/// How strict the permission gate is for this session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Uses explicit contributions first and profile defaults otherwise.
    #[default]
    Default,
    /// Adds `Allow` for workspace-local edits; explicit Ask and Deny still win.
    AcceptEdits,
    /// Denies mutation, process, network, remote-tool, and sub-agent checks.
    /// Read-only checks continue through normal policy resolution.
    Plan,
    /// Adds `Allow` so profile-default Ask is skipped; explicit Ask and Deny win.
    BypassPermissions,
    /// Resolves policy normally but rejects any remaining approval request.
    DontAsk,
}

impl PermissionMode {
    /// Parses a mode name from settings or a CLI flag. Accepts both the
    /// Claude-Code-style camelCase and a kebab/short form.
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim() {
            "default" => Some(Self::Default),
            "acceptEdits" | "accept-edits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" | "bypass-permissions" | "bypass" => Some(Self::BypassPermissions),
            "dontAsk" | "dont-ask" => Some(Self::DontAsk),
            _ => None,
        }
    }
}

/// Monotonic version of the mutable session policy overlay.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionOverlayRevision(u64);

impl SessionOverlayRevision {
    /// Returns the monotonic revision value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Session-isolated mode and typed policy mutations used by authorization.
#[derive(Clone, Debug)]
pub struct SessionPolicyOverlay {
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
    revision: SessionOverlayRevision,
}

impl Default for SessionPolicyOverlay {
    fn default() -> Self {
        Self::new(PermissionMode::Default)
    }
}

impl SessionPolicyOverlay {
    /// Creates an empty overlay in the requested mode.
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            rules: Vec::new(),
            revision: next_session_overlay_revision(),
        }
    }

    /// Returns the active mode.
    pub const fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Changes mode and invalidates outstanding approvals when needed.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        if self.mode != mode {
            self.mode = mode;
            self.bump_revision();
        }
    }

    /// Appends a challenge-validated session rule.
    pub fn push(&mut self, rule: PermissionRule) {
        self.rules.push(rule);
        self.bump_revision();
    }

    /// Returns typed session rules in insertion order.
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Returns the version included in authorization snapshots.
    pub const fn revision(&self) -> SessionOverlayRevision {
        self.revision
    }

    fn bump_revision(&mut self) {
        self.revision = next_session_overlay_revision();
    }
}

fn next_session_overlay_revision() -> SessionOverlayRevision {
    SessionOverlayRevision(NEXT_SESSION_OVERLAY_REVISION.fetch_add(1, Ordering::Relaxed))
}
