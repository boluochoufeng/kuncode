//! Tool registry and dispatch.

use std::{collections::HashMap, sync::Arc};

use kuncode_core::completion::ToolDefinition;

use crate::tool::{Tool, ToolError, ToolOutput};

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

    /// Dispatches a tool call by the name emitted by the model.
    ///
    /// Unknown tools are returned as model-recoverable tool failures so the
    /// runner can append the result to the transcript and let the model retry.
    pub async fn call(&self, name: &str, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        match self
            .index
            .get(name)
            .and_then(|&index| self.tools.get(index))
        {
            Some(tool) => tool.call(args).await,
            None => Ok(ToolOutput::failure(
                "unknown_tool",
                format!("tool `{name}` is not registered"),
            )),
        }
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

    use crate::tool::{Tool, ToolOutput, bash::Bash};

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

        async fn call(
            &self,
            _args: serde_json::Value,
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

    #[test]
    fn registered_tool_is_advertised_to_the_model() {
        let mut registry = ToolRegistry::new();

        assert!(registry.register(Bash::new()).is_none());

        let definitions = registry.definition();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "bash");
    }

    #[test]
    fn definitions_json_snapshot_is_stable_for_cache_prefix() {
        let mut registry = ToolRegistry::new();
        registry.register(Bash::new());

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

    #[tokio::test]
    async fn dispatches_registered_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Bash::new());

        let output = registry
            .call("bash", serde_json::json!({ "cmd": "printf registry" }))
            .await
            .expect("registered tool should not fail at harness level");

        assert!(output.ok);
        assert_eq!(output.data.expect("tool data")["stdout"], "registry");
    }

    #[tokio::test]
    async fn unknown_tool_is_model_recoverable() {
        let registry = ToolRegistry::new();

        let output = registry
            .call("missing", serde_json::json!({}))
            .await
            .expect("unknown tool should be reported to the model");

        assert!(!output.ok);
        assert_eq!(output.error.expect("error payload").kind, "unknown_tool");
    }
}
