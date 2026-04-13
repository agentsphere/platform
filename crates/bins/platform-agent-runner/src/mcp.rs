// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::path::{Path, PathBuf};

use anyhow::Context;

/// MCP server names (admin intentionally excluded — agents should not have admin access).
const MCP_SERVERS: &[&str] = &[
    "platform-core",
    "platform-issues",
    "platform-pipeline",
    "platform-deploy",
    "platform-observe",
];

/// Workspace-downloaded MCP path (init container extracts here).
const MCP_WORKSPACE_PATH: &str = "/workspace/.platform/mcp/servers";
/// Baked-in MCP path from Docker image (fallback).
const MCP_BAKED_IN_PATH: &str = "/usr/local/lib/mcp/servers";

/// Resolve the MCP server base path: prefer workspace download, fallback to baked-in.
fn mcp_server_base_path() -> &'static str {
    if Path::new(MCP_WORKSPACE_PATH).is_dir() {
        MCP_WORKSPACE_PATH
    } else {
        MCP_BAKED_IN_PATH
    }
}

/// Context vars passed to each MCP server process.
pub struct McpContext<'a> {
    pub platform_api_url: &'a str,
    pub platform_api_token: &'a str,
    pub session_id: &'a str,
    pub project_id: &'a str,
}

/// Generate MCP config JSON for the agent's Claude CLI invocation.
pub fn generate_mcp_config(ctx: &McpContext<'_>) -> serde_json::Value {
    let base_path = mcp_server_base_path();
    let mut servers = serde_json::Map::new();

    for server_name in MCP_SERVERS {
        let server_path = format!("{base_path}/{server_name}.js");
        let server_config = serde_json::json!({
            "command": "node",
            "args": [server_path],
            "env": {
                "PLATFORM_API_URL": ctx.platform_api_url,
                "PLATFORM_API_TOKEN": ctx.platform_api_token,
                "SESSION_ID": ctx.session_id,
                "PROJECT_ID": ctx.project_id,
            }
        });
        servers.insert((*server_name).to_owned(), server_config);
    }

    serde_json::json!({ "mcpServers": servers })
}

/// Write MCP config to a file in the given directory.
/// Returns the path to the written file.
pub fn write_mcp_config(config_dir: &Path, config: &serde_json::Value) -> anyhow::Result<PathBuf> {
    let path = config_dir.join("mcp_config.json");
    let json = serde_json::to_string_pretty(config).context("failed to serialize MCP config")?;
    std::fs::write(&path, json).context("failed to write MCP config file")?;
    Ok(path)
}

/// Resolve MCP config from environment variables.
/// Returns `None` if either `PLATFORM_API_TOKEN` or `PLATFORM_API_URL` is missing or empty.
pub fn resolve_mcp_config() -> Option<serde_json::Value> {
    let api_url = std::env::var("PLATFORM_API_URL")
        .ok()
        .filter(|v| !v.is_empty())?;
    let api_token = std::env::var("PLATFORM_API_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())?;
    let session_id = std::env::var("SESSION_ID").unwrap_or_default();
    let project_id = std::env::var("PROJECT_ID").unwrap_or_default();
    Some(generate_mcp_config(&McpContext {
        platform_api_url: &api_url,
        platform_api_token: &api_token,
        session_id: &session_id,
        project_id: &project_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unsafe_code)]
    fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: tests are #[serial] — no concurrent env access
        unsafe { std::env::set_var(key, val) }
    }

    #[allow(unsafe_code)]
    fn remove_env(key: &str) {
        // SAFETY: tests are #[serial] — no concurrent env access
        unsafe { std::env::remove_var(key) }
    }

    fn test_ctx() -> McpContext<'static> {
        McpContext {
            platform_api_url: "http://platform:8080",
            platform_api_token: "tok-123",
            session_id: "sess-abc",
            project_id: "proj-xyz",
        }
    }

    #[test]
    fn test_generate_mcp_config_valid_json() {
        let config = generate_mcp_config(&test_ctx());
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 5, "should have 5 MCP servers");
    }

    #[test]
    fn test_generate_mcp_config_correct_paths() {
        let base = mcp_server_base_path();
        let config = generate_mcp_config(&test_ctx());
        let servers = config["mcpServers"].as_object().unwrap();
        for (name, server) in servers {
            let args = server["args"].as_array().unwrap();
            let path = args[0].as_str().unwrap();
            assert_eq!(
                path,
                format!("{base}/{name}.js"),
                "server path mismatch for {name}"
            );
            assert_eq!(server["command"], "node");
        }
    }

    #[test]
    fn test_generate_mcp_config_excludes_admin() {
        let config = generate_mcp_config(&test_ctx());
        let servers = config["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("platform-admin"),
            "admin server must be excluded"
        );
    }

    #[test]
    fn test_generate_mcp_config_injects_env_vars() {
        let ctx = McpContext {
            platform_api_url: "http://my-platform:8080",
            platform_api_token: "my-token",
            session_id: "my-session",
            project_id: "my-project",
        };
        let config = generate_mcp_config(&ctx);
        let servers = config["mcpServers"].as_object().unwrap();
        for (_name, server) in servers {
            let env = server["env"].as_object().unwrap();
            assert_eq!(env["PLATFORM_API_URL"], "http://my-platform:8080");
            assert_eq!(env["PLATFORM_API_TOKEN"], "my-token");
            assert_eq!(env["SESSION_ID"], "my-session");
            assert_eq!(env["PROJECT_ID"], "my-project");
        }
    }

    #[test]
    fn test_generate_mcp_config_file_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = generate_mcp_config(&test_ctx());
        let path = write_mcp_config(dir.path(), &config).unwrap();
        assert!(path.exists(), "config file should exist");
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(content["mcpServers"].is_object());
    }

    #[test]
    fn test_mcp_config_all_server_names() {
        let config = generate_mcp_config(&test_ctx());
        let servers = config["mcpServers"].as_object().unwrap();
        let names: Vec<&str> = servers.keys().map(|s| s.as_str()).collect();
        assert!(names.contains(&"platform-core"));
        assert!(names.contains(&"platform-issues"));
        assert!(names.contains(&"platform-pipeline"));
        assert!(names.contains(&"platform-deploy"));
        assert!(names.contains(&"platform-observe"));
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_mcp_config_returns_some_when_both_set() {
        let url_backup = std::env::var("PLATFORM_API_URL").ok();
        let token_backup = std::env::var("PLATFORM_API_TOKEN").ok();
        set_env("PLATFORM_API_URL", "http://platform:8080");
        set_env("PLATFORM_API_TOKEN", "tok-abc");

        let config = resolve_mcp_config();
        assert!(config.is_some());
        let servers = config.unwrap()["mcpServers"].as_object().unwrap().clone();
        assert_eq!(servers.len(), 5);
        // Verify env vars injected
        let core = &servers["platform-core"];
        assert_eq!(core["env"]["PLATFORM_API_URL"], "http://platform:8080");
        assert_eq!(core["env"]["PLATFORM_API_TOKEN"], "tok-abc");

        match url_backup {
            Some(v) => set_env("PLATFORM_API_URL", v),
            None => remove_env("PLATFORM_API_URL"),
        }
        match token_backup {
            Some(v) => set_env("PLATFORM_API_TOKEN", v),
            None => remove_env("PLATFORM_API_TOKEN"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_mcp_config_returns_none_when_url_missing() {
        let url_backup = std::env::var("PLATFORM_API_URL").ok();
        let token_backup = std::env::var("PLATFORM_API_TOKEN").ok();
        remove_env("PLATFORM_API_URL");
        set_env("PLATFORM_API_TOKEN", "tok-abc");

        assert!(resolve_mcp_config().is_none());

        match url_backup {
            Some(v) => set_env("PLATFORM_API_URL", v),
            None => remove_env("PLATFORM_API_URL"),
        }
        match token_backup {
            Some(v) => set_env("PLATFORM_API_TOKEN", v),
            None => remove_env("PLATFORM_API_TOKEN"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_mcp_config_returns_none_when_token_missing() {
        let url_backup = std::env::var("PLATFORM_API_URL").ok();
        let token_backup = std::env::var("PLATFORM_API_TOKEN").ok();
        set_env("PLATFORM_API_URL", "http://platform:8080");
        remove_env("PLATFORM_API_TOKEN");

        assert!(resolve_mcp_config().is_none());

        match url_backup {
            Some(v) => set_env("PLATFORM_API_URL", v),
            None => remove_env("PLATFORM_API_URL"),
        }
        match token_backup {
            Some(v) => set_env("PLATFORM_API_TOKEN", v),
            None => remove_env("PLATFORM_API_TOKEN"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_mcp_config_returns_none_when_url_empty() {
        let url_backup = std::env::var("PLATFORM_API_URL").ok();
        let token_backup = std::env::var("PLATFORM_API_TOKEN").ok();
        set_env("PLATFORM_API_URL", "");
        set_env("PLATFORM_API_TOKEN", "tok-abc");

        assert!(resolve_mcp_config().is_none());

        match url_backup {
            Some(v) => set_env("PLATFORM_API_URL", v),
            None => remove_env("PLATFORM_API_URL"),
        }
        match token_backup {
            Some(v) => set_env("PLATFORM_API_TOKEN", v),
            None => remove_env("PLATFORM_API_TOKEN"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_mcp_config_returns_none_when_token_empty() {
        let url_backup = std::env::var("PLATFORM_API_URL").ok();
        let token_backup = std::env::var("PLATFORM_API_TOKEN").ok();
        set_env("PLATFORM_API_URL", "http://platform:8080");
        set_env("PLATFORM_API_TOKEN", "");

        assert!(resolve_mcp_config().is_none());

        match url_backup {
            Some(v) => set_env("PLATFORM_API_URL", v),
            None => remove_env("PLATFORM_API_URL"),
        }
        match token_backup {
            Some(v) => set_env("PLATFORM_API_TOKEN", v),
            None => remove_env("PLATFORM_API_TOKEN"),
        }
    }

    #[test]
    fn test_write_mcp_config_to_nonexistent_dir_fails() {
        let config = generate_mcp_config(&test_ctx());
        let result = write_mcp_config(Path::new("/nonexistent/dir/path"), &config);
        assert!(result.is_err());
    }
}
