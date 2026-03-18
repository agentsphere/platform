use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A parsed command input (e.g. `/dev fix the auth bug`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub name: String,
    pub arguments: String,
}

/// A resolved command ready for execution.
#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    pub name: String,
    pub prompt: String,
    pub persistent: bool,
}

/// Database row for a platform command (used by `sqlx::query_as!`).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields populated by sqlx query_as but not all read in Rust
pub struct CommandRecord {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub name: String,
    pub description: String,
    pub prompt_template: String,
    pub persistent_session: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A resolved command file entry (for the `/api/commands/resolved` endpoint).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvedCommandFile {
    pub name: String,
    pub prompt_template: String,
    pub scope: String,
    pub persistent_session: bool,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Maximum template size: 100 KB.
const MAX_TEMPLATE_SIZE: usize = 100 * 1024;

/// Parse a user input that may start with a `/command` prefix.
///
/// Returns `None` if the input is not a command (doesn't start with `/`).
pub fn parse_command_input(input: &str) -> Option<ParsedCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    // Split on first whitespace: "/dev fix the bug" → ("dev", "fix the bug")
    let without_slash = &trimmed[1..];
    let (name, arguments) = match without_slash.find(char::is_whitespace) {
        Some(pos) => (&without_slash[..pos], without_slash[pos..].trim_start()),
        None => (without_slash, ""),
    };

    if name.is_empty() {
        return None;
    }

    Some(ParsedCommand {
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a command name: 1-100 chars, alphanumeric + hyphens only.
pub fn validate_command_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 100 {
        return Err("command name must be between 1 and 100 characters".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("command name must contain only alphanumeric characters and hyphens".into());
    }
    Ok(())
}

/// Validate a prompt template: non-empty, max 100 KB.
pub fn validate_template(template: &str) -> Result<(), String> {
    if template.is_empty() {
        return Err("prompt template must not be empty".into());
    }
    if template.len() > MAX_TEMPLATE_SIZE {
        return Err(format!(
            "prompt template exceeds maximum size ({} bytes, max {})",
            template.len(),
            MAX_TEMPLATE_SIZE
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Template rendering
// ---------------------------------------------------------------------------

/// Replace `$ARGUMENTS` placeholders in a template with the actual arguments.
pub fn render_template(template: &str, arguments: &str) -> String {
    template.replace("$ARGUMENTS", arguments)
}

// ---------------------------------------------------------------------------
// Resolution (DB lookup)
// ---------------------------------------------------------------------------

/// Resolve a command by name, checking project → workspace → global.
///
/// Returns the rendered prompt with `$ARGUMENTS` replaced.
#[tracing::instrument(skip(pool), err)]
pub async fn resolve_command(
    pool: &PgPool,
    project_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
    input: &str,
) -> Result<ResolvedCommand, ApiError> {
    let parsed = parse_command_input(input)
        .ok_or_else(|| ApiError::BadRequest("input is not a command (must start with /)".into()))?;

    // Try project-scoped first, then workspace, then global
    let record = if let Some(pid) = project_id {
        let project_cmd = sqlx::query_as!(
            CommandRecord,
            r#"
            SELECT id, project_id, workspace_id, name, description, prompt_template,
                   persistent_session, created_at, updated_at
            FROM platform_commands
            WHERE name = $1 AND project_id = $2
            "#,
            parsed.name,
            pid,
        )
        .fetch_optional(pool)
        .await?;

        match project_cmd {
            Some(cmd) => cmd,
            None => fetch_workspace_or_global(pool, workspace_id, &parsed.name).await?,
        }
    } else {
        fetch_workspace_or_global(pool, workspace_id, &parsed.name).await?
    };

    let prompt = render_template(&record.prompt_template, &parsed.arguments);

    Ok(ResolvedCommand {
        name: record.name,
        prompt,
        persistent: record.persistent_session,
    })
}

/// Try workspace-scoped first, then global.
async fn fetch_workspace_or_global(
    pool: &PgPool,
    workspace_id: Option<Uuid>,
    name: &str,
) -> Result<CommandRecord, ApiError> {
    if let Some(wid) = workspace_id {
        let ws_cmd = sqlx::query_as!(
            CommandRecord,
            r#"
            SELECT id, project_id, workspace_id, name, description, prompt_template,
                   persistent_session, created_at, updated_at
            FROM platform_commands
            WHERE name = $1 AND workspace_id = $2 AND project_id IS NULL
            "#,
            name,
            wid,
        )
        .fetch_optional(pool)
        .await?;

        if let Some(cmd) = ws_cmd {
            return Ok(cmd);
        }
    }
    fetch_global_command(pool, name).await
}

async fn fetch_global_command(pool: &PgPool, name: &str) -> Result<CommandRecord, ApiError> {
    sqlx::query_as!(
        CommandRecord,
        r#"
        SELECT id, project_id, workspace_id, name, description, prompt_template,
               persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE name = $1 AND project_id IS NULL AND workspace_id IS NULL
        "#,
        name,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("command '{name}' not found")))
}

/// Resolve all commands for a project, applying the override hierarchy:
/// project > workspace > global. Returns the merged set with scope annotations.
#[tracing::instrument(skip(pool), err)]
pub async fn resolve_all_commands(
    pool: &PgPool,
    project_id: Uuid,
    workspace_id: Option<Uuid>,
) -> Result<Vec<ResolvedCommandFile>, ApiError> {
    let rows = sqlx::query_as!(
        CommandRecord,
        r#"
        SELECT DISTINCT ON (name)
            id, project_id, workspace_id, name, description, prompt_template,
            persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE (project_id IS NULL AND workspace_id IS NULL)
           OR (workspace_id = $1 AND project_id IS NULL)
           OR (project_id = $2)
        ORDER BY name,
            CASE WHEN project_id IS NOT NULL THEN 1
                 WHEN workspace_id IS NOT NULL THEN 2
                 ELSE 3 END
        "#,
        workspace_id,
        project_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let scope = if r.project_id.is_some() {
                "project"
            } else if r.workspace_id.is_some() {
                "workspace"
            } else {
                "global"
            };
            ResolvedCommandFile {
                name: r.name,
                prompt_template: r.prompt_template,
                scope: scope.to_owned(),
                persistent_session: r.persistent_session,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_command_input --

    #[test]
    fn parse_command_input_dev() {
        let result = parse_command_input("/dev fix bug").unwrap();
        assert_eq!(result.name, "dev");
        assert_eq!(result.arguments, "fix bug");
    }

    #[test]
    fn parse_command_input_no_args() {
        let result = parse_command_input("/review").unwrap();
        assert_eq!(result.name, "review");
        assert_eq!(result.arguments, "");
    }

    #[test]
    fn parse_command_input_not_a_command() {
        assert!(parse_command_input("fix the bug").is_none());
    }

    #[test]
    fn parse_command_input_slash_in_middle() {
        assert!(parse_command_input("fix /dev bug").is_none());
    }

    #[test]
    fn parse_command_input_empty() {
        assert!(parse_command_input("").is_none());
    }

    #[test]
    fn parse_command_input_just_slash() {
        assert!(parse_command_input("/").is_none());
    }

    #[test]
    fn parse_command_input_whitespace_before_slash() {
        // Trimmed, so leading whitespace is stripped
        let result = parse_command_input("  /plan add caching").unwrap();
        assert_eq!(result.name, "plan");
        assert_eq!(result.arguments, "add caching");
    }

    #[test]
    fn parse_command_input_multi_word_args() {
        let result = parse_command_input("/dev fix the authentication bug in auth.rs").unwrap();
        assert_eq!(result.name, "dev");
        assert_eq!(result.arguments, "fix the authentication bug in auth.rs");
    }

    // -- validate_command_name --

    #[test]
    fn command_name_validation_valid() {
        assert!(validate_command_name("dev").is_ok());
        assert!(validate_command_name("plan-review").is_ok());
        assert!(validate_command_name("my-cmd").is_ok());
        assert!(validate_command_name("a").is_ok());
    }

    #[test]
    fn command_name_validation_invalid() {
        assert!(validate_command_name("").is_err());
        assert!(validate_command_name("has space").is_err());
        assert!(validate_command_name("has_underscore").is_err());
        assert!(validate_command_name("has.dot").is_err());
        assert!(validate_command_name(&"a".repeat(101)).is_err());
    }

    // -- validate_template --

    #[test]
    fn template_valid() {
        assert!(validate_template("Do something with $ARGUMENTS").is_ok());
    }

    #[test]
    fn template_empty_rejected() {
        assert!(validate_template("").is_err());
    }

    #[test]
    fn template_size_limit() {
        let big = "x".repeat(MAX_TEMPLATE_SIZE + 1);
        assert!(validate_template(&big).is_err());
    }

    #[test]
    fn template_at_limit_accepted() {
        let at_limit = "x".repeat(MAX_TEMPLATE_SIZE);
        assert!(validate_template(&at_limit).is_ok());
    }

    // -- render_template --

    #[test]
    fn template_arguments_substitution() {
        let result = render_template("Fix the bug: $ARGUMENTS", "auth module");
        assert_eq!(result, "Fix the bug: auth module");
    }

    #[test]
    fn template_no_arguments_placeholder() {
        let result = render_template("Just review everything", "some args");
        assert_eq!(result, "Just review everything");
    }

    #[test]
    fn template_multiple_arguments_placeholders() {
        let result = render_template("First: $ARGUMENTS\nSecond: $ARGUMENTS", "test");
        assert_eq!(result, "First: test\nSecond: test");
    }

    #[test]
    fn template_empty_arguments() {
        let result = render_template("Do $ARGUMENTS now", "");
        assert_eq!(result, "Do  now");
    }
}
