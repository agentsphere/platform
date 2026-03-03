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

/// Base path for MCP server JS files inside the container.
const MCP_SERVER_BASE_PATH: &str = "/opt/mcp/servers";

/// Generate MCP config JSON for the agent's Claude CLI invocation.
///
/// Returns `None` if `PLATFORM_API_TOKEN` or `PLATFORM_API_URL` are not set.
pub fn generate_mcp_config(
    platform_api_url: &str,
    platform_api_token: &str,
) -> serde_json::Value {
    let mut servers = serde_json::Map::new();

    for server_name in MCP_SERVERS {
        let server_path = format!("{MCP_SERVER_BASE_PATH}/{server_name}.js");
        let server_config = serde_json::json!({
            "command": "node",
            "args": [server_path],
            "env": {
                "PLATFORM_API_URL": platform_api_url,
                "PLATFORM_API_TOKEN": platform_api_token,
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
    let api_url = std::env::var("PLATFORM_API_URL").ok().filter(|v| !v.is_empty())?;
    let api_token = std::env::var("PLATFORM_API_TOKEN").ok().filter(|v| !v.is_empty())?;
    Some(generate_mcp_config(&api_url, &api_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_mcp_config_valid_json() {
        let config = generate_mcp_config("http://platform:8080", "tok-123");
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 5, "should have 5 MCP servers");
    }

    #[test]
    fn test_generate_mcp_config_correct_paths() {
        let config = generate_mcp_config("http://platform:8080", "tok-123");
        let servers = config["mcpServers"].as_object().unwrap();
        for (name, server) in servers {
            let args = server["args"].as_array().unwrap();
            let path = args[0].as_str().unwrap();
            assert_eq!(
                path,
                format!("/opt/mcp/servers/{name}.js"),
                "server path mismatch for {name}"
            );
            assert_eq!(server["command"], "node");
        }
    }

    #[test]
    fn test_generate_mcp_config_excludes_admin() {
        let config = generate_mcp_config("http://platform:8080", "tok-123");
        let servers = config["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("platform-admin"),
            "admin server must be excluded"
        );
    }

    #[test]
    fn test_generate_mcp_config_injects_env_vars() {
        let config = generate_mcp_config("http://my-platform:8080", "my-token");
        let servers = config["mcpServers"].as_object().unwrap();
        for (_name, server) in servers {
            let env = server["env"].as_object().unwrap();
            assert_eq!(env["PLATFORM_API_URL"], "http://my-platform:8080");
            assert_eq!(env["PLATFORM_API_TOKEN"], "my-token");
        }
    }

    #[test]
    fn test_generate_mcp_config_file_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = generate_mcp_config("http://platform:8080", "tok-123");
        let path = write_mcp_config(dir.path(), &config).unwrap();
        assert!(path.exists(), "config file should exist");
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(content["mcpServers"].is_object());
    }

    #[test]
    fn test_mcp_config_all_server_names() {
        let config = generate_mcp_config("http://platform:8080", "tok-123");
        let servers = config["mcpServers"].as_object().unwrap();
        let names: Vec<&str> = servers.keys().map(|s| s.as_str()).collect();
        assert!(names.contains(&"platform-core"));
        assert!(names.contains(&"platform-issues"));
        assert!(names.contains(&"platform-pipeline"));
        assert!(names.contains(&"platform-deploy"));
        assert!(names.contains(&"platform-observe"));
    }
}
