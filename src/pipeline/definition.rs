use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::error::PipelineError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Top-level `.platform.yaml` structure.
#[derive(Debug, Deserialize)]
pub struct PipelineFile {
    pub pipeline: PipelineDefinition,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // artifacts consumed via serde for future use
pub struct PipelineDefinition {
    pub steps: Vec<StepDef>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactDef>,
    #[serde(rename = "on")]
    pub trigger: Option<TriggerConfig>,
    #[serde(default)]
    pub dev_image: Option<DevImageConfig>,
}

/// Configuration for building a custom dev/agent image from the project repo.
#[derive(Debug, Deserialize)]
pub struct DevImageConfig {
    pub dockerfile: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields consumed via serde + executor
pub struct StepDef {
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Per-step condition: controls when this step runs based on trigger type and branch.
    #[serde(default)]
    pub only: Option<StepCondition>,
    /// Deploy-test step: deploy app to temp namespace and run test image.
    /// When present, `image` and `commands` are ignored.
    #[serde(default)]
    pub deploy_test: Option<DeployTestDef>,
    /// Quality gate: marks this step as a quality gate (UI/semantic only).
    #[serde(default)]
    pub gate: bool,
}

/// Per-step condition controlling when a step runs.
/// Both fields AND together. Absent `only` = always run.
/// Empty list = match all.
#[derive(Debug, Deserialize, Default)]
pub struct StepCondition {
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub branches: Vec<String>,
}

fn default_readiness_path() -> String {
    "/healthz".into()
}

fn default_readiness_timeout() -> u32 {
    120
}

/// Configuration for a deploy-test step.
#[derive(Debug, Deserialize, Serialize)]
pub struct DeployTestDef {
    /// Test image to run (supports `$REGISTRY/$PROJECT/$COMMIT_SHA` expansion).
    pub test_image: String,
    /// Commands to run in the test container (if empty, uses image entrypoint).
    #[serde(default)]
    pub commands: Vec<String>,
    /// Path to deploy manifests (default: `deploy/production.yaml`).
    #[serde(default)]
    pub manifests: Option<String>,
    /// Readiness path to poll before running tests (default: `/healthz`).
    #[serde(default = "default_readiness_path")]
    pub readiness_path: String,
    /// Timeout in seconds for app to become ready (default: 120).
    #[serde(default = "default_readiness_timeout")]
    pub readiness_timeout: u32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields consumed via serde + executor
pub struct ArtifactDef {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub expires: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TriggerConfig {
    pub push: Option<PushTrigger>,
    pub mr: Option<MrTrigger>,
    pub tag: Option<TagTrigger>,
}

#[derive(Debug, Deserialize)]
pub struct PushTrigger {
    pub branches: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct MrTrigger {
    pub actions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TagTrigger {
    pub patterns: Vec<String>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a `.platform.yaml` file contents into a validated `PipelineDefinition`.
pub fn parse(yaml: &str) -> Result<PipelineDefinition, PipelineError> {
    let file: PipelineFile =
        serde_yaml::from_str(yaml).map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;

    validate(&file.pipeline)?;
    Ok(file.pipeline)
}

fn validate(def: &PipelineDefinition) -> Result<(), PipelineError> {
    if def.steps.is_empty() {
        return Err(PipelineError::InvalidDefinition(
            "pipeline must have at least one step".into(),
        ));
    }

    for (i, step) in def.steps.iter().enumerate() {
        if step.name.is_empty() {
            return Err(PipelineError::InvalidDefinition(format!(
                "step {i} is missing a name"
            )));
        }
        // deploy_test steps don't need image/commands
        if let Some(ref dt) = step.deploy_test {
            if !step.commands.is_empty() {
                return Err(PipelineError::InvalidDefinition(format!(
                    "step '{}': deploy_test and commands are mutually exclusive",
                    step.name,
                )));
            }
            validate_deploy_test(&step.name, dt)?;
        } else if step.image.is_empty() {
            return Err(PipelineError::InvalidDefinition(format!(
                "step '{}' is missing an image",
                step.name
            )));
        }
        if let Some(ref cond) = step.only {
            validate_step_condition(&step.name, cond)?;
        }
    }

    validate_dag(&def.steps)?;

    if let Some(dev) = &def.dev_image {
        if dev.dockerfile.is_empty() {
            return Err(PipelineError::InvalidDefinition(
                "dev_image.dockerfile must not be empty".into(),
            ));
        }
        if dev.dockerfile.len() > 255 {
            return Err(PipelineError::InvalidDefinition(
                "dev_image.dockerfile must be 255 characters or fewer".into(),
            ));
        }
        if dev.dockerfile.contains("..") {
            return Err(PipelineError::InvalidDefinition(
                "dev_image.dockerfile must not contain path traversal (..)".into(),
            ));
        }
        if dev.dockerfile.starts_with('/') {
            return Err(PipelineError::InvalidDefinition(
                "dev_image.dockerfile must be a relative path".into(),
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// DAG validation
// ---------------------------------------------------------------------------

/// Validate that `depends_on` references are valid and the graph is acyclic.
fn validate_dag(steps: &[StepDef]) -> Result<(), PipelineError> {
    let names: HashSet<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    for step in steps {
        for dep in &step.depends_on {
            if !names.contains(dep.as_str()) {
                return Err(PipelineError::InvalidDefinition(format!(
                    "step '{}': depends_on references unknown step '{dep}'",
                    step.name,
                )));
            }
            if dep == &step.name {
                return Err(PipelineError::InvalidDefinition(format!(
                    "step '{}': depends_on cannot reference itself",
                    step.name,
                )));
            }
        }
    }

    // Cycle detection via Kahn's algorithm
    if topological_layers(steps).is_none() {
        return Err(PipelineError::InvalidDefinition(
            "pipeline dependency graph contains a cycle".into(),
        ));
    }

    Ok(())
}

/// Compute parallel execution layers via topological sort.
///
/// Returns `None` if the graph has a cycle. Otherwise returns groups of step
/// indices that can run in parallel: layer 0 has no deps, layer N depends only
/// on layers < N.
pub fn topological_layers(steps: &[StepDef]) -> Option<Vec<Vec<usize>>> {
    let name_to_idx: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();

    let n = steps.len();
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, step) in steps.iter().enumerate() {
        for dep_name in &step.depends_on {
            if let Some(&dep_idx) = name_to_idx.get(dep_name.as_str()) {
                in_degree[i] += 1;
                dependents[dep_idx].push(i);
            }
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(i, _)| i)
        .collect();

    let mut layers = Vec::new();
    let mut processed = 0usize;

    while !queue.is_empty() {
        let layer: Vec<usize> = queue.drain(..).collect();
        for &idx in &layer {
            processed += 1;
            for &dep_idx in &dependents[idx] {
                in_degree[dep_idx] -= 1;
                if in_degree[dep_idx] == 0 {
                    queue.push_back(dep_idx);
                }
            }
        }
        layers.push(layer);
    }

    if processed == n {
        Some(layers)
    } else {
        None // cycle detected
    }
}

// ---------------------------------------------------------------------------
// Trigger matching
// ---------------------------------------------------------------------------

/// Check if a push to `branch` matches the trigger configuration.
///
/// If no trigger config or no push trigger is defined, all branches match.
pub fn matches_push(trigger: Option<&TriggerConfig>, branch: &str) -> bool {
    let Some(config) = trigger else {
        return true;
    };
    let Some(push) = &config.push else {
        return true;
    };
    if push.branches.is_empty() {
        return true;
    }
    push.branches
        .iter()
        .any(|pattern| crate::validation::match_glob_pattern(pattern, branch))
}

/// Check if an MR action matches the trigger configuration.
///
/// If no trigger config or no MR trigger is defined, all actions match.
pub fn matches_mr(trigger: Option<&TriggerConfig>, action: &str) -> bool {
    let Some(config) = trigger else {
        return true;
    };
    let Some(mr) = &config.mr else {
        return true;
    };
    if mr.actions.is_empty() {
        return true;
    }
    mr.actions.iter().any(|a| a == action)
}

/// Check if a tag name matches the trigger configuration.
///
/// If no trigger config or no tag trigger is defined, returns false
/// (tags don't trigger by default, unlike pushes/MRs).
pub fn matches_tag(trigger: Option<&TriggerConfig>, tag_name: &str) -> bool {
    let Some(config) = trigger else {
        return false;
    };
    let Some(tag) = &config.tag else {
        return false;
    };
    if tag.patterns.is_empty() {
        return true;
    }
    tag.patterns
        .iter()
        .any(|pattern| crate::validation::match_glob_pattern(pattern, tag_name))
}

// ---------------------------------------------------------------------------
// Per-step condition matching
// ---------------------------------------------------------------------------

const VALID_EVENTS: &[&str] = &["push", "mr", "tag", "api"];

/// Check if a step should run given the trigger type and branch.
///
/// - `None` condition → always run (backward compat)
/// - Empty `events` list → match all events
/// - Empty `branches` list → match all branches
/// - Both fields AND together
pub fn step_matches(condition: Option<&StepCondition>, trigger_type: &str, branch: &str) -> bool {
    let Some(cond) = condition else {
        return true;
    };

    let events_match = cond.events.is_empty() || cond.events.iter().any(|e| e == trigger_type);
    let branches_match = cond.branches.is_empty()
        || cond
            .branches
            .iter()
            .any(|pattern| crate::validation::match_glob_pattern(pattern, branch));

    events_match && branches_match
}

/// Validate per-step condition fields.
fn validate_step_condition(step_name: &str, cond: &StepCondition) -> Result<(), PipelineError> {
    for event in &cond.events {
        if !VALID_EVENTS.contains(&event.as_str()) {
            return Err(PipelineError::InvalidDefinition(format!(
                "step '{step_name}': invalid event '{event}' (allowed: push, mr, tag, api)"
            )));
        }
    }
    for branch in &cond.branches {
        if branch.is_empty() || branch.len() > 255 {
            return Err(PipelineError::InvalidDefinition(format!(
                "step '{step_name}': branch pattern must be 1-255 characters"
            )));
        }
    }
    Ok(())
}

/// Validate deploy-test step configuration.
fn validate_deploy_test(step_name: &str, dt: &DeployTestDef) -> Result<(), PipelineError> {
    if dt.test_image.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "step '{step_name}': deploy_test.test_image must not be empty"
        )));
    }
    if let Some(ref manifests) = dt.manifests
        && manifests.contains("..")
    {
        return Err(PipelineError::InvalidDefinition(format!(
            "step '{step_name}': deploy_test.manifests must not contain path traversal (..)"
        )));
    }
    if !dt.readiness_path.starts_with('/') {
        return Err(PipelineError::InvalidDefinition(format!(
            "step '{step_name}': deploy_test.readiness_path must start with '/'"
        )));
    }
    if dt.readiness_timeout == 0 || dt.readiness_timeout > 600 {
        return Err(PipelineError::InvalidDefinition(format!(
            "step '{step_name}': deploy_test.readiness_timeout must be 1-600"
        )));
    }
    Ok(())
}

/// Expand `$VAR` references in a string using the given env var slice.
pub fn expand_step_env(value: &str, env_vars: &[(String, String)]) -> String {
    let mut result = value.to_owned();
    for (key, val) in env_vars {
        result = result.replace(&format!("${key}"), val);
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_YAML: &str = r"
pipeline:
  steps:
    - name: test
      image: rust:1.85-slim
      commands:
        - cargo nextest run
    - name: build-image
      image: gcr.io/kaniko-project/executor:latest
      environment:
        DOCKER_CONFIG: /kaniko/.docker
      commands:
        - /kaniko/executor --context=. --dockerfile=Dockerfile

  artifacts:
    - name: test-results
      path: target/nextest/
      expires: 7d

  on:
    push:
      branches: [main, develop]
    mr:
      actions: [opened, synchronized]
";

    #[test]
    fn parse_valid_yaml() {
        let def = parse(VALID_YAML).unwrap();
        assert_eq!(def.steps.len(), 2);
        assert_eq!(def.steps[0].name, "test");
        assert_eq!(def.steps[0].image, "rust:1.85-slim");
        assert_eq!(def.steps[0].commands.len(), 1);
        assert_eq!(def.steps[1].name, "build-image");
        assert!(!def.steps[1].environment.is_empty());
        assert_eq!(def.artifacts.len(), 1);
        assert_eq!(def.artifacts[0].name, "test-results");
        assert!(def.trigger.is_some());
    }

    #[test]
    fn parse_minimal_yaml() {
        let yaml = r"
pipeline:
  steps:
    - name: hello
      image: alpine
      commands:
        - echo hello
";
        let def = parse(yaml).unwrap();
        assert_eq!(def.steps.len(), 1);
        assert!(def.artifacts.is_empty());
        assert!(def.trigger.is_none());
    }

    #[test]
    fn parse_missing_steps() {
        let yaml = r"
pipeline:
  steps: []
";
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("at least one step")),
            "empty steps should produce InvalidDefinition, got: {err:?}"
        );
    }

    #[test]
    fn parse_missing_image() {
        let yaml = r#"
pipeline:
  steps:
    - name: test
      image: ""
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("missing an image")),
            "empty image should produce InvalidDefinition, got: {err:?}"
        );
    }

    #[test]
    fn parse_missing_name() {
        let yaml = r#"
pipeline:
  steps:
    - name: ""
      image: alpine
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("missing a name")),
            "empty name should produce InvalidDefinition, got: {err:?}"
        );
    }

    #[test]
    fn parse_invalid_yaml() {
        let err = parse("not valid yaml: [").unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(_)),
            "invalid YAML should produce InvalidDefinition, got: {err:?}"
        );
    }

    #[test]
    fn matches_push_with_branches() {
        let def = parse(VALID_YAML).unwrap();
        assert!(matches_push(def.trigger.as_ref(), "main"));
        assert!(matches_push(def.trigger.as_ref(), "develop"));
        assert!(!matches_push(def.trigger.as_ref(), "feature/foo"));
    }

    #[test]
    fn matches_push_no_trigger() {
        assert!(matches_push(None, "any-branch"));
    }

    #[test]
    fn matches_push_wildcard() {
        let yaml = r#"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: ["feature/*"]
"#;
        let def = parse(yaml).unwrap();
        assert!(matches_push(def.trigger.as_ref(), "feature/foo"));
        assert!(matches_push(def.trigger.as_ref(), "feature/bar"));
        assert!(!matches_push(def.trigger.as_ref(), "main"));
    }

    #[test]
    fn matches_push_star_only() {
        let yaml = r#"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: ["*"]
"#;
        let def = parse(yaml).unwrap();
        assert!(matches_push(def.trigger.as_ref(), "main"));
        assert!(matches_push(def.trigger.as_ref(), "anything"));
    }

    #[test]
    fn matches_mr_actions() {
        let def = parse(VALID_YAML).unwrap();
        assert!(matches_mr(def.trigger.as_ref(), "opened"));
        assert!(matches_mr(def.trigger.as_ref(), "synchronized"));
        assert!(!matches_mr(def.trigger.as_ref(), "closed"));
    }

    #[test]
    fn matches_mr_no_trigger() {
        assert!(matches_mr(None, "any-action"));
    }

    // -- Pattern matching edge cases --

    #[test]
    fn match_pattern_exact_no_wildcard() {
        use crate::validation::match_glob_pattern;
        assert!(match_glob_pattern("main", "main"));
        assert!(!match_glob_pattern("main", "develop"));
    }

    #[test]
    fn match_pattern_prefix_wildcard() {
        use crate::validation::match_glob_pattern;
        // "*-release" should match "v1-release"
        assert!(match_glob_pattern("*-release", "v1-release"));
        assert!(match_glob_pattern("*-release", "hotfix-release"));
        assert!(!match_glob_pattern("*-release", "release-v1"));
    }

    #[test]
    fn match_pattern_suffix_wildcard() {
        use crate::validation::match_glob_pattern;
        assert!(match_glob_pattern("release/*", "release/v1"));
        assert!(match_glob_pattern("release/*", "release/"));
        assert!(!match_glob_pattern("release/*", "hotfix/v1"));
    }

    #[test]
    fn match_pattern_multi_wildcard_falls_to_exact() {
        use crate::validation::match_glob_pattern;
        // Complex patterns with 2+ wildcards fall to exact match
        assert!(
            !match_glob_pattern("a/*/b/*", "a/x/b/y"),
            "multi-wildcard falls to exact match"
        );
        assert!(
            match_glob_pattern("a/*/b/*", "a/*/b/*"),
            "multi-wildcard matches itself exactly"
        );
    }

    #[test]
    fn matches_tag_patterns() {
        let yaml = r#"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    tag:
      patterns: ["v*"]
"#;
        let def = parse(yaml).unwrap();
        assert!(matches_tag(def.trigger.as_ref(), "v1.0.0"));
        assert!(matches_tag(def.trigger.as_ref(), "v2.0"));
        assert!(!matches_tag(def.trigger.as_ref(), "release-1.0"));
    }

    #[test]
    fn matches_tag_no_trigger_returns_false() {
        assert!(!matches_tag(None, "v1.0.0"));
    }

    #[test]
    fn matches_tag_no_tag_trigger_returns_false() {
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: [main]
";
        let def = parse(yaml).unwrap();
        assert!(!matches_tag(def.trigger.as_ref(), "v1.0.0"));
    }

    #[test]
    fn matches_push_empty_branches_matches_all() {
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: []
";
        let def = parse(yaml).unwrap();
        assert!(matches_push(def.trigger.as_ref(), "any-branch"));
    }

    #[test]
    fn matches_mr_empty_actions_matches_all() {
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    mr:
      actions: []
";
        let def = parse(yaml).unwrap();
        assert!(matches_mr(def.trigger.as_ref(), "any-action"));
    }

    // -- Deep field verification --

    #[test]
    fn parsed_step_environment_values() {
        let def = parse(VALID_YAML).unwrap();
        assert_eq!(
            def.steps[1].environment.get("DOCKER_CONFIG"),
            Some(&"/kaniko/.docker".to_owned()),
        );
    }

    #[test]
    fn parsed_step_commands_content() {
        let def = parse(VALID_YAML).unwrap();
        assert_eq!(def.steps[0].commands[0], "cargo nextest run");
    }

    #[test]
    fn parsed_artifact_fields() {
        let def = parse(VALID_YAML).unwrap();
        assert_eq!(def.artifacts[0].path, "target/nextest/");
        assert_eq!(def.artifacts[0].expires.as_deref(), Some("7d"));
    }

    #[test]
    fn parsed_trigger_push_branches() {
        let def = parse(VALID_YAML).unwrap();
        let push = def.trigger.as_ref().unwrap().push.as_ref().unwrap();
        assert_eq!(push.branches, vec!["main", "develop"]);
    }

    #[test]
    fn parsed_trigger_mr_actions() {
        let def = parse(VALID_YAML).unwrap();
        let mr = def.trigger.as_ref().unwrap().mr.as_ref().unwrap();
        assert_eq!(mr.actions, vec!["opened", "synchronized"]);
    }

    // -- Additional trigger matching tests --

    #[test]
    fn matches_push_multiple_branches_any_match() {
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: [main, develop, staging]
";
        let def = parse(yaml).unwrap();
        assert!(matches_push(def.trigger.as_ref(), "main"));
        assert!(matches_push(def.trigger.as_ref(), "develop"));
        assert!(matches_push(def.trigger.as_ref(), "staging"));
        assert!(!matches_push(def.trigger.as_ref(), "feature/foo"));
    }

    #[test]
    fn matches_push_suffix_wildcard() {
        let yaml = r#"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: ["*-release"]
"#;
        let def = parse(yaml).unwrap();
        assert!(matches_push(def.trigger.as_ref(), "v2-release"));
        assert!(matches_push(def.trigger.as_ref(), "hotfix-release"));
        assert!(!matches_push(def.trigger.as_ref(), "release-v1"));
    }

    #[test]
    fn matches_push_no_push_trigger_matches_all() {
        // Only MR trigger configured, no push trigger
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    mr:
      actions: [opened]
";
        let def = parse(yaml).unwrap();
        // No push trigger means all branches match
        assert!(matches_push(def.trigger.as_ref(), "any-branch"));
    }

    #[test]
    fn matches_mr_no_mr_trigger_matches_all() {
        // Only push trigger configured, no MR trigger
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
  on:
    push:
      branches: [main]
";
        let def = parse(yaml).unwrap();
        // No MR trigger means all actions match
        assert!(matches_mr(def.trigger.as_ref(), "any-action"));
    }

    #[test]
    fn complex_definition_parsing() {
        let yaml = r#"
pipeline:
  steps:
    - name: lint
      image: rust:1.85
      commands:
        - cargo clippy --all-features
    - name: test
      image: rust:1.85
      commands:
        - cargo nextest run
      depends_on:
        - lint
    - name: build
      image: rust:1.85
      commands:
        - cargo build --release
      depends_on:
        - test
      environment:
        CARGO_INCREMENTAL: "0"
        RUSTFLAGS: "-C link-arg=-s"
  artifacts:
    - name: binary
      path: target/release/platform
      expires: 30d
    - name: test-results
      path: target/nextest/
  on:
    push:
      branches: [main, "release/*"]
    mr:
      actions: [opened, synchronized, reopened]
"#;
        let def = parse(yaml).unwrap();
        assert_eq!(def.steps.len(), 3);
        assert_eq!(def.steps[0].name, "lint");
        assert_eq!(def.steps[1].depends_on, vec!["lint"]);
        assert_eq!(def.steps[2].depends_on, vec!["test"]);
        assert_eq!(def.steps[2].environment.len(), 2);
        assert_eq!(def.artifacts.len(), 2);
        assert_eq!(def.artifacts[0].expires.as_deref(), Some("30d"));
        assert!(def.artifacts[1].expires.is_none());

        // Trigger matching
        assert!(matches_push(def.trigger.as_ref(), "main"));
        assert!(matches_push(def.trigger.as_ref(), "release/v1.0"));
        assert!(!matches_push(def.trigger.as_ref(), "feature/foo"));
        assert!(matches_mr(def.trigger.as_ref(), "opened"));
        assert!(matches_mr(def.trigger.as_ref(), "reopened"));
        assert!(!matches_mr(def.trigger.as_ref(), "closed"));
    }

    // -- dev_image parsing --

    #[test]
    fn parse_dev_image_config() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: gcr.io/kaniko-project/executor:debug
      commands:
        - /kaniko/executor --context=. --dockerfile=Dockerfile
  dev_image:
    dockerfile: Dockerfile.dev
";
        let def = parse(yaml).unwrap();
        let dev = def.dev_image.as_ref().unwrap();
        assert_eq!(dev.dockerfile, "Dockerfile.dev");
    }

    #[test]
    fn parse_dev_image_custom_path() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
  dev_image:
    dockerfile: docker/Dockerfile.agent
";
        let def = parse(yaml).unwrap();
        let dev = def.dev_image.as_ref().unwrap();
        assert_eq!(dev.dockerfile, "docker/Dockerfile.agent");
    }

    #[test]
    fn parse_dev_image_optional() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
";
        let def = parse(yaml).unwrap();
        assert!(def.dev_image.is_none());
    }

    #[test]
    fn validate_dev_image_empty_dockerfile() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
  dev_image:
    dockerfile: ""
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("must not be empty")),
            "empty dockerfile should fail: {err:?}"
        );
    }

    #[test]
    fn validate_dev_image_path_traversal() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
  dev_image:
    dockerfile: "../Dockerfile.dev"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("path traversal")),
            "path traversal should fail: {err:?}"
        );
    }

    #[test]
    fn validate_dev_image_absolute_path() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
  dev_image:
    dockerfile: /etc/Dockerfile
";
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("relative path")),
            "absolute path should fail: {err:?}"
        );
    }

    // -- StepCondition parsing --

    #[test]
    fn parse_only_with_events() {
        let yaml = r#"
pipeline:
  steps:
    - name: lint
      image: rust:1.85
      commands: [cargo clippy]
      only:
        events: [mr]
"#;
        let def = parse(yaml).unwrap();
        let cond = def.steps[0].only.as_ref().unwrap();
        assert_eq!(cond.events, vec!["mr"]);
        assert!(cond.branches.is_empty());
    }

    #[test]
    fn parse_only_with_branches() {
        let yaml = r#"
pipeline:
  steps:
    - name: deploy
      image: alpine
      only:
        branches: ["main"]
"#;
        let def = parse(yaml).unwrap();
        let cond = def.steps[0].only.as_ref().unwrap();
        assert!(cond.events.is_empty());
        assert_eq!(cond.branches, vec!["main"]);
    }

    #[test]
    fn parse_only_with_both() {
        let yaml = r#"
pipeline:
  steps:
    - name: deploy
      image: alpine
      only:
        events: [push, tag]
        branches: ["main", "release/*"]
"#;
        let def = parse(yaml).unwrap();
        let cond = def.steps[0].only.as_ref().unwrap();
        assert_eq!(cond.events, vec!["push", "tag"]);
        assert_eq!(cond.branches, vec!["main", "release/*"]);
    }

    #[test]
    fn parse_only_absent_is_none() {
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
";
        let def = parse(yaml).unwrap();
        assert!(def.steps[0].only.is_none());
    }

    #[test]
    fn parse_only_invalid_event() {
        let yaml = r#"
pipeline:
  steps:
    - name: test
      image: alpine
      only:
        events: [invalid]
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("invalid event")),
            "got: {err:?}"
        );
    }

    // -- step_matches --

    #[test]
    fn step_matches_none_always_runs() {
        assert!(step_matches(None, "push", "main"));
        assert!(step_matches(None, "mr", "feature/foo"));
    }

    #[test]
    fn step_matches_empty_events_matches_all() {
        let cond = StepCondition {
            events: vec![],
            branches: vec![],
        };
        assert!(step_matches(Some(&cond), "push", "main"));
        assert!(step_matches(Some(&cond), "mr", "any"));
    }

    #[test]
    fn step_matches_events_filter() {
        let cond = StepCondition {
            events: vec!["mr".into()],
            branches: vec![],
        };
        assert!(step_matches(Some(&cond), "mr", "main"));
        assert!(!step_matches(Some(&cond), "push", "main"));
    }

    #[test]
    fn step_matches_branches_filter() {
        let cond = StepCondition {
            events: vec![],
            branches: vec!["main".into()],
        };
        assert!(step_matches(Some(&cond), "push", "main"));
        assert!(!step_matches(Some(&cond), "push", "feature/foo"));
    }

    #[test]
    fn step_matches_events_and_branches_both_must_match() {
        let cond = StepCondition {
            events: vec!["push".into()],
            branches: vec!["main".into()],
        };
        // Both match
        assert!(step_matches(Some(&cond), "push", "main"));
        // Event matches but branch doesn't
        assert!(!step_matches(Some(&cond), "push", "feature/foo"));
        // Branch matches but event doesn't
        assert!(!step_matches(Some(&cond), "mr", "main"));
    }

    #[test]
    fn step_matches_branch_glob() {
        let cond = StepCondition {
            events: vec![],
            branches: vec!["feature/*".into()],
        };
        assert!(step_matches(Some(&cond), "push", "feature/foo"));
        assert!(!step_matches(Some(&cond), "push", "main"));
    }

    // -- DeployTestDef parsing --

    #[test]
    fn parse_deploy_test_step() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
        readiness_path: /healthz
        readiness_timeout: 60
"#;
        let def = parse(yaml).unwrap();
        let dt = def.steps[0].deploy_test.as_ref().unwrap();
        assert_eq!(dt.test_image, "registry/test:v1");
        assert_eq!(dt.readiness_path, "/healthz");
        assert_eq!(dt.readiness_timeout, 60);
        assert!(dt.commands.is_empty());
        assert!(dt.manifests.is_none());
    }

    #[test]
    fn parse_deploy_test_with_commands() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
        commands: [npm test, npm run e2e]
"#;
        let def = parse(yaml).unwrap();
        let dt = def.steps[0].deploy_test.as_ref().unwrap();
        assert_eq!(dt.commands, vec!["npm test", "npm run e2e"]);
    }

    #[test]
    fn parse_deploy_test_defaults() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
"#;
        let def = parse(yaml).unwrap();
        let dt = def.steps[0].deploy_test.as_ref().unwrap();
        assert_eq!(dt.readiness_path, "/healthz");
        assert_eq!(dt.readiness_timeout, 120);
    }

    #[test]
    fn validate_deploy_test_and_commands_mutual_exclusion() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      commands: [echo hello]
      deploy_test:
        test_image: registry/test:v1
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("mutually exclusive")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_deploy_test_empty_test_image() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: ""
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("test_image must not be empty")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_deploy_test_bad_readiness_path() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
        readiness_path: healthz
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("readiness_path must start with")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_deploy_test_bad_timeout() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
        readiness_timeout: 0
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("readiness_timeout must be 1-600")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_deploy_test_manifests_path_traversal() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
        manifests: "../etc/passwd"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("path traversal")),
            "got: {err:?}"
        );
    }

    // -- expand_step_env --

    #[test]
    fn expand_step_env_replaces_vars() {
        let env = vec![
            ("REGISTRY".into(), "localhost:5000".into()),
            ("PROJECT".into(), "myapp".into()),
            ("COMMIT_SHA".into(), "abc123".into()),
        ];
        let result = expand_step_env("$REGISTRY/$PROJECT/test:$COMMIT_SHA", &env);
        assert_eq!(result, "localhost:5000/myapp/test:abc123");
    }

    #[test]
    fn expand_step_env_no_vars() {
        let result = expand_step_env("plain-string", &[]);
        assert_eq!(result, "plain-string");
    }

    #[test]
    fn expand_step_env_unknown_var_left_as_is() {
        let env = vec![("REGISTRY".into(), "localhost:5000".into())];
        let result = expand_step_env("$REGISTRY/$UNKNOWN", &env);
        assert_eq!(result, "localhost:5000/$UNKNOWN");
    }

    // -- gate field parsing --

    #[test]
    fn parse_gate_default_false() {
        let yaml = r"
pipeline:
  steps:
    - name: test
      image: alpine
";
        let def = parse(yaml).unwrap();
        assert!(!def.steps[0].gate);
    }

    #[test]
    fn parse_gate_true() {
        let yaml = r"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
      gate: true
";
        let def = parse(yaml).unwrap();
        assert!(def.steps[0].gate);
    }

    // -- DAG validation --

    #[test]
    fn validate_dag_unknown_dep_rejected() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
      depends_on: [nonexistent]
";
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("unknown step")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_dag_self_dep_rejected() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
      depends_on: [build]
";
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("cannot reference itself")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_dag_cycle_detected() {
        let yaml = r"
pipeline:
  steps:
    - name: a
      image: alpine
      depends_on: [b]
    - name: b
      image: alpine
      depends_on: [a]
";
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("cycle")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_dag_valid_chain() {
        let yaml = r"
pipeline:
  steps:
    - name: lint
      image: alpine
    - name: test
      image: alpine
      depends_on: [lint]
    - name: build
      image: alpine
      depends_on: [test]
";
        parse(yaml).unwrap(); // should not error
    }

    // -- topological_layers --

    #[test]
    fn topological_layers_no_deps() {
        let yaml = r"
pipeline:
  steps:
    - name: a
      image: alpine
    - name: b
      image: alpine
    - name: c
      image: alpine
";
        let def = parse(yaml).unwrap();
        let layers = topological_layers(&def.steps).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 3);
    }

    #[test]
    fn topological_layers_diamond() {
        // A → B, A → C, B → D, C → D
        let yaml = r"
pipeline:
  steps:
    - name: a
      image: alpine
    - name: b
      image: alpine
      depends_on: [a]
    - name: c
      image: alpine
      depends_on: [a]
    - name: d
      image: alpine
      depends_on: [b, c]
";
        let def = parse(yaml).unwrap();
        let layers = topological_layers(&def.steps).unwrap();
        assert_eq!(layers.len(), 3);
        // Layer 0: a
        assert_eq!(layers[0], vec![0]);
        // Layer 1: b, c (parallel)
        let mut l1 = layers[1].clone();
        l1.sort();
        assert_eq!(l1, vec![1, 2]);
        // Layer 2: d
        assert_eq!(layers[2], vec![3]);
    }

    #[test]
    fn topological_layers_single_step() {
        let yaml = r"
pipeline:
  steps:
    - name: only
      image: alpine
";
        let def = parse(yaml).unwrap();
        let layers = topological_layers(&def.steps).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0], vec![0]);
    }

    #[test]
    fn topological_layers_linear_chain() {
        let yaml = r"
pipeline:
  steps:
    - name: a
      image: alpine
    - name: b
      image: alpine
      depends_on: [a]
    - name: c
      image: alpine
      depends_on: [b]
";
        let def = parse(yaml).unwrap();
        let layers = topological_layers(&def.steps).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![0]);
        assert_eq!(layers[1], vec![1]);
        assert_eq!(layers[2], vec![2]);
    }

    #[test]
    fn topological_layers_cycle_returns_none() {
        let steps = vec![
            StepDef {
                name: "a".into(),
                image: "alpine".into(),
                depends_on: vec!["b".into()],
                commands: vec![],
                environment: HashMap::new(),
                only: None,
                deploy_test: None,
                gate: false,
            },
            StepDef {
                name: "b".into(),
                image: "alpine".into(),
                depends_on: vec!["a".into()],
                commands: vec![],
                environment: HashMap::new(),
                only: None,
                deploy_test: None,
                gate: false,
            },
        ];
        assert!(topological_layers(&steps).is_none());
    }
}
