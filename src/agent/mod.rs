pub mod claude_cli;
pub mod claude_code;
pub mod cli_invoke;
pub mod commands;
pub mod create_app;
pub mod create_app_prompt;
pub mod error;
pub mod identity;
pub mod preview_watcher;
pub mod provider;
pub mod pubsub_bridge;
pub mod service;
pub mod valkey_acl;

use std::fmt;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// AgentRoleName — typed representation of agent DB roles
// ---------------------------------------------------------------------------

/// Typed representation of agent role names.
///
/// Each variant maps to a `roles` row (name = `"agent-{variant}"`).
/// Project-scoped roles restrict agents to a single project.
/// Workspace-scoped roles allow agents to operate across a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRoleName {
    Dev,
    Ops,
    Test,
    Review,
    Manager,
}

impl AgentRoleName {
    /// The database role name (matches `roles.name` column).
    pub fn db_role_name(self) -> &'static str {
        match self {
            Self::Dev => "agent-dev",
            Self::Ops => "agent-ops",
            Self::Test => "agent-test",
            Self::Review => "agent-review",
            Self::Manager => "agent-manager",
        }
    }

    /// Whether this role is scoped to a workspace (vs project).
    /// Manager is workspace-scoped; all others are project-scoped.
    pub fn is_workspace_scoped(self) -> bool {
        matches!(self, Self::Manager)
    }
}

impl FromStr for AgentRoleName {
    type Err = AgentRoleParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "dev" | "agent-dev" => Ok(Self::Dev),
            "ops" | "agent-ops" => Ok(Self::Ops),
            "test" | "agent-test" => Ok(Self::Test),
            "review" | "agent-review" => Ok(Self::Review),
            "manager" | "agent-manager" | "create-app" => Ok(Self::Manager),
            _ => Err(AgentRoleParseError(s.to_owned())),
        }
    }
}

impl fmt::Display for AgentRoleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.db_role_name())
    }
}

#[derive(Debug)]
pub struct AgentRoleParseError(pub String);

impl fmt::Display for AgentRoleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown agent role: {:?}", self.0)
    }
}

impl std::error::Error for AgentRoleParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    // -- AgentRoleName::from_str --

    #[test]
    fn from_str_dev() {
        assert_eq!("dev".parse::<AgentRoleName>().unwrap(), AgentRoleName::Dev);
    }

    #[test]
    fn from_str_agent_dev() {
        assert_eq!(
            "agent-dev".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Dev
        );
    }

    #[test]
    fn from_str_ops() {
        assert_eq!("ops".parse::<AgentRoleName>().unwrap(), AgentRoleName::Ops);
    }

    #[test]
    fn from_str_agent_ops() {
        assert_eq!(
            "agent-ops".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Ops
        );
    }

    #[test]
    fn from_str_test() {
        assert_eq!(
            "test".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Test
        );
    }

    #[test]
    fn from_str_review() {
        assert_eq!(
            "review".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Review
        );
    }

    #[test]
    fn from_str_manager() {
        assert_eq!(
            "manager".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Manager
        );
    }

    #[test]
    fn from_str_agent_manager() {
        assert_eq!(
            "agent-manager".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Manager
        );
    }

    #[test]
    fn from_str_create_app_alias() {
        assert_eq!(
            "create-app".parse::<AgentRoleName>().unwrap(),
            AgentRoleName::Manager
        );
    }

    #[test]
    fn from_str_unknown() {
        assert!("unknown".parse::<AgentRoleName>().is_err());
    }

    #[test]
    fn from_str_empty() {
        assert!("".parse::<AgentRoleName>().is_err());
    }

    // -- db_role_name --

    #[test]
    fn db_role_name_dev() {
        assert_eq!(AgentRoleName::Dev.db_role_name(), "agent-dev");
    }

    #[test]
    fn db_role_name_ops() {
        assert_eq!(AgentRoleName::Ops.db_role_name(), "agent-ops");
    }

    #[test]
    fn db_role_name_test() {
        assert_eq!(AgentRoleName::Test.db_role_name(), "agent-test");
    }

    #[test]
    fn db_role_name_review() {
        assert_eq!(AgentRoleName::Review.db_role_name(), "agent-review");
    }

    #[test]
    fn db_role_name_manager() {
        assert_eq!(AgentRoleName::Manager.db_role_name(), "agent-manager");
    }

    // -- is_workspace_scoped --

    #[test]
    fn workspace_scoped_manager() {
        assert!(AgentRoleName::Manager.is_workspace_scoped());
    }

    #[test]
    fn workspace_scoped_dev() {
        assert!(!AgentRoleName::Dev.is_workspace_scoped());
    }

    #[test]
    fn workspace_scoped_ops() {
        assert!(!AgentRoleName::Ops.is_workspace_scoped());
    }

    #[test]
    fn workspace_scoped_test() {
        assert!(!AgentRoleName::Test.is_workspace_scoped());
    }

    #[test]
    fn workspace_scoped_review() {
        assert!(!AgentRoleName::Review.is_workspace_scoped());
    }

    // -- Display --

    #[test]
    fn display_uses_db_role_name() {
        assert_eq!(format!("{}", AgentRoleName::Dev), "agent-dev");
        assert_eq!(format!("{}", AgentRoleName::Manager), "agent-manager");
    }
}
