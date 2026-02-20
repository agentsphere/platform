use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, EnvVarSource, Pod, PodSpec, ResourceRequirements,
    SecretKeySelector, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use crate::agent::provider::{AgentSession, ProviderConfig};

/// Parameters for building an agent pod. Grouped into a struct to stay under
/// clippy's 7-argument threshold.
pub struct PodBuildParams<'a> {
    pub session: &'a AgentSession,
    pub config: &'a ProviderConfig,
    pub agent_api_token: &'a str,
    pub platform_api_url: &'a str,
    pub repo_clone_url: &'a str,
    pub namespace: &'a str,
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

    let labels = BTreeMap::from([
        ("platform.io/component".into(), "agent-session".into()),
        ("platform.io/session".into(), session_id.to_string()),
        ("platform.io/project".into(), project_id.to_string()),
    ]);

    let claude_args = build_claude_args(params, &branch);
    let env_vars = build_env_vars(params, session_id, &branch);
    let init_container = build_init_container(params.repo_clone_url, &branch);
    let main_container = build_main_container(claude_args, env_vars);

    Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(pod_name),
            namespace: Some(params.namespace.to_owned()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            init_containers: Some(vec![init_container]),
            containers: vec![main_container],
            volumes: Some(vec![Volume {
                name: "workspace".into(),
                empty_dir: Some(EmptyDirVolumeSource {
                    size_limit: Some(Quantity("1Gi".into())),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
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

    vec![
        EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: "platform-agent-secrets".into(),
                    key: "anthropic-api-key".into(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        env_var("SESSION_ID", &session_id.to_string()),
        env_var("PLATFORM_API_TOKEN", params.agent_api_token),
        env_var("PLATFORM_API_URL", params.platform_api_url),
        env_var("BRANCH", branch),
        env_var("PROJECT_ID", &params.session.project_id.to_string()),
        env_var("AGENT_ROLE", role),
    ]
}

fn build_init_container(repo_clone_url: &str, branch: &str) -> Container {
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

fn build_main_container(claude_args: Vec<String>, env_vars: Vec<EnvVar>) -> Container {
    Container {
        name: "claude".into(),
        image: Some("platform-claude-runner:latest".into()),
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
            project_id: Uuid::parse_str("abcdef01-2345-6789-abcd-ef0123456789").unwrap(),
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
        });
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels["platform.io/component"], "agent-session");
        assert_eq!(labels["platform.io/session"], session.id.to_string());
        assert_eq!(
            labels["platform.io/project"],
            session.project_id.to_string()
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
        assert_eq!(get("PROJECT_ID"), Some(&*session.project_id.to_string()));
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
        });
        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
    }
}
