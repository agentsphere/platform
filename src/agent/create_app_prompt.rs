/// Build the system prompt for create-app sessions.
///
/// This prompt instructs the LLM to operate in two phases: clarify requirements,
/// then execute tools via structured output. The LLM responds with a JSON schema
/// containing `text` (response to user) and `tools` (actions to execute).
pub fn build_create_app_system_prompt() -> &'static str {
    CREATE_APP_SYSTEM_PROMPT
}

const CREATE_APP_SYSTEM_PROMPT: &str = r#"You are the Manager Agent for Platform. You help users create projects and spawn coding agents.

You respond using structured output with two fields:
- "text": Your message to the user
- "tools": Array of tools to execute (empty array [] ONLY when no action is needed)

IMPORTANT: When the user's request is clear enough to act on, IMMEDIATELY call a tool. Do NOT return tools: [] unless you genuinely need to ask the user a question first.

Available tools:

1. create_project — Parameters: { "name": "slug-style-name" }
2. spawn_coding_agent — Parameters: { "project_id": "<uuid>", "prompt": "detailed instructions" }
3. check_session_progress — Parameters: { "session_id": "<uuid>" }
4. send_message_to_session — Parameters: { "session_id": "<uuid>", "message": "instruction" }

Rules:
- ONE tool per response. You will receive tool results, then call the next tool.
- You need create_project's result (project_id) before calling spawn_coding_agent.
- Do NOT call check_session_progress or send_message_to_session during initial setup.
- Use exact parameter names: "name" (not "project_name"), "session_id" (not "session_name").

Workflow:
1. If the user's request is vague, ask 1-2 clarifying questions (tools: []).
2. Otherwise, call create_project immediately with a slug-style name.
3. After receiving create_project results, call spawn_coding_agent with the project_id and a detailed prompt.
4. After spawn_coding_agent succeeds, tell the user the agent is working and return tools: [].

The prompt for spawn_coding_agent MUST instruct the worker to:
- Create the application source code with a GET /healthz endpoint returning 200 on port 8080
- Create a multi-stage Dockerfile that builds and runs the app, EXPOSEing port 8080
- Create `.platform.yaml` at repo root with a kaniko build step:
  ```yaml
  pipeline:
    steps:
      - name: build
        image: gcr.io/kaniko-project/executor:debug
        commands:
          - /kaniko/executor --context=dir:///workspace --dockerfile=/workspace/Dockerfile --destination=$REGISTRY/$PROJECT:$COMMIT_SHA --insecure --cache=true
  ```
- Create `deploy/production.yaml` with K8s Deployment + Service using template vars: {{ project_name }}, {{ image_ref }}, {{ values.replicas | default(1) }}
- Add OpenTelemetry SDK instrumentation (reads OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME env vars)
- After creating all files: `git add -A && git commit -m "Initial app scaffold" && git push origin main`

Keep all responses concise."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_has_workflow_steps() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains("create_project"));
        assert!(prompt.contains("spawn_coding_agent"));
        assert!(prompt.contains("Workflow"));
    }

    #[test]
    fn system_prompt_mentions_tools() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains("create_project"));
        assert!(prompt.contains("spawn_coding_agent"));
        assert!(prompt.contains("send_message_to_session"));
        assert!(prompt.contains("check_session_progress"));
    }

    #[test]
    fn system_prompt_has_tool_parameter_docs() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains(r#""name": "slug-style-name""#));
        assert!(prompt.contains(r#""project_id": "<uuid>""#));
        assert!(prompt.contains(r#""session_id": "<uuid>""#));
    }

    #[test]
    fn system_prompt_mentions_structured_output() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains("structured output"));
        assert!(prompt.contains("\"text\""));
        assert!(prompt.contains("\"tools\""));
    }

    #[test]
    fn system_prompt_mentions_lifecycle_flow() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains("Dockerfile"));
        assert!(prompt.contains(".platform.yaml"));
        assert!(prompt.contains("healthz"));
        assert!(prompt.contains("OTEL_EXPORTER_OTLP_ENDPOINT"));
        assert!(prompt.contains("deploy/production.yaml"));
    }

    #[test]
    fn system_prompt_enforces_sequential_tools() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains("ONE tool per response"));
    }

    #[test]
    fn system_prompt_warns_about_param_names() {
        let prompt = build_create_app_system_prompt();
        assert!(prompt.contains("not \"project_name\""));
        assert!(prompt.contains("not \"session_name\""));
    }
}
