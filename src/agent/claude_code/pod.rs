use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, EnvVarSource, Pod, PodSpec, ResourceRequirements,
    SecretKeySelector, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use crate::agent::provider::{AgentSession, BrowserConfig, ProviderConfig};

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
}

/// Resolves the container image for an agent pod.
///
/// Priority: session config > project default > platform default
fn resolve_image(config: &ProviderConfig, project_image: Option<&str>) -> String {
    config
        .image
        .as_deref()
        .or(project_image)
        .unwrap_or("platform-claude-runner:latest")
        .to_string()
}

/// Determines the image pull policy based on the image tag.
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

    let claude_args = build_claude_args(params, &branch);
    let env_vars = build_env_vars(params, session_id, &branch);
    let init_containers = build_init_containers(params, &branch);
    let resolved_image = resolve_image(params.config, params.project_agent_image);
    let pull_policy = image_pull_policy(&resolved_image);
    let main_container = build_main_container(claude_args, env_vars, &resolved_image, &pull_policy);

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
            init_containers: Some(init_containers),
            containers,
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_claude_args(params: &PodBuildParams<'_>, _branch: &str) -> Vec<String> {
    let mut args = vec![
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--permission-mode".to_owned(),
        "auto-accept-only".to_owned(),
        "--mcp-config".to_owned(),
        "/tmp/mcp-config.json".to_owned(),
    ];
    if let Some(ref model) = params.config.model {
        args.push("--model".to_owned());
        args.push(model.clone());
    }
    if let Some(max_turns) = params.config.max_turns {
        args.push("--max-turns".to_owned());
        args.push(max_turns.to_string());
    }
    args.push(params.session.prompt.clone());
    args
}

fn build_env_vars(
    params: &PodBuildParams<'_>,
    session_id: uuid::Uuid,
    branch: &str,
) -> Vec<EnvVar> {
    let role = params.config.role.as_deref().unwrap_or("dev");

    // Use user-provided key if available, otherwise fall back to global K8s secret
    let api_key_env = match params.anthropic_api_key {
        Some(key) => env_var("ANTHROPIC_API_KEY", key),
        None => EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: "platform-agent-secrets".into(),
                    key: "anthropic-api-key".into(),
                    optional: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    };

    let mut vars = vec![
        api_key_env,
        env_var("SESSION_ID", &session_id.to_string()),
        env_var("PLATFORM_API_TOKEN", params.agent_api_token),
        env_var("PLATFORM_API_URL", params.platform_api_url),
        env_var("BRANCH", branch),
        env_var("AGENT_ROLE", role),
    ];
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
    vars
}

fn build_init_containers(params: &PodBuildParams<'_>, branch: &str) -> Vec<Container> {
    let mut containers = vec![build_git_clone_container(params.repo_clone_url, branch)];

    // Optional setup container (runs after clone, before claude)
    if let Some(ref commands) = params.config.setup_commands
        && !commands.is_empty()
    {
        let resolved_image = resolve_image(params.config, params.project_agent_image);
        let joined = commands.join(" && ");
        containers.push(Container {
            name: "setup".into(),
            image: Some(resolved_image),
            command: Some(vec!["sh".into(), "-c".into(), joined]),
            working_dir: Some("/workspace".into()),
            volume_mounts: Some(vec![workspace_mount()]),
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

fn build_git_clone_container(repo_clone_url: &str, branch: &str) -> Container {
    Container {
        name: "git-clone".into(),
        image: Some("alpine/git:latest".into()),
        command: Some(vec!["sh".into(), "-c".into()]),
        args: Some(vec![format!(
            "set -eu; git clone {repo_clone_url} /workspace; cd /workspace; \
             git checkout -b {branch}; \
             git config user.name 'platform-agent'; \
             git config user.email 'agent@platform.local'",
        )]),
        volume_mounts: Some(vec![workspace_mount()]),
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
    claude_args: Vec<String>,
    env_vars: Vec<EnvVar>,
    image: &str,
    pull_policy: &str,
) -> Container {
    Container {
        name: "claude".into(),
        image: Some(image.to_owned()),
        image_pull_policy: Some(pull_policy.to_owned()),
        args: Some(claude_args),
        stdin: Some(true),
        tty: Some(false),
        working_dir: Some("/workspace".into()),
        env: Some(env_vars),
        volume_mounts: Some(vec![workspace_mount()]),
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "platform-agents",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
    fn main_container_has_stdin_enabled() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
        });
        let spec = pod.spec.unwrap();
        let claude_container = &spec.containers[0];
        assert_eq!(claude_container.name, "claude");
        assert_eq!(claude_container.stdin, Some(true));
        assert_eq!(claude_container.tty, Some(false));
    }

    #[test]
    fn anthropic_key_from_secret_ref() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
        });
        let spec = pod.spec.unwrap();
        let env = spec.containers[0].env.as_ref().unwrap();
        let api_key_env = env.iter().find(|e| e.name == "ANTHROPIC_API_KEY").unwrap();
        // Must come from a K8s Secret, not a hardcoded value
        assert!(api_key_env.value.is_none());
        let secret_ref = api_key_env
            .value_from
            .as_ref()
            .unwrap()
            .secret_key_ref
            .as_ref()
            .unwrap();
        assert_eq!(secret_ref.name, "platform-agent-secrets");
        assert_eq!(secret_ref.key, "anthropic-api-key");
        assert_eq!(secret_ref.optional, Some(true));
    }

    #[test]
    fn anthropic_key_from_user_provided() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: Some("sk-ant-user-key-1234"),
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
    fn mcp_config_in_claude_args() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
        });
        let spec = pod.spec.unwrap();
        let args = spec.containers[0].args.as_ref().unwrap();
        assert!(args.contains(&"--mcp-config".to_owned()));
        assert!(args.contains(&"/tmp/mcp-config.json".to_owned()));
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "file:///data/repos/myproject",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
        });
        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        assert_eq!(init.name, "git-clone");
        let args = init.args.as_ref().unwrap();
        assert!(args[0].contains("git clone file:///data/repos/myproject /workspace"));
        assert!(args[0].contains("git checkout -b agent/12345678"));
    }

    #[test]
    fn resource_limits_set() {
        let session = test_session();
        let pod = build_agent_pod(&PodBuildParams {
            session: &session,
            config: &ProviderConfig::default(),
            agent_api_token: "tok",
            platform_api_url: "http://platform:8080",
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
        assert_eq!(resolve_image(&config, Some("rust:1.80")), "golang:1.23");
    }

    #[test]
    fn resolve_image_project_default() {
        let config = ProviderConfig::default();
        assert_eq!(resolve_image(&config, Some("rust:1.80")), "rust:1.80");
    }

    #[test]
    fn resolve_image_platform_fallback() {
        let config = ProviderConfig::default();
        assert_eq!(
            resolve_image(&config, None),
            "platform-claude-runner:latest"
        );
    }

    #[test]
    fn pull_policy_latest_is_always() {
        assert_eq!(image_pull_policy("golang:latest"), "Always");
        assert_eq!(image_pull_policy("golang"), "Always"); // no tag = latest
    }

    #[test]
    fn pull_policy_tagged_is_if_not_present() {
        assert_eq!(image_pull_policy("golang:1.23"), "IfNotPresent");
        assert_eq!(image_pull_policy("image@sha256:abc123"), "IfNotPresent");
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: Some("rust:1.80"),
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
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
            repo_clone_url: "file:///data/repos/test",
            namespace: "ns",
            project_agent_image: None,
            anthropic_api_key: None,
        });
        let spec = pod.spec.unwrap();
        let browser = &spec.containers[1];
        let mounts = browser.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "dshm");
        assert_eq!(mounts[0].mount_path, "/dev/shm");
    }
}
