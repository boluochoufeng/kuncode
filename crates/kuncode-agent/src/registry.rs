//! Tool registry and dispatch.

use std::{collections::HashMap, sync::Arc};

use kuncode_core::completion::ToolDefinition;

use crate::{
    tool::{
        Tool,
        bash::Bash,
        filesystem::{EditFile, Glob, ReadFile, WriteFile},
        todo_write::TodoWrite,
    },
    workspace::Workspace,
};

/// Registered tools available to the agent loop.
///
/// Tool order is explicit: replacement keeps the original position and new
/// tools append to the end.
///
/// The tool definition list is sent to the model, so preserving append-only order
/// keeps the stable prefix intact for provider-side KV cache reuse.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    index: HashMap<String, usize>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry with the default tools bound to a workspace.
    pub fn with_default_workspace_tools(workspace: Workspace) -> Self {
        let mut registry = Self::new();
        registry.register_default_workspace_tools(workspace);
        registry
    }

    /// Registers the default tools used by the CLI.
    ///
    /// The order is stable for provider-side cache reuse and keeps the
    /// lowest-level escape hatch (`bash`) first, followed by safer file tools.
    pub fn register_default_workspace_tools(&mut self, workspace: Workspace) {
        let _ = self.register(Bash::new(workspace.clone()));
        let _ = self.register(ReadFile::new(workspace.clone()));
        let _ = self.register(WriteFile::new(workspace.clone()));
        let _ = self.register(EditFile::new(workspace.clone()));
        let _ = self.register(Glob::new(workspace));
        // Plan tool last: workspace-free and appended after the file tools to
        // keep the definition prefix stable for provider-side cache reuse.
        let _ = self.register(TodoWrite::new());
    }

    /// Registers a tool, replacing any existing tool with the same model-facing
    /// name.
    ///
    /// Returns the previously registered tool when a replacement occurred.
    pub fn register<T>(&mut self, tool: T) -> Option<Arc<dyn Tool>>
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    /// Registers an already shared tool instance.
    ///
    /// This is useful when several runners should dispatch to the same tool
    /// value without cloning the tool's internal state.
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        let name = tool.name().to_string();
        if let Some(&index) = self.index.get(&name) {
            return Some(std::mem::replace(&mut self.tools[index], tool));
        }

        self.index.insert(name, self.tools.len());
        self.tools.push(tool);
        None
    }

    /// Returns definition for all registered tools.
    pub fn definition(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| tool.definition().clone())
            .collect()
    }

    /// Looks up a registered tool by its model-facing name.
    ///
    /// The runner uses this to compute the permission request and dispatch on
    /// the same handle, so the gate and the call see one tool instance.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.index
            .get(name)
            .and_then(|&index| self.tools.get(index))
            .cloned()
    }

    /// Returns the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Returns `true` when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::ToolRegistry;
    use async_trait::async_trait;
    use kuncode_core::completion::ToolDefinition;

    use crate::permission::{PermissionAction, PermissionRequest};
    use crate::tool::{Tool, ToolContext, ToolOutput, bash::Bash};
    use crate::workspace::Workspace;

    async fn bash() -> Bash {
        Bash::from_current_dir()
            .await
            .expect("current directory should be a valid workspace")
    }

    struct NamedTool {
        definition: ToolDefinition,
    }

    impl NamedTool {
        fn new(name: &str) -> Self {
            Self {
                definition: ToolDefinition {
                    name: name.to_string(),
                    description: format!("tool {name}"),
                    parameters: serde_json::json!({ "type": "object" }),
                },
            }
        }
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        fn permission(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<PermissionRequest, ToolOutput> {
            Ok(PermissionRequest::new(
                self.definition.name.clone(),
                PermissionAction::Read,
                None,
                "test tool",
            ))
        }

        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, crate::tool::ToolError> {
            Ok(ToolOutput::success(serde_json::json!({
                "name": self.definition.name
            })))
        }
    }

    #[test]
    fn registry_starts_empty() {
        let registry = ToolRegistry::new();

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.definition().is_empty());
    }

    #[tokio::test]
    async fn registered_tool_is_advertised_to_the_model() {
        let mut registry = ToolRegistry::new();

        assert!(registry.register(bash().await).is_none());

        let definitions = registry.definition();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "bash");
    }

    #[tokio::test]
    async fn default_workspace_tools_are_registered_in_stable_order() {
        let workspace = Workspace::from_current_dir()
            .await
            .expect("current directory should be a valid workspace");
        let registry = ToolRegistry::with_default_workspace_tools(workspace);

        let names = registry
            .definition()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "bash",
                "read_file",
                "write_file",
                "edit_file",
                "glob",
                "todo_write"
            ]
        );
    }

    #[tokio::test]
    async fn definitions_json_snapshot_is_stable_for_cache_prefix() {
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);

        let snapshot =
            serde_json::to_string(&registry.definition()).expect("definitions serialize to JSON");

        assert_eq!(
            snapshot,
            r#"[{"name":"bash","description":"Run a shell command","parameters":{"description":"Arguments accepted by the [`Bash`] tool.","properties":{"cmd":{"description":"The shell command to run, e.g. `ls -la .`","type":"string"}},"required":["cmd"],"type":"object"}}]"#
        );
    }

    #[test]
    fn definitions_order_is_append_only_for_cache_stability() {
        let mut registry = ToolRegistry::new();

        registry.register(NamedTool::new("bash"));
        registry.register(NamedTool::new("read"));
        registry.register(NamedTool::new("edit"));
        registry.register(NamedTool::new("read"));
        registry.register(NamedTool::new("write"));

        let names = registry
            .definition()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["bash", "read", "edit", "write"]);
    }
}
