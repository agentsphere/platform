#[allow(dead_code)] // Copied from platform crate — not all paths used in standalone binary
mod control;
#[allow(dead_code)] // Copied from platform crate — not all paths used in standalone binary
mod error;
mod mcp;
mod messages;
mod pubsub;
mod render;
mod repl;
#[allow(dead_code)] // Copied from platform crate — not all paths used in standalone binary
mod transport;

#[cfg(test)]
mod llm_tests;

use anyhow::{bail, Context};
use clap::Parser;

use pubsub::PubSubClient;
use transport::CliSpawnOptions;

// ---------------------------------------------------------------------------
// Reserved env vars — cannot be overridden via --extra-env
// ---------------------------------------------------------------------------

const RESERVED_ENV_VARS: &[&str] = &[
    // Auth & platform credentials
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "VALKEY_URL",
    "SESSION_ID",
    "PLATFORM_API_TOKEN",
    "PLATFORM_API_URL",
    // System paths
    "PATH",
    "HOME",
    "TMPDIR",
    // Proxy vars — prevent redirecting Claude CLI HTTP traffic through attacker proxy
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    // Node.js security — Claude CLI is a Node process
    "NODE_OPTIONS",
    "NODE_EXTRA_CA_CERTS",
    // TLS trust — prevent attacker-controlled CA injection
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "agent-runner",
    about = "Claude CLI wrapper with Valkey pub/sub for platform agent pods"
)]
struct Cli {
    /// Initial prompt to send to Claude (if omitted, reads first line from stdin)
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Model selection (e.g. "opus", "sonnet")
    #[arg(long)]
    model: Option<String>,

    /// System prompt
    #[arg(long)]
    system_prompt: Option<String>,

    /// Maximum turns before auto-exit
    #[arg(long)]
    max_turns: Option<u32>,

    /// Permission mode (e.g. "bypassPermissions")
    #[arg(long)]
    permission_mode: Option<String>,

    /// Comma-separated tool names to allow
    #[arg(long)]
    allowed_tools: Option<String>,

    /// Working directory for the Claude CLI
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,

    /// Path to the `claude` binary (overrides CLAUDE_CLI_PATH)
    #[arg(long)]
    cli_path: Option<std::path::PathBuf>,

    /// Additional KEY=VALUE env vars to pass to the CLI (repeatable)
    #[arg(long = "extra-env")]
    extra_env: Vec<String>,

    /// Use --dangerously-skip-permissions instead of --permission-mode
    #[arg(long)]
    dangerously_skip_permissions: bool,

    /// Disable MCP server integration even when PLATFORM_API_TOKEN is set.
    #[arg(long)]
    no_mcp: bool,
}

// ---------------------------------------------------------------------------
// Auth resolution
// ---------------------------------------------------------------------------

// No Debug derive — contains secrets (API keys, OAuth tokens)
enum AuthToken {
    OAuth(String),
    ApiKey(String),
}

fn resolve_auth() -> anyhow::Result<AuthToken> {
    if let Ok(token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        if !token.is_empty() {
            return Ok(AuthToken::OAuth(token));
        }
    }
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            return Ok(AuthToken::ApiKey(key));
        }
    }
    bail!(
        "no auth credentials found.\n\
         Set CLAUDE_CODE_OAUTH_TOKEN (subscription) or ANTHROPIC_API_KEY (API key)."
    )
}

// ---------------------------------------------------------------------------
// Extra env parsing + validation
// ---------------------------------------------------------------------------

fn parse_extra_env(raw: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for entry in raw {
        let (key, value) = entry.split_once('=').with_context(|| {
            format!("invalid --extra-env format: {entry:?} (expected KEY=VALUE)")
        })?;

        if RESERVED_ENV_VARS.contains(&key) {
            bail!(
                "--extra-env cannot override reserved variable: {key}\n\
                 Reserved vars: {}",
                RESERVED_ENV_VARS.join(", ")
            );
        }

        pairs.push((key.to_owned(), value.to_owned()));
    }
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// Pub/sub config resolution
// ---------------------------------------------------------------------------

// No Debug derive — url may contain embedded Valkey password
struct PubSubConfig {
    url: String,
    session_id: String,
}

fn resolve_pubsub() -> anyhow::Result<Option<PubSubConfig>> {
    let valkey_url = std::env::var("VALKEY_URL").ok();
    let session_id = std::env::var("SESSION_ID").ok();

    match (valkey_url, session_id) {
        (Some(url), Some(sid)) if !url.is_empty() && !sid.is_empty() => Ok(Some(PubSubConfig {
            url,
            session_id: sid,
        })),
        (Some(url), _) if !url.is_empty() => {
            bail!("VALKEY_URL is set but SESSION_ID is missing — both are required for pub/sub")
        }
        _ => Ok(None), // No pub/sub — REPL-only mode
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 1. Resolve auth (may be None if user relies on config-dir OAuth)
    let auth = resolve_auth().ok();

    // 2. Validate extra-env
    let extra_env = parse_extra_env(&cli.extra_env)?;

    // 3. Resolve pub/sub
    let pubsub_config = resolve_pubsub()?;

    // 4. Config dir isolation
    //    - Pod mode (pub/sub): use temp dir for security isolation
    //    - REPL mode (local): use default ~/.claude/ so OAuth credentials work
    let config_dir = if pubsub_config.is_some() {
        Some(tempfile::TempDir::new().context("failed to create temp config dir")?)
    } else {
        None
    };

    // 5. Connect pub/sub if configured
    let pubsub = if let Some(ref ps_config) = pubsub_config {
        eprintln!(
            "[info] pub/sub enabled for session {}",
            ps_config.session_id
        );
        Some(
            PubSubClient::connect(&ps_config.url, &ps_config.session_id)
                .await
                .context("failed to connect to Valkey")?,
        )
    } else {
        eprintln!("[info] REPL-only mode (no VALKEY_URL)");
        None
    };

    // 6. Resolve initial prompt
    //    In pod mode (pubsub active), prompt is optional — agent starts idle
    //    and waits for messages via pub/sub.
    //    In local REPL mode, prompt is required (read from -p flag or stdin).
    let initial_prompt = if let Some(prompt) = cli.prompt {
        Some(prompt)
    } else if pubsub_config.is_some() {
        // Pod mode: no prompt needed — agent starts idle, waits for pub/sub input
        None
    } else {
        eprintln!("Enter your prompt (then press Enter):");
        eprint!("> ");
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("failed to read initial prompt from stdin")?;
        let trimmed = line.trim().to_owned();
        if trimmed.is_empty() {
            bail!("no initial prompt provided");
        }
        Some(trimmed)
    };

    // 7. Generate MCP config (if platform vars available and not disabled)
    let mcp_config_path = if !cli.no_mcp {
        if let Some(mcp_config) = mcp::resolve_mcp_config() {
            let mcp_dir = config_dir
                .as_ref()
                .map(|d| d.path().to_path_buf())
                .unwrap_or_else(std::env::temp_dir);
            let path = mcp::write_mcp_config(&mcp_dir, &mcp_config)
                .context("failed to write MCP config")?;
            eprintln!("[info] MCP config written to {}", path.display());
            Some(path)
        } else {
            None
        }
    } else {
        eprintln!("[info] MCP disabled via --no-mcp");
        None
    };

    // 8. Build spawn options
    let allowed_tools = cli
        .allowed_tools
        .map(|s| s.split(',').map(|t| t.trim().to_owned()).collect());

    let (oauth_token, anthropic_api_key) = match &auth {
        Some(AuthToken::OAuth(t)) => (Some(t.clone()), None),
        Some(AuthToken::ApiKey(k)) => (None, Some(k.clone())),
        None => (None, None),
    };

    let is_pod_mode = pubsub_config.is_some();

    // Resolve initial_session_id from SESSION_ID env var (pod mode sets this)
    let initial_session_id = pubsub_config
        .as_ref()
        .map(|ps| ps.session_id.clone());

    let opts = CliSpawnOptions {
        cli_path: cli.cli_path,
        cwd: cli.cwd,
        model: cli.model,
        system_prompt: cli.system_prompt,
        max_turns: cli.max_turns,
        permission_mode: cli.permission_mode,
        allowed_tools,
        config_dir: config_dir.as_ref().map(|d| d.path().to_path_buf()),
        oauth_token,
        anthropic_api_key,
        extra_env,
        isolate_env: is_pod_mode,
        mcp_config: mcp_config_path,
        dangerously_skip_permissions: cli.dangerously_skip_permissions,
        initial_session_id,
        ..Default::default()
    };

    // 9. Spawn stdin reader and pass channel to REPL
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(32);
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let reader = tokio::io::BufReader::new(stdin);
        let mut lines = tokio::io::AsyncBufReadExt::lines(reader);
        while let Ok(Some(line)) = lines.next_line().await {
            if stdin_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    // 10. Run per-turn spawn REPL (opts are cloned per turn)
    repl::run(opts, pubsub, initial_prompt, stdin_rx).await?;

    // config_dir is dropped here (auto-cleanup)
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serial_test::serial;

    // -- Auth resolution tests --

    #[test]
    fn auth_from_oauth_env() {
        let token = AuthToken::OAuth("tok".into());
        assert!(matches!(token, AuthToken::OAuth(ref t) if t == "tok"));
    }

    #[test]
    fn auth_from_api_key_env() {
        let token = AuthToken::ApiKey("key".into());
        assert!(matches!(token, AuthToken::ApiKey(ref k) if k == "key"));
    }

    #[test]
    #[serial]
    fn no_auth_fails() {
        let oauth_backup = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
        let api_backup = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");

        let result = resolve_auth();
        assert!(result.is_err());

        // Restore
        if let Some(v) = oauth_backup {
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", v);
        }
        if let Some(v) = api_backup {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }

    #[test]
    #[serial]
    fn oauth_takes_precedence() {
        let oauth_backup = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
        let api_backup = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "oauth-val");
        std::env::set_var("ANTHROPIC_API_KEY", "api-val");

        let result = resolve_auth().unwrap();
        assert!(matches!(result, AuthToken::OAuth(ref t) if t == "oauth-val"));

        // Restore
        match oauth_backup {
            Some(v) => std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", v),
            None => std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN"),
        }
        match api_backup {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
    }

    // R9: Empty string OAuth falls through to API key
    #[test]
    #[serial]
    fn auth_empty_oauth_falls_to_api_key() {
        let oauth_backup = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
        let api_backup = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "");
        std::env::set_var("ANTHROPIC_API_KEY", "api-val");

        let result = resolve_auth().unwrap();
        assert!(matches!(result, AuthToken::ApiKey(ref k) if k == "api-val"));

        match oauth_backup {
            Some(v) => std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", v),
            None => std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN"),
        }
        match api_backup {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
    }

    // R9: Both empty strings → error
    #[test]
    #[serial]
    fn auth_empty_both_fails() {
        let oauth_backup = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
        let api_backup = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "");
        std::env::set_var("ANTHROPIC_API_KEY", "");

        let result = resolve_auth();
        assert!(result.is_err());

        match oauth_backup {
            Some(v) => std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", v),
            None => std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN"),
        }
        match api_backup {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
    }

    // -- Pub/sub config tests --

    #[test]
    #[serial]
    fn valkey_without_session_id_fails() {
        let url_backup = std::env::var("VALKEY_URL").ok();
        let sid_backup = std::env::var("SESSION_ID").ok();
        std::env::set_var("VALKEY_URL", "redis://localhost:6379");
        std::env::remove_var("SESSION_ID");

        let result = resolve_pubsub();
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("SESSION_ID"));

        match url_backup {
            Some(v) => std::env::set_var("VALKEY_URL", v),
            None => std::env::remove_var("VALKEY_URL"),
        }
        if let Some(v) = sid_backup {
            std::env::set_var("SESSION_ID", v);
        }
    }

    #[test]
    #[serial]
    fn session_id_without_valkey_ok() {
        let url_backup = std::env::var("VALKEY_URL").ok();
        let sid_backup = std::env::var("SESSION_ID").ok();
        std::env::remove_var("VALKEY_URL");
        std::env::set_var("SESSION_ID", "test-123");

        let result = resolve_pubsub().unwrap();
        assert!(result.is_none());

        match url_backup {
            Some(v) => std::env::set_var("VALKEY_URL", v),
            None => std::env::remove_var("VALKEY_URL"),
        }
        match sid_backup {
            Some(v) => std::env::set_var("SESSION_ID", v),
            None => std::env::remove_var("SESSION_ID"),
        }
    }

    #[test]
    #[serial]
    fn valkey_with_session_id() {
        let url_backup = std::env::var("VALKEY_URL").ok();
        let sid_backup = std::env::var("SESSION_ID").ok();
        std::env::set_var("VALKEY_URL", "redis://localhost:6379");
        std::env::set_var("SESSION_ID", "test-456");

        let result = resolve_pubsub().unwrap();
        assert!(result.is_some());
        let config = result.unwrap();
        assert_eq!(config.url, "redis://localhost:6379");
        assert_eq!(config.session_id, "test-456");

        match url_backup {
            Some(v) => std::env::set_var("VALKEY_URL", v),
            None => std::env::remove_var("VALKEY_URL"),
        }
        match sid_backup {
            Some(v) => std::env::set_var("SESSION_ID", v),
            None => std::env::remove_var("SESSION_ID"),
        }
    }

    // R9: Empty VALKEY_URL treated as absent
    #[test]
    #[serial]
    fn pubsub_empty_valkey_url_returns_none() {
        let url_backup = std::env::var("VALKEY_URL").ok();
        let sid_backup = std::env::var("SESSION_ID").ok();
        std::env::set_var("VALKEY_URL", "");
        std::env::set_var("SESSION_ID", "test-789");

        let result = resolve_pubsub().unwrap();
        assert!(result.is_none());

        match url_backup {
            Some(v) => std::env::set_var("VALKEY_URL", v),
            None => std::env::remove_var("VALKEY_URL"),
        }
        match sid_backup {
            Some(v) => std::env::set_var("SESSION_ID", v),
            None => std::env::remove_var("SESSION_ID"),
        }
    }

    // R9: Empty SESSION_ID with valid VALKEY_URL → error
    #[test]
    #[serial]
    fn pubsub_empty_session_id_fails() {
        let url_backup = std::env::var("VALKEY_URL").ok();
        let sid_backup = std::env::var("SESSION_ID").ok();
        std::env::set_var("VALKEY_URL", "redis://localhost:6379");
        std::env::set_var("SESSION_ID", "");

        let result = resolve_pubsub();
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("SESSION_ID"));

        match url_backup {
            Some(v) => std::env::set_var("VALKEY_URL", v),
            None => std::env::remove_var("VALKEY_URL"),
        }
        match sid_backup {
            Some(v) => std::env::set_var("SESSION_ID", v),
            None => std::env::remove_var("SESSION_ID"),
        }
    }

    // -- CLI arg parsing tests --

    #[test]
    fn parse_model_flag() {
        let cli = Cli::try_parse_from(["agent-runner", "--model", "opus"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("opus"));
    }

    #[test]
    fn parse_system_prompt() {
        let cli =
            Cli::try_parse_from(["agent-runner", "--system-prompt", "You are helpful"]).unwrap();
        assert_eq!(cli.system_prompt.as_deref(), Some("You are helpful"));
    }

    #[test]
    fn parse_max_turns() {
        let cli = Cli::try_parse_from(["agent-runner", "--max-turns", "10"]).unwrap();
        assert_eq!(cli.max_turns, Some(10));
    }

    #[test]
    fn parse_allowed_tools() {
        let cli = Cli::try_parse_from(["agent-runner", "--allowed-tools", "Read,Write"]).unwrap();
        assert_eq!(cli.allowed_tools.as_deref(), Some("Read,Write"));
    }

    #[test]
    fn parse_permission_mode() {
        let cli = Cli::try_parse_from(["agent-runner", "--permission-mode", "bypassPermissions"])
            .unwrap();
        assert_eq!(cli.permission_mode.as_deref(), Some("bypassPermissions"));
    }

    #[test]
    fn parse_cwd() {
        let cli = Cli::try_parse_from(["agent-runner", "--cwd", "/tmp"]).unwrap();
        assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn parse_no_mcp_flag() {
        let cli = Cli::try_parse_from(["agent-runner", "--no-mcp"]).unwrap();
        assert!(cli.no_mcp);
    }

    #[test]
    fn no_mcp_flag_default_false() {
        let cli = Cli::try_parse_from(["agent-runner"]).unwrap();
        assert!(!cli.no_mcp);
    }

    // -- Extra env tests --

    #[test]
    fn parse_extra_env_single() {
        let pairs = parse_extra_env(&["KEY=VALUE".into()]).unwrap();
        assert_eq!(pairs, vec![("KEY".into(), "VALUE".into())]);
    }

    #[test]
    fn parse_extra_env_multiple() {
        let pairs = parse_extra_env(&["A=1".into(), "B=2".into()]).unwrap();
        assert_eq!(
            pairs,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn parse_extra_env_invalid_no_equals() {
        let result = parse_extra_env(&["NOEQUALS".into()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn extra_env_reserved_var_rejected() {
        let result = parse_extra_env(&["ANTHROPIC_API_KEY=x".into()]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn extra_env_reserved_path_rejected() {
        let result = parse_extra_env(&["PATH=/x".into()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("PATH"));
    }

    // R19: Empty array input
    #[test]
    fn parse_extra_env_empty() {
        let pairs = parse_extra_env(&[]).unwrap();
        assert!(pairs.is_empty());
    }

    // R19: Value containing '=' (split at first '=' only)
    #[test]
    fn parse_extra_env_value_with_equals() {
        let pairs = parse_extra_env(&["KEY=a=b".into()]).unwrap();
        assert_eq!(pairs, vec![("KEY".into(), "a=b".into())]);
    }

    // R19/R3: All reserved vars rejected (including new proxy/Node.js vars)
    #[test]
    fn extra_env_all_reserved_vars_rejected() {
        for var in RESERVED_ENV_VARS {
            let input = format!("{var}=x");
            let result = parse_extra_env(&[input]);
            assert!(result.is_err(), "expected {var} to be rejected");
        }
    }

    #[test]
    fn parse_prompt_flag() {
        let cli = Cli::try_parse_from(["agent-runner", "-p", "fix the bug"]).unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn parse_prompt_long_flag() {
        let cli = Cli::try_parse_from(["agent-runner", "--prompt", "analyze code"]).unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("analyze code"));
    }

    #[test]
    fn parse_cli_path_flag() {
        let cli =
            Cli::try_parse_from(["agent-runner", "--cli-path", "/usr/local/bin/claude"]).unwrap();
        assert_eq!(
            cli.cli_path.as_deref(),
            Some(std::path::Path::new("/usr/local/bin/claude"))
        );
    }

    #[test]
    fn parse_extra_env_flag() {
        let cli = Cli::try_parse_from([
            "agent-runner",
            "--extra-env",
            "FOO=bar",
            "--extra-env",
            "BAZ=qux",
        ])
        .unwrap();
        assert_eq!(cli.extra_env, vec!["FOO=bar", "BAZ=qux"]);
    }

    #[test]
    fn parse_all_flags_combined() {
        let cli = Cli::try_parse_from([
            "agent-runner",
            "-p",
            "hello",
            "--model",
            "opus",
            "--system-prompt",
            "be helpful",
            "--max-turns",
            "5",
            "--permission-mode",
            "bypassPermissions",
            "--allowed-tools",
            "Read,Write",
            "--cwd",
            "/tmp",
            "--no-mcp",
        ])
        .unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("hello"));
        assert_eq!(cli.model.as_deref(), Some("opus"));
        assert_eq!(cli.system_prompt.as_deref(), Some("be helpful"));
        assert_eq!(cli.max_turns, Some(5));
        assert_eq!(cli.permission_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(cli.allowed_tools.as_deref(), Some("Read,Write"));
        assert!(cli.no_mcp);
    }

    #[test]
    fn parse_defaults() {
        let cli = Cli::try_parse_from(["agent-runner"]).unwrap();
        assert!(cli.prompt.is_none());
        assert!(cli.model.is_none());
        assert!(cli.system_prompt.is_none());
        assert!(cli.max_turns.is_none());
        assert!(cli.permission_mode.is_none());
        assert!(cli.allowed_tools.is_none());
        assert!(cli.cwd.is_none());
        assert!(cli.cli_path.is_none());
        assert!(cli.extra_env.is_empty());
        assert!(!cli.no_mcp);
    }

    // Extra env: multiple reserved vars in one call
    #[test]
    fn extra_env_rejects_first_reserved_in_list() {
        let result = parse_extra_env(&[
            "GOOD=ok".into(),
            "HTTP_PROXY=evil.com".into(),
            "ALSO_GOOD=fine".into(),
        ]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HTTP_PROXY"));
    }

    // Test reserved proxy vars specifically
    #[test]
    fn extra_env_proxy_vars_all_rejected() {
        let proxy_vars = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ];
        for var in proxy_vars {
            let result = parse_extra_env(&[format!("{var}=http://evil.com")]);
            assert!(result.is_err(), "expected proxy var {var} to be rejected");
        }
    }

    // Test Node.js security vars
    #[test]
    fn extra_env_node_vars_rejected() {
        let result = parse_extra_env(&["NODE_OPTIONS=--inspect".into()]);
        assert!(result.is_err());
        let result = parse_extra_env(&["NODE_EXTRA_CA_CERTS=/tmp/evil.pem".into()]);
        assert!(result.is_err());
    }

    // Test TLS vars
    #[test]
    fn extra_env_tls_vars_rejected() {
        let result = parse_extra_env(&["SSL_CERT_FILE=/tmp/evil.pem".into()]);
        assert!(result.is_err());
        let result = parse_extra_env(&["SSL_CERT_DIR=/tmp/evil-certs".into()]);
        assert!(result.is_err());
    }
}
