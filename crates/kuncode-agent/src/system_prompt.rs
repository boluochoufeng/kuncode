//! Runtime-assembled system prompt.
//!
//! Instead of one pre-baked string, the system message is built fresh each
//! request from ordered, pluggable [`PromptSection`]s. A section reads live state
//! (the registered tools) through [`PromptContext`] and may return `None` to omit
//! itself — so a section with nothing to say adds no tokens. The runner holds a
//! [`SystemPrompt`] like its other collaborators and calls
//! [`SystemPrompt::assemble`] in `build_request`.
//!
//! The system message is the cached prefix of every request, so volatile content
//! (e.g. the live task plan) is deliberately *not* a section here: changing it
//! would invalidate the KV cache for the whole transcript that follows. The
//! runner projects the live plan as a final request-only state envelope instead.

use std::path::PathBuf;

use chrono::Local;
use kuncode_core::completion::ToolDefinition;

/// Live, borrowed state a [`PromptSection`] may render from. Rebuilt per request.
pub struct PromptContext<'a> {
    /// Tool definitions actually registered for this run.
    pub tools: &'a [ToolDefinition],
}

/// One pluggable block of the system prompt.
///
/// `render` returns `None` to omit the block entirely, so a section with nothing
/// to contribute adds no stray header and no wasted tokens. Held across `.await`
/// points by the runner, hence `Send + Sync`.
pub trait PromptSection: Send + Sync {
    /// Renders this block, or `None` to omit it.
    fn render(&self, ctx: &PromptContext) -> Option<String>;
}

/// Ordered set of [`PromptSection`]s assembled into the first system message.
///
/// The default is empty, which assembles to `None` — i.e. no system message,
/// preserving the runner's prompt-free default. Frontends compose the sections
/// they want.
#[derive(Default)]
pub struct SystemPrompt {
    sections: Vec<Box<dyn PromptSection>>,
}

impl SystemPrompt {
    /// Builds from an ordered section list; list order is render order.
    pub fn new(sections: Vec<Box<dyn PromptSection>>) -> Self {
        Self { sections }
    }

    /// Joins every non-`None` block with a blank line. Returns `None` when all
    /// blocks opt out — equivalent to sending no system message.
    pub fn assemble(&self, ctx: &PromptContext) -> Option<String> {
        let blocks: Vec<String> = self
            .sections
            .iter()
            .filter_map(|section| section.render(ctx))
            .collect();
        (!blocks.is_empty()).then(|| blocks.join("\n\n"))
    }
}

/// Always-on identity and behavioral instructions.
pub struct IdentitySection(String);

impl IdentitySection {
    /// Wraps the identity/instruction text rendered verbatim every request.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

impl PromptSection for IdentitySection {
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        Some(self.0.clone())
    }
}

/// Always-on environment block: working directory, OS, and today's local date.
pub struct EnvironmentSection {
    root: PathBuf,
}

impl EnvironmentSection {
    /// `root` is the workspace directory shown to the model as the cwd.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl PromptSection for EnvironmentSection {
    fn render(&self, _ctx: &PromptContext) -> Option<String> {
        // Local (wall-clock) date, read fresh each request. It only changes
        // across day boundaries, so it does not thrash the prompt (or its cache)
        // within a session. The date grounds relative-time references ("latest",
        // "this year") and signals that knowledge past the model's training
        // cutoff may be stale.
        let today = Local::now().format("%Y-%m-%d");
        Some(format!(
            "Working directory: {}\nOS: {}\nToday's date: {today}",
            self.root.display(),
            std::env::consts::OS,
        ))
    }
}

/// Always-on list of the tool names registered for this run.
pub struct ToolsSection;

impl PromptSection for ToolsSection {
    fn render(&self, ctx: &PromptContext) -> Option<String> {
        if ctx.tools.is_empty() {
            return None;
        }
        let names: Vec<&str> = ctx.tools.iter().map(|tool| tool.name.as_str()).collect();
        Some(format!("Available tools: {}", names.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn ctx(tools: &[ToolDefinition]) -> PromptContext<'_> {
        PromptContext { tools }
    }

    #[test]
    fn identity_renders_verbatim() {
        let section = IdentitySection::new("be terse");
        assert_eq!(section.render(&ctx(&[])).as_deref(), Some("be terse"));
    }

    #[test]
    fn environment_lists_cwd_os_and_a_parseable_date() {
        let section = EnvironmentSection::new(PathBuf::from("/work"));
        let block = section.render(&ctx(&[])).expect("always renders");
        assert!(block.contains("Working directory: /work"), "{block}");
        assert!(
            block.contains(&format!("OS: {}", std::env::consts::OS)),
            "{block}"
        );
        // The date value is non-deterministic; assert its shape instead.
        let date_line = block
            .lines()
            .find_map(|l| l.strip_prefix("Today's date: "))
            .expect("a date line");
        assert!(
            chrono::NaiveDate::parse_from_str(date_line, "%Y-%m-%d").is_ok(),
            "not an ISO date: {date_line}"
        );
    }

    #[test]
    fn tools_section_omits_itself_when_no_tools() {
        assert!(ToolsSection.render(&ctx(&[])).is_none());
        let tools = [tool("bash"), tool("read_file")];
        assert_eq!(
            ToolsSection.render(&ctx(&tools)).as_deref(),
            Some("Available tools: bash, read_file"),
        );
    }

    #[test]
    fn assemble_joins_present_blocks_and_skips_none() {
        let prompt = SystemPrompt::new(vec![
            Box::new(IdentitySection::new("identity")),
            Box::new(ToolsSection), // omitted when no tools
        ]);
        assert_eq!(prompt.assemble(&ctx(&[])).as_deref(), Some("identity"));

        let tools = [tool("bash")];
        assert_eq!(
            prompt.assemble(&ctx(&tools)).as_deref(),
            Some("identity\n\nAvailable tools: bash"),
        );
    }

    #[test]
    fn empty_prompt_assembles_to_none() {
        assert!(SystemPrompt::default().assemble(&ctx(&[])).is_none());
    }
}
