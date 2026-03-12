use std::collections::HashMap;

use serde::Deserialize;

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
    pub image: String,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
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
        if step.image.is_empty() {
            return Err(PipelineError::InvalidDefinition(format!(
                "step '{}' is missing an image",
                step.name
            )));
        }
    }

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
}
