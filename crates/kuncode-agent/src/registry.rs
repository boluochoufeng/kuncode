//! Tool registry and dispatch.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use kuncode_core::completion::ToolDefinition;
use thiserror::Error;

use crate::{
    permission::{
        CanonicalPath, PermissionNamespace, PermissionTargetError, ProfileDefault,
        ToolPermissionProfile, ToolProfileError,
    },
    tool::{
        Tool,
        bash::Bash,
        filesystem::{EditFile, Glob, ReadFile, WriteFile},
        todo_write::TodoWrite,
    },
    workspace::Workspace,
};

static NEXT_TOOL_REGISTRY_REVISION: AtomicU64 = AtomicU64::new(1);

/// Registered tools available to the agent loop.
///
/// Tool order is explicit: replacement keeps the original position and new
/// tools append to the end.
///
/// The tool definition list is sent to the model, so preserving append-only order
/// keeps the stable prefix intact for provider-side KV cache reuse.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    index: HashMap<String, usize>,
    revision: ToolRegistryRevision,
    workspace_root: Option<CanonicalPath>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            revision: next_tool_registry_revision(),
            workspace_root: None,
        }
    }
}

/// Monotonic version invalidating authorization when registrations change.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolRegistryRevision(u64);

impl ToolRegistryRevision {
    /// Returns the monotonic revision value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Tool handle and trusted permission profile captured from one registry slot.
#[derive(Clone)]
pub struct RegisteredTool {
    tool: Arc<dyn Tool>,
    profile: ToolPermissionProfile,
}

impl RegisteredTool {
    /// Returns the executable tool handle.
    pub fn tool(&self) -> &Arc<dyn Tool> {
        &self.tool
    }

    /// Returns the registry-owned permission profile.
    pub fn profile(&self) -> &ToolPermissionProfile {
        &self.profile
    }
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry with the default tools bound to a workspace.
    pub fn with_default_workspace_tools(workspace: Workspace) -> Result<Self, RegistryError> {
        let mut registry = Self::new();
        registry.register_default_workspace_tools(workspace)?;
        Ok(registry)
    }

    /// Registers the default tools used by the CLI.
    ///
    /// The order is stable for provider-side cache reuse and keeps the
    /// lowest-level escape hatch (`bash`) first, followed by safer file tools.
    pub fn register_default_workspace_tools(
        &mut self,
        workspace: Workspace,
    ) -> Result<(), RegistryError> {
        let workspace_root = CanonicalPath::from_absolute(workspace.root())?;
        if self
            .workspace_root
            .as_ref()
            .is_some_and(|root| root != &workspace_root)
        {
            return Err(RegistryError::WorkspaceMismatch);
        }
        self.workspace_root = Some(workspace_root.clone());
        self.register_with_profile(
            Bash::new(workspace.clone()),
            ToolPermissionProfile::new(
                "bash",
                [(PermissionNamespace::Bash, ProfileDefault::RequireApproval)],
                true,
            )?,
        )?;
        self.register_with_profile(
            ReadFile::new(workspace.clone()),
            ToolPermissionProfile::new(
                "read_file",
                [(PermissionNamespace::Read, ProfileDefault::Allow)],
                false,
            )?
            .constrain_paths_to(workspace_root.clone())?,
        )?;
        self.register_with_profile(
            WriteFile::new(workspace.clone()),
            ToolPermissionProfile::new(
                "write_file",
                [(PermissionNamespace::Edit, ProfileDefault::RequireApproval)],
                false,
            )?
            .constrain_paths_to(workspace_root.clone())?,
        )?;
        self.register_with_profile(
            EditFile::new(workspace.clone()),
            ToolPermissionProfile::new(
                "edit_file",
                [(PermissionNamespace::Edit, ProfileDefault::RequireApproval)],
                false,
            )?
            .constrain_paths_to(workspace_root.clone())?,
        )?;
        self.register_with_profile(
            Glob::new(workspace),
            ToolPermissionProfile::new(
                "glob",
                [
                    (PermissionNamespace::Read, ProfileDefault::Allow),
                    (
                        PermissionNamespace::ExactTool,
                        ProfileDefault::RequireApproval,
                    ),
                ],
                true,
            )?
            .constrain_paths_to(workspace_root)?,
        )?;
        self.register_with_profile(
            TodoWrite::new(),
            ToolPermissionProfile::new(
                "todo_write",
                [(PermissionNamespace::TodoWrite, ProfileDefault::Allow)],
                false,
            )?,
        )?;
        Ok(())
    }

    /// Registers a tool, replacing any existing tool with the same model-facing
    /// name.
    ///
    /// Returns the previously registered tool when a replacement occurred.
    pub fn register<T>(&mut self, tool: T) -> Result<Option<RegisteredTool>, RegistryError>
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    /// Registers an already shared tool instance.
    ///
    /// This is useful when several runners should dispatch to the same tool
    /// value without cloning the tool's internal state.
    pub fn register_arc(
        &mut self,
        tool: Arc<dyn Tool>,
    ) -> Result<Option<RegisteredTool>, RegistryError> {
        let profile = ToolPermissionProfile::exact_tool(tool.name())?;
        self.register_arc_with_profile(tool, profile)
    }

    /// Registers a tool under an explicit trusted permission profile.
    ///
    /// # Errors
    /// Returns an error when the profile identity differs from the tool name.
    pub fn register_with_profile<T>(
        &mut self,
        tool: T,
        profile: ToolPermissionProfile,
    ) -> Result<Option<RegisteredTool>, RegistryError>
    where
        T: Tool + 'static,
    {
        self.register_arc_with_profile(Arc::new(tool), profile)
    }

    /// Registers a shared tool under an explicit trusted permission profile.
    ///
    /// # Errors
    /// Returns an error when the profile identity differs from the tool name.
    pub fn register_arc_with_profile(
        &mut self,
        tool: Arc<dyn Tool>,
        profile: ToolPermissionProfile,
    ) -> Result<Option<RegisteredTool>, RegistryError> {
        let name = tool.name().to_string();
        if profile.tool() != name {
            return Err(RegistryError::ProfileToolMismatch {
                tool: name,
                profile: profile.tool().to_string(),
            });
        }
        if let Some(path_root) = profile.path_root() {
            if self
                .workspace_root
                .as_ref()
                .is_some_and(|root| root != path_root)
            {
                return Err(RegistryError::WorkspaceMismatch);
            }
            self.workspace_root = Some(path_root.clone());
        }
        let registered = RegisteredTool { tool, profile };
        if let Some(&index) = self.index.get(&name) {
            self.bump_revision();
            return Ok(Some(std::mem::replace(&mut self.tools[index], registered)));
        }

        self.index.insert(name, self.tools.len());
        self.tools.push(registered);
        self.bump_revision();
        Ok(None)
    }

    /// Returns definition for all registered tools.
    pub fn definition(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|registered| registered.tool.definition().clone())
            .collect()
    }

    /// Looks up a registered tool by its model-facing name.
    ///
    /// The runner uses this to compute the permission request and dispatch on
    /// the same handle, so the gate and the call see one tool instance.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.registered(name)
            .map(|registered| registered.tool.clone())
    }

    /// Looks up a tool and its permission profile from the same registry slot.
    pub fn registered(&self, name: &str) -> Option<&RegisteredTool> {
        self.index
            .get(name)
            .and_then(|&index| self.tools.get(index))
    }

    /// Returns the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Returns `true` when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns the version bound into authorization context snapshots.
    pub const fn revision(&self) -> ToolRegistryRevision {
        self.revision
    }

    /// Returns the canonical workspace anchor registered for built-in tools.
    pub fn workspace_root(&self) -> Option<&CanonicalPath> {
        self.workspace_root.as_ref()
    }

    fn bump_revision(&mut self) {
        self.revision = next_tool_registry_revision();
    }
}

fn next_tool_registry_revision() -> ToolRegistryRevision {
    ToolRegistryRevision(NEXT_TOOL_REGISTRY_REVISION.fetch_add(1, Ordering::Relaxed))
}

/// Invalid registry mutation.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Tool and profile identities must be exactly equal.
    #[error("tool `{tool}` cannot be registered with profile `{profile}`")]
    ProfileToolMismatch {
        /// Model-facing tool name.
        tool: String,
        /// Profile-bound tool name.
        profile: String,
    },
    /// Profile construction failed before registration.
    #[error(transparent)]
    Profile(#[from] ToolProfileError),
    /// Built-in workspace tools require a canonical UTF-8 root.
    #[error(transparent)]
    Target(#[from] PermissionTargetError),
    /// One registry cannot mix built-in tools from different workspaces.
    #[error("tool registry contains conflicting workspace roots")]
    WorkspaceMismatch,
}

#[cfg(test)]
mod tests {
    use super::ToolRegistry;
    use async_trait::async_trait;
    use kuncode_core::completion::ToolDefinition;

    use crate::permission::{CanonicalToolInput, ToolDisplay};
    use crate::tool::{
        PreparationContext, ToolContext, ToolOutput, TypedPreparation, TypedTool, bash::Bash,
        exact_typed_preparation,
    };
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
    impl TypedTool for NamedTool {
        type Args = serde_json::Value;
        type Prepared = serde_json::Value;
        type Output = serde_json::Value;

        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }

        async fn prepare_typed(
            &self,
            args: Self::Args,
            canonical_input: CanonicalToolInput,
            _ctx: &PreparationContext,
        ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
            exact_typed_preparation(
                &self.definition.name,
                args,
                canonical_input,
                ToolDisplay::new("Run named test tool"),
            )
        }

        async fn run_prepared(
            &self,
            _prepared: Self::Prepared,
            _ctx: &ToolContext,
        ) -> ToolOutput<Self::Output> {
            ToolOutput::success(serde_json::json!({
                "name": self.definition.name
            }))
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

        assert!(
            registry
                .register(bash().await)
                .expect("valid fallback profile")
                .is_none()
        );

        let definitions = registry.definition();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "bash");
    }

    #[tokio::test]
    async fn default_workspace_tools_are_registered_in_stable_order() {
        let workspace = Workspace::from_current_dir()
            .await
            .expect("current directory should be a valid workspace");
        let registry = ToolRegistry::with_default_workspace_tools(workspace)
            .expect("built-in profiles are valid");

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
        registry
            .register(bash().await)
            .expect("valid fallback profile");

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

        registry
            .register(NamedTool::new("bash"))
            .expect("valid profile");
        registry
            .register(NamedTool::new("read"))
            .expect("valid profile");
        registry
            .register(NamedTool::new("edit"))
            .expect("valid profile");
        registry
            .register(NamedTool::new("read"))
            .expect("valid profile");
        registry
            .register(NamedTool::new("write"))
            .expect("valid profile");

        let names = registry
            .definition()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["bash", "read", "edit", "write"]);
    }
}
