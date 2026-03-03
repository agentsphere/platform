use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, LocalObjectReference, Pod,
    PodSecurityContext, PodSpec, ResourceRequirements, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use crate::agent::provider::{AgentSession, BrowserConfig, ProviderConfig};

/// Hardened security context for all containers: drop all capabilities, no privilege escalation.
fn container_security() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Parameters for building an agent pod. Grouped into a struct to stay under
/// clippy's 7-argument threshold.
pub struct PodBuildParams<'a> {
    pub session: &'a AgentSession,
    pub config: &'a ProviderConfig,
    pub agent_api_token: &'a str,
    pub platform_api_url: &'a str,
    pub repo_clone_url: &'a str,
    pub namespace: &'a str,
    /// Project-level default agent image (from `projects.agent_image` column).
    pub project_agent_image: Option<&'a str>,
    /// User-provided Anthropic API key. If set, used as a plain env var
    /// instead of referencing the global K8s secret.
    pub anthropic_api_key: Option<&'a str>,
    /// CLI OAuth token for subscription auth. When set, `CLAUDE_CODE_OAUTH_TOKEN` is
    /// injected instead of `ANTHROPIC_API_KEY`, letting the CLI use the user's subscription.
    pub cli_oauth_token: Option<&'a str>,
    /// Extra env vars from project secrets (scope=agent/all), injected into the pod.
    pub extra_env_vars: &'a [(String, String)],
    /// Container registry URL (e.g. `host.docker.internal:8080`). Prefixed to the default agent image.
    pub registry_url: Option<&'a str>,
    /// K8s Secret name for `imagePullSecrets` (registry auth for image pulls).
    pub registry_secret_name: Option<&'a str>,
    /// Valkey URL with per-session ACL credentials for pub/sub.
    pub valkey_url: Option<&'a str>,
}

/// Resolves the container image for an agent pod.
///
/// Priority: session config > project default > registry-prefixed default > bare default
fn resolve_image(
    config: &ProviderConfig,
    project_image: Option<&str>,
    registry_url: Option<&str>,
) -> String {
    if let Some(image) = config.image.as_deref().or(project_image) {
        return image.to_string();
    }
    match registry_url {
        Some(reg) => format!("{reg}/platform-claude-runner:latest"),
        None => "platform-claude-runner:latest".to_string(),
    }
}

/// Determines the image pull policy based on the image tag.
///
/// Uses `Always` for `:latest` tags to ensure the newest image is used.
/// Uses `IfNotPresent` for specific tags (e.g. `v1.2.3`) to avoid unnecessary pulls.
fn image_pull_policy(image: &str) -> String {
    if image.ends_with(":latest") || !image.contains(':') {
        "Always".to_string()
    } else {
        "IfNotPresent".to_string()
    }
}

/// Build the K8s Pod object for a Claude Code agent session.
pub fn build_agent_pod(params: &PodBuildParams<'_>) -> Pod {
    let session_id = params.session.id;
    let project_id = params.session.project_id;
    let short_id = &session_id.to_string()[..8];
    let pod_name = format!("agent-{short_id}");

    let branch = params
        .session
        .branch
        .clone()
        .unwrap_or_else(|| format!("agent/{short_id}"));

    let mut labels = BTreeMap::from([
        ("platform.io/component".into(), "agent-session".into()),
        ("platform.io/session".into(), session_id.to_string()),
    ]);
    if let Some(pid) = project_id {
        labels.insert("platform.io/project".into(), pid.to_string());
    }

    let agent_runner_args = build_agent_runner_args(params);
    let env_vars = build_env_vars(params, session_id, &branch);
    let init_containers = build_init_containers(params, &branch);
    let resolved_image = resolve_image(
        params.config,
        params.project_agent_image,
        params.registry_url,
    );
    let pull_policy = image_pull_policy(&resolved_image);
    let main_container =
        build_main_container(agent_runner_args, env_vars, &resolved_image, &pull_policy);

    let mut containers = vec![main_container];
    let mut volumes = vec![Volume {
        name: "workspace".into(),
        empty_dir: Some(EmptyDirVolumeSource {
            size_limit: Some(Quantity("1Gi".into())),
            ..Default::default()
        }),
        ..Default::default()
    }];

    // Add browser sidecar when browser config is present
    if let Some(ref browser) = params.config.browser {
        containers.push(build_browser_sidecar(browser));
        // Chromium needs a large /dev/shm — add tmpfs-backed emptyDir
        volumes.push(Volume {
            name: "dshm".into(),
            empty_dir: Some(EmptyDirVolumeSource {
                medium: Some("Memory".into()),
                size_limit: Some(Quantity("256Mi".into())),
            }),
            ..Default::default()
        });
    }

    Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(pod_name),
            namespace: Some(params.namespace.to_owned()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(1000),
                fs_group: Some(1000),
                ..Default::default()
            }),
            image_pull_secrets: params.registry_secret_name.map(|name| {
                vec![LocalObjectReference {
                    name: name.to_string(),
                }]
            }),
            init_containers: Some(init_containers),
            containers,
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_agent_runner_args(params: &PodBuildParams<'_>) -> Vec<String> {
    let mut args = vec![
        "--prompt".to_owned(),
        params.session.prompt.clone(),
        "--cwd".to_owned(),
        "/workspace".to_owned(),
        "--permission-mode".to_owned(),
        "bypassPermissions".to_owned(),
    ];
    if let Some(ref model) = params.config.model {
        args.push("--model".to_owned());
        args.push(model.clone());
    }
    if let Some(max_turns) = params.config.max_turns {
        args.push("--max-turns".to_owned());
        args.push(max_turns.to_string());
    }
    args
}

/// Env var names that must not be overridden by project secrets.
/// Overriding these could hijack the agent's identity or redirect API calls.
const RESERVED_ENV_VARS: &[&str] = &[
    "PLATFORM_API_TOKEN",
    "PLATFORM_API_URL",
    "SESSION_ID",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "VALKEY_URL",
    "BRANCH",
    "AGENT_ROLE",
    "PROJECT_ID",
    "GIT_AUTH_TOKEN",
    "GIT_BRANCH",
    "BROWSER_ENABLED",
    "BROWSER_CDP_URL",
    "BROWSER_ALLOWED_ORIGINS",
];

fn is_reserved_env_var(name: &str) -> bool {
    RESERVED_ENV_VARS.contains(&name)
}

fn build_env_vars(
    params: &PodBuildParams<'_>,
    session_id: uuid::Uuid,
    branch: &str,
) -> Vec<EnvVar> {
    let role = params.config.role.as_deref().unwrap_or("dev");

    let mut vars = vec![
        env_var("SESSION_ID", &session_id.to_string()),
        env_var("PLATFORM_API_TOKEN", params.agent_api_token),
        env_var("PLATFORM_API_URL", params.platform_api_url),
        env_var("BRANCH", branch),
        env_var("AGENT_ROLE", role),
    ];

    // Auth priority: CLI OAuth token (subscription) > Anthropic API key.
    // When cli_oauth_token is set, the CLI uses the user's subscription.
    // When only anthropic_api_key is set, the CLI uses the API directly.
    // If neither is set, the env vars are omitted (Claude Code will error clearly).
    if let Some(oauth_token) = params.cli_oauth_token {
        vars.push(env_var("CLAUDE_CODE_OAUTH_TOKEN", oauth_token));
    } else if let Some(api_key) = params.anthropic_api_key {
        vars.push(env_var("ANTHROPIC_API_KEY", api_key));
    }
    if let Some(valkey_url) = params.valkey_url {
        vars.push(env_var("VALKEY_URL", valkey_url));
    }
    if let Some(pid) = params.session.project_id {
        vars.push(env_var("PROJECT_ID", &pid.to_string()));
    }
    // Browser sidecar env vars
    if let Some(ref browser) = params.config.browser {
        vars.push(env_var("BROWSER_ENABLED", "true"));
        vars.push(env_var("BROWSER_CDP_URL", "http://localhost:9222"));
        let origins_json =
            serde_json::to_string(&browser.allowed_origins).unwrap_or_else(|_| "[]".into());
        vars.push(env_var("BROWSER_ALLOWED_ORIGINS", &origins_json));
    }
    // Project secrets (scope=agent/all) as extra env vars.
    // Skip reserved names to prevent privilege escalation (e.g. overriding PLATFORM_API_TOKEN).
    for (name, value) in params.extra_env_vars {
        if is_reserved_env_var(name) {
            tracing::warn!(%name, "skipping reserved env var from project secrets");
            continue;
        }
        vars.push(env_var(name, value));
    }
    vars
}

fn build_init_containers(params: &PodBuildParams<'_>, branch: &str) -> Vec<Container> {
    let mut containers = vec![build_git_clone_container(
        params.repo_clone_url,
        branch,
        params.agent_api_token,
    )];

    // Optional setup container (runs after clone, before claude)
    if let Some(ref commands) = params.config.setup_commands
        && !commands.is_empty()
    {
        let resolved_image = resolve_image(
            params.config,
            params.project_agent_image,
            params.registry_url,
        );
        let joined = commands.join(" && ");
        containers.push(Container {
            name: "setup".into(),
            image: Some(resolved_image),
            command: Some(vec!["sh".into(), "-c".into(), joined]),
            working_dir: Some("/workspace".into()),
            volume_mounts: Some(vec![workspace_mount()]),
            security_context: Some(container_security()),
            resources: Some(ResourceRequirements {
                requests: Some(BTreeMap::from([
                    ("cpu".into(), Quantity("200m".into())),
                    ("memory".into(), Quantity("256Mi".into())),
                ])),
                limits: Some(BTreeMap::from([
                    ("cpu".into(), Quantity("500m".into())),
                    ("memory".into(), Quantity("512Mi".into())),
                ])),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    containers
}

fn build_git_clone_container(repo_clone_url: &str, branch: &str, api_token: &str) -> Container {
    // Use GIT_ASKPASS to provide the API token for HTTP auth.
    // The askpass script echoes the token when git prompts for a password.
    // This avoids embedding tokens in clone URLs (which leak to logs/proc/pod spec).
    Container {
        name: "git-clone".into(),
        image: Some("alpine/git:latest".into()),
        command: Some(vec!["sh".into(), "-c".into()]),
        args: Some(vec![format!(
            "set -eu; export HOME=/tmp; \
             printf '#!/bin/sh\\necho \"$GIT_AUTH_TOKEN\"\\n' > /tmp/git-askpass.sh; \
             chmod +x /tmp/git-askpass.sh; \
             git config --global --add safe.directory /workspace; \
             if ! GIT_ASKPASS=/tmp/git-askpass.sh git clone {repo_clone_url} /workspace 2>/dev/null; then \
               git init /workspace; \
               cd /workspace; \
               GIT_ASKPASS=/tmp/git-askpass.sh git remote add origin {repo_clone_url}; \
             else \
               cd /workspace; \
             fi; \
             git checkout \"$GIT_BRANCH\" 2>/dev/null || git checkout -b \"$GIT_BRANCH\"; \
             git config user.name 'platform-agent'; \
             git config user.email 'agent@platform.local'",
        )]),
        env: Some(vec![
            env_var("GIT_AUTH_TOKEN", api_token),
            env_var("GIT_BRANCH", branch),
        ]),
        volume_mounts: Some(vec![workspace_mount()]),
        security_context: Some(container_security()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), Quantity("50m".into())),
                ("memory".into(), Quantity("64Mi".into())),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".into(), Quantity("200m".into())),
                ("memory".into(), Quantity("128Mi".into())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_main_container(
    agent_runner_args: Vec<String>,
    env_vars: Vec<EnvVar>,
    image: &str,
    pull_policy: &str,
) -> Container {
    Container {
        name: "claude".into(),
        image: Some(image.to_owned()),
        image_pull_policy: Some(pull_policy.to_owned()),
        command: Some(vec!["agent-runner".to_owned()]),
        args: Some(agent_runner_args),
        stdin: Some(false),
        tty: Some(false),
        working_dir: Some("/workspace".into()),
        env: Some(env_vars),
        volume_mounts: Some(vec![workspace_mount()]),
        security_context: Some(container_security()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), Quantity("200m".into())),
                ("memory".into(), Quantity("256Mi".into())),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".into(), Quantity("500m".into())),
                ("memory".into(), Quantity("512Mi".into())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the headless Chromium browser sidecar container.
/// Exposes CDP on port 9222 for the Playwright MCP server in the main container.
fn build_browser_sidecar(_browser_config: &BrowserConfig) -> Container {
    use k8s_openapi::api::core::v1::ContainerPort;

    Container {
        name: "browser".into(),
        image: Some("chromedp/headless-shell:131".into()),
        image_pull_policy: Some("IfNotPresent".into()),
        args: Some(vec![
            "--no-sandbox".into(),
            "--disable-gpu".into(),
            "--disable-dev-shm-usage".into(),
            "--remote-debugging-address=0.0.0.0".into(),
            "--remote-debugging-port=9222".into(),
        ]),
        ports: Some(vec![ContainerPort {
            container_port: 9222,
            name: Some("cdp".into()),
            ..Default::default()
        }]),
        volume_mounts: Some(vec![VolumeMount {
            name: "dshm".into(),
            mount_path: "/dev/shm".into(),
            ..Default::default()
        }]),
        security_context: Some(container_security()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), Quantity("200m".into())),
                ("memory".into(), Quantity("512Mi".into())),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".into(), Quantity("1".into())),
                ("memory".into(), Quantity("1Gi".into())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn workspace_mount() -> VolumeMount {
    VolumeMount {
        name: "workspace".into(),
        mount_path: "/workspace".into(),
        ..Default::default()
    }
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::agent::provider::{AgentSession, ProviderConfig};

    fn test_session() -> AgentSession {
        AgentSession {
            id: Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap(),
            project_id: Some(Uuid::parse_str("abcdef01-2345-6789-abcd-ef0123456789").unwrap()),
            user_id: Uuid::new_v4(),
            agent_user_id: None,
            prompt: "Fix the tests".to_owned(),
            status: "pending".to_owned(),
            branch: Some("agent/12345678".to_owned()),
            pod_name: None,
            provider: "claude-code".to_owned(),
            provider_config: None,
            cost_tokens: None,
            created_at: Utc::now(),
            finished_at: None,
            parent_session_id: None,
            spawn_depth: 0,
            allowed_child_roles: None,
            execution_mode: "pod".to_owned(),
            uses_pubsub: false,
        }
    }

    #[test]
    fn pod_name_format() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "plat_api_test",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "platform-agents",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        assert_eq!(pod.metadata.name.as_deref(), Some("agent-12345678"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("platform-agents"));
    }

    #[test]
    fn pod_has_correct_labels() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["platform.io/component"], "agent-session");
        assert_eq!(labels["platform.io/session"], session.id.to_string());
        assert_eq!(
            labels["platform.io/project"],
            session.project_id.unwrap().to_string()
        );
    }

    #[test]
    fn main_container_has_stdin_disabled() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let claude_container = &spec.containers[0];
        assert_eq!(claude_container.name, "claude");
        assert_eq!(claude_container.stdin, Some(false));
        assert_eq!(claude_container.tty, Some(false));
    }

    #[test]
    fn no_api_key_omits_env_var() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        // When no API key is provided, ANTHROPIC_API_KEY should be absent entirely
        assert!(
            env.iter().all(|e| e.name != "ANTHROPIC_API_KEY"),
            "ANTHROPIC_API_KEY should not be present when no key is provided"
        );
    }

    #[test]
    fn anthropic_key_from_user_provided() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: Some("sk-ant-user-key-1234"),
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let api_key_env = env.iter().find(|e| e.name == "ANTHROPIC_API_KEY").unwrap();
        // User-provided key should be a plain value, not a secret ref
        assert_eq!(api_key_env.value.as_deref(), Some("sk-ant-user-key-1234"));
        assert!(api_key_env.value_from.is_none());
    }

    #[test]
    fn env_vars_include_session_data() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "plat_api_xyz",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.as_deref())
        };
        assert_eq!(get("SESSION_ID"), Some(&*session.id.to_string()));
        assert_eq!(get("PLATFORM_API_TOKEN"), Some("plat_api_xyz"));
        assert_eq!(get("PLATFORM_API_URL"), Some("http://platform:8080"));
        assert_eq!(get("BRANCH"), Some("agent/12345678"));
        assert_eq!(
            get("PROJECT_ID"),
            Some(&*session.project_id.unwrap().to_string())
        );
        assert_eq!(get("AGENT_ROLE"), Some("dev"));
    }

    #[test]
    fn agent_role_from_config() {
        let session = test_session();
        let config = ProviderConfig {
            role: Some("ops".into()),
            ..Default::default()
        };
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let role = env
            .iter()
            .find(|e| e.name == "AGENT_ROLE")
            .and_then(|e| e.value.as_deref());
        assert_eq!(role, Some("ops"));
    }

    #[test]
    fn no_mcp_config_in_claude_args() {
        // MCP servers are disabled due to a Claude CLI compatibility issue
        // where --mcp-config causes the process to hang indefinitely.
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let args = spec.containers[0].args.as_ref().unwrap();
        assert!(!args.contains(&"--mcp-config".to_owned()));
    }

    #[test]
    fn model_and_max_turns_in_args() {
        let session = test_session();
        let config = ProviderConfig {
            model: Some("claude-sonnet-4-5-20250929".into()),
            max_turns: Some(25),
            ..Default::default()
        };
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let args = spec.containers[0].args.as_ref().unwrap();
        assert!(args.contains(&"--model".to_owned()));
        assert!(args.contains(&"claude-sonnet-4-5-20250929".to_owned()));
        assert!(args.contains(&"--max-turns".to_owned()));
        assert!(args.contains(&"25".to_owned()));
    }

    #[test]
    fn init_container_clones_repo() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "plat_api_test",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/myproject.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        assert_eq!(init.name, "git-clone");
        let args = init.args.as_ref().unwrap();
        assert!(
            args[0].contains("git clone http://platform:8080/owner/myproject.git /workspace"),
            "should clone via HTTP, got: {}",
            args[0]
        );
        assert!(
            args[0].contains("GIT_ASKPASS=/tmp/git-askpass.sh"),
            "should use GIT_ASKPASS for auth, got: {}",
            args[0]
        );
        // Branch is passed via env var, not interpolated into shell command
        assert!(
            args[0].contains(
                "git checkout \"$GIT_BRANCH\" 2>/dev/null || git checkout -b \"$GIT_BRANCH\""
            ),
            "should reference $GIT_BRANCH env var, got: {}",
            args[0]
        );
        // Verify env vars are set on init container
        let env = init.env.as_ref().unwrap();
        let token_env = env.iter().find(|e| e.name == "GIT_AUTH_TOKEN").unwrap();
        assert_eq!(token_env.value.as_deref(), Some("plat_api_test"));
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH").unwrap();
        assert_eq!(branch_env.value.as_deref(), Some("agent/12345678"));
    }

    #[test]
    fn init_container_no_token_in_clone_url() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "plat_secret_token",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let args = init.args.as_ref().unwrap();
        assert!(
            !args[0].contains("plat_secret_token"),
            "token must not appear in clone command args"
        );
    }

    #[test]
    fn branch_passed_as_env_var_not_in_shell_args() {
        let mut session = test_session();
        session.branch = Some("feat/$(malicious-cmd)".into());
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let args = init.args.as_ref().unwrap();
        // Branch name must NOT appear in the shell command (prevents injection)
        assert!(
            !args[0].contains("$(malicious-cmd)"),
            "branch must not be interpolated into shell args, got: {}",
            args[0]
        );
        // Branch should be referenced via $GIT_BRANCH env var
        assert!(
            args[0].contains("$GIT_BRANCH"),
            "should reference $GIT_BRANCH env var, got: {}",
            args[0]
        );
        // GIT_BRANCH env var should be set with the actual branch value
        let env = init.env.as_ref().unwrap();
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH").unwrap();
        assert_eq!(branch_env.value.as_deref(), Some("feat/$(malicious-cmd)"));
    }

    #[test]
    fn resource_limits_set() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let resources = spec.containers[0].resources.as_ref().unwrap();
        let limits = resources.limits.as_ref().unwrap();
        assert_eq!(limits["cpu"], Quantity("500m".into()));
        assert_eq!(limits["memory"], Quantity("512Mi".into()));
    }

    #[test]
    fn restart_policy_never() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
    }

    // -- Image resolution tests --

    #[test]
    fn resolve_image_session_override() {
        let config = ProviderConfig {
            image: Some("golang:1.23".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_image(&config, Some("rust:1.80"), None),
            "golang:1.23"
        );
    }

    #[test]
    fn resolve_image_project_default() {
        let config = ProviderConfig::default();
        assert_eq!(resolve_image(&config, Some("rust:1.80"), None), "rust:1.80");
    }

    #[test]
    fn resolve_image_platform_fallback() {
        let config = ProviderConfig::default();
        assert_eq!(
            resolve_image(&config, None, None),
            "platform-claude-runner:latest"
        );
    }

    #[test]
    fn resolve_image_registry_prefix() {
        let config = ProviderConfig::default();
        assert_eq!(
            resolve_image(&config, None, Some("localhost:5001")),
            "localhost:5001/platform-claude-runner:latest"
        );
    }

    #[test]
    fn resolve_image_registry_ignored_when_explicit() {
        let config = ProviderConfig {
            image: Some("custom:v1".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_image(&config, None, Some("localhost:5001")),
            "custom:v1"
        );
    }

    #[test]
    fn pull_policy_latest_uses_always() {
        assert_eq!(image_pull_policy("golang:latest"), "Always");
        assert_eq!(image_pull_policy("golang"), "Always"); // no tag = latest
        assert_eq!(
            image_pull_policy("kind-registry:5000/platform-claude-runner:latest"),
            "Always"
        );
    }

    #[test]
    fn pull_policy_specific_tag_uses_if_not_present() {
        assert_eq!(image_pull_policy("golang:1.23"), "IfNotPresent");
        assert_eq!(image_pull_policy("image@sha256:abc123"), "IfNotPresent");
        assert_eq!(image_pull_policy("myapp:v2.1.0"), "IfNotPresent");
    }

    #[test]
    fn main_container_uses_resolved_image() {
        let session = test_session();
        let config = ProviderConfig {
            image: Some("golang:1.23".into()),
            ..Default::default()
        };
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let main = &spec.containers[0];
        assert_eq!(main.image.as_deref(), Some("golang:1.23"));
        assert_eq!(main.image_pull_policy.as_deref(), Some("IfNotPresent"));
    }

    #[test]
    fn main_container_uses_project_image() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: Some("rust:1.80"),
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let main = &spec.containers[0];
        assert_eq!(main.image.as_deref(), Some("rust:1.80"));
        assert_eq!(main.image_pull_policy.as_deref(), Some("IfNotPresent"));
    }

    #[test]
    fn setup_container_added_when_commands_present() {
        let session = test_session();
        let config = ProviderConfig {
            setup_commands: Some(vec!["npm install".into(), "npm run build".into()]),
            ..Default::default()
        };
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = spec.init_containers.unwrap();
        assert_eq!(init.len(), 2); // git-clone + setup
        assert_eq!(init[0].name, "git-clone");
        assert_eq!(init[1].name, "setup");
        let cmd = init[1].command.as_ref().unwrap();
        assert_eq!(cmd[2], "npm install && npm run build");
    }

    #[test]
    fn no_setup_container_when_commands_empty() {
        let session = test_session();
        let config = ProviderConfig {
            setup_commands: Some(vec![]),
            ..Default::default()
        };
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = spec.init_containers.unwrap();
        assert_eq!(init.len(), 1); // git-clone only
    }

    #[test]
    fn no_setup_container_when_commands_none() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = spec.init_containers.unwrap();
        assert_eq!(init.len(), 1); // git-clone only
    }

    // -- Browser sidecar tests --

    fn browser_config() -> ProviderConfig {
        ProviderConfig {
            role: Some("ui".into()),
            browser: Some(crate::agent::provider::BrowserConfig {
                allowed_origins: vec!["http://localhost:3000".into(), "http://myapp:8080".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn browser_sidecar_added_when_config_present() {
        let session = test_session();
        let config = browser_config();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        assert_eq!(spec.containers.len(), 2, "should have claude + browser");
        assert_eq!(spec.containers[0].name, "claude");
        assert_eq!(spec.containers[1].name, "browser");
    }

    #[test]
    fn browser_sidecar_not_added_when_absent() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        assert_eq!(spec.containers.len(), 1, "should have only claude");
    }

    #[test]
    fn dshm_volume_added_for_browser() {
        let session = test_session();
        let config = browser_config();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let volumes = spec.volumes.unwrap();
        assert_eq!(volumes.len(), 2, "should have workspace + dshm");
        let dshm = &volumes[1];
        assert_eq!(dshm.name, "dshm");
        let empty_dir = dshm.empty_dir.as_ref().unwrap();
        assert_eq!(empty_dir.medium.as_deref(), Some("Memory"));
        assert_eq!(
            empty_dir.size_limit.as_ref().unwrap(),
            &Quantity("256Mi".into())
        );
    }

    #[test]
    fn no_dshm_volume_without_browser() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let volumes = spec.volumes.unwrap();
        assert_eq!(volumes.len(), 1, "should have only workspace");
    }

    #[test]
    fn browser_env_vars_set_when_enabled() {
        let session = test_session();
        let config = browser_config();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.as_deref())
        };
        assert_eq!(get("BROWSER_ENABLED"), Some("true"));
        assert_eq!(get("BROWSER_CDP_URL"), Some("http://localhost:9222"));
        let origins = get("BROWSER_ALLOWED_ORIGINS").unwrap();
        let parsed: Vec<String> = serde_json::from_str(origins).unwrap();
        assert_eq!(parsed, vec!["http://localhost:3000", "http://myapp:8080"]);
    }

    #[test]
    fn browser_env_vars_absent_when_disabled() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        assert!(
            env.iter().all(|e| e.name != "BROWSER_ENABLED"),
            "BROWSER_ENABLED should not be set"
        );
        assert!(
            env.iter().all(|e| e.name != "BROWSER_CDP_URL"),
            "BROWSER_CDP_URL should not be set"
        );
    }

    #[test]
    fn browser_sidecar_has_cdp_port() {
        let session = test_session();
        let config = browser_config();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let browser = &spec.containers[1];
        let ports = browser.ports.as_ref().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, 9222);
        assert_eq!(ports[0].name.as_deref(), Some("cdp"));
    }

    #[test]
    fn browser_sidecar_mounts_dshm() {
        let session = test_session();
        let config = browser_config();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let browser = &spec.containers[1];
        let mounts = browser.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "dshm");
        assert_eq!(mounts[0].mount_path, "/dev/shm");
    }

    // -- SecurityContext --

    #[test]
    fn pod_security_context_runs_as_non_root() {
        let session = test_session();
        let config = ProviderConfig::default();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let psc = spec.security_context.unwrap();
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.run_as_user, Some(1000));
        assert_eq!(psc.fs_group, Some(1000));
    }

    #[test]
    fn main_container_drops_all_capabilities() {
        let session = test_session();
        let config = ProviderConfig::default();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        let sc = container.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        let caps = sc.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_ref().unwrap(), &vec!["ALL".to_string()]);
    }

    #[test]
    fn init_container_drops_all_capabilities() {
        let session = test_session();
        let config = ProviderConfig::default();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &config,
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let sc = init.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        let caps = sc.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_ref().unwrap(), &vec!["ALL".to_string()]);
    }

    // -- Extra env vars (project secrets) tests --

    #[test]
    fn extra_env_vars_injected_into_pod() {
        let session = test_session();
        let secrets = vec![
            ("DATABASE_URL".into(), "postgres://db:5432/app".into()),
            ("API_SECRET".into(), "s3cr3t".into()),
        ];
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &secrets,
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.as_deref())
        };
        assert_eq!(get("DATABASE_URL"), Some("postgres://db:5432/app"));
        assert_eq!(get("API_SECRET"), Some("s3cr3t"));
    }

    #[test]
    fn extra_env_vars_empty_does_not_add_vars() {
        let session = test_session();
        let pod_without = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod_without.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        // Should only have the standard env vars (no extra ones)
        assert!(
            env.iter().all(|e| e.name != "DATABASE_URL"),
            "should not have DATABASE_URL without extra_env_vars"
        );
    }

    #[test]
    fn reserved_env_vars_are_filtered_out() {
        let session = test_session();
        let secrets = vec![
            ("PLATFORM_API_TOKEN".into(), "hijacked-token".into()),
            ("PLATFORM_API_URL".into(), "http://evil.com".into()),
            ("SESSION_ID".into(), "fake-session".into()),
            ("SAFE_VAR".into(), "safe-value".into()),
        ];
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "real-tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &secrets,
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|e| e.name == name)
                .and_then(|e| e.value.as_deref())
        };
        // Reserved vars keep their original values
        assert_eq!(get("PLATFORM_API_TOKEN"), Some("real-tok"));
        assert_eq!(get("PLATFORM_API_URL"), Some("http://platform:8080"));
        // Safe custom var is present
        assert_eq!(get("SAFE_VAR"), Some("safe-value"));
    }

    #[test]
    fn is_reserved_env_var_works() {
        assert!(is_reserved_env_var("PLATFORM_API_TOKEN"));
        assert!(is_reserved_env_var("ANTHROPIC_API_KEY"));
        assert!(is_reserved_env_var("SESSION_ID"));
        assert!(!is_reserved_env_var("MY_CUSTOM_VAR"));
        assert!(!is_reserved_env_var("DATABASE_URL"));
    }

    // -- imagePullSecrets tests --

    #[test]
    fn image_pull_secrets_set_when_provided() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: Some("host.docker.internal:8080"),
            registry_secret_name: Some("regpull-12345678"),
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let secrets = spec.image_pull_secrets.unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "regpull-12345678");
    }

    #[test]
    fn image_pull_secrets_absent_when_none() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        assert!(
            spec.image_pull_secrets.is_none(),
            "imagePullSecrets should be absent when no registry secret is configured"
        );
    }

    // -- CLI OAuth token (subscription auth) tests --

    #[test]
    fn pod_env_includes_oauth_token() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: Some("ccode-oauth-token-12345"),
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let oauth = env
            .iter()
            .find(|e| e.name == "CLAUDE_CODE_OAUTH_TOKEN")
            .expect("CLAUDE_CODE_OAUTH_TOKEN should be set");
        assert_eq!(oauth.value.as_deref(), Some("ccode-oauth-token-12345"));
    }

    #[test]
    fn pod_env_no_api_key_when_oauth_set() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: Some("ccode-oauth-token-12345"),
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        assert!(
            env.iter().all(|e| e.name != "ANTHROPIC_API_KEY"),
            "ANTHROPIC_API_KEY should not be set when OAuth token is present"
        );
    }

    #[test]
    fn pod_env_fallback_to_api_key() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: Some("sk-ant-fallback-key"),
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let api_key = env
            .iter()
            .find(|e| e.name == "ANTHROPIC_API_KEY")
            .expect("ANTHROPIC_API_KEY should be set as fallback");
        assert_eq!(api_key.value.as_deref(), Some("sk-ant-fallback-key"));
        assert!(
            env.iter().all(|e| e.name != "CLAUDE_CODE_OAUTH_TOKEN"),
            "CLAUDE_CODE_OAUTH_TOKEN should not be set when using API key fallback"
        );
    }

    #[test]
    fn oauth_token_is_reserved() {
        assert!(
            is_reserved_env_var("CLAUDE_CODE_OAUTH_TOKEN"),
            "CLAUDE_CODE_OAUTH_TOKEN must be reserved to prevent privilege escalation"
        );
    }

    #[test]
    fn config_dir_is_reserved() {
        assert!(
            is_reserved_env_var("CLAUDE_CONFIG_DIR"),
            "CLAUDE_CONFIG_DIR must be reserved to prevent config hijacking"
        );
    }

    #[test]
    fn both_oauth_and_api_key_prefers_oauth() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: Some("sk-ant-should-be-ignored"),
            cli_oauth_token: Some("ccode-oauth-winner"),
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        // OAuth token should be present
        let oauth = env
            .iter()
            .find(|e| e.name == "CLAUDE_CODE_OAUTH_TOKEN")
            .expect("CLAUDE_CODE_OAUTH_TOKEN should be set");
        assert_eq!(oauth.value.as_deref(), Some("ccode-oauth-winner"));
        // API key should NOT be present (OAuth takes priority)
        assert!(
            env.iter().all(|e| e.name != "ANTHROPIC_API_KEY"),
            "ANTHROPIC_API_KEY should not be set when OAuth token is present"
        );
    }

    #[test]
    fn no_auth_omits_both_env_vars() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "http://platform:8080/owner/test.git",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
            cli_oauth_token: None,
            extra_env_vars: &[],
            registry_url: None,
            registry_secret_name: None,
            valkey_url: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        assert!(
            env.iter().all(|e| e.name != "ANTHROPIC_API_KEY"),
            "ANTHROPIC_API_KEY should not be set when no auth configured"
        );
        assert!(
            env.iter().all(|e| e.name != "CLAUDE_CODE_OAUTH_TOKEN"),
            "CLAUDE_CODE_OAUTH_TOKEN should not be set when no auth configured"
        );
    }
}
