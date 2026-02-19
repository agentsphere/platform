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
#[allow(dead_code)] // fields consumed via serde + executor
pub struct PipelineDefinition {
    pub steps: Vec<StepDef>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactDef>,
    #[serde(rename = "on")]
    pub trigger: Option<TriggerConfig>,
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
#[allow(dead_code)] // mr field consumed via serde + matches_mr
pub struct TriggerConfig {
    pub push: Option<PushTrigger>,
    pub mr: Option<MrTrigger>,
}

#[derive(Debug, Deserialize)]
pub struct PushTrigger {
    pub branches: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // consumed via serde + matches_mr
pub struct MrTrigger {
    pub actions: Vec<String>,
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
        .any(|pattern| match_pattern(pattern, branch))
}

/// Check if an MR action matches the trigger configuration.
///
/// If no trigger config or no MR trigger is defined, all actions match.
#[allow(dead_code)] // used by trigger::on_mr, wired in MR integration
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

/// Simple glob-like pattern matching for branch names.
///
/// Supports `*` as a wildcard matching any sequence of characters.
fn match_pattern(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if !pattern.contains('*') {
        return pattern == value;
    }

    let parts: Vec<&str> = pattern.split('*').collect();

    // Pattern like "feature/*"
    if parts.len() == 2 {
        let prefix = parts[0];
        let suffix = parts[1];
        return value.starts_with(prefix)
            && value.ends_with(suffix)
            && value.len() >= prefix.len() + suffix.len();
    }

    // Fallback: exact match for complex patterns
    pattern == value
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_YAML: &str = r#"
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
"#;

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
        let yaml = r#"
pipeline:
  steps:
    - name: hello
      image: alpine
      commands:
        - echo hello
"#;
        let def = parse(yaml).unwrap();
        assert_eq!(def.steps.len(), 1);
        assert!(def.artifacts.is_empty());
        assert!(def.trigger.is_none());
    }

    #[test]
    fn parse_missing_steps() {
        let yaml = r#"
pipeline:
  steps: []
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.to_string().contains("at least one step"));
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
        assert!(err.to_string().contains("missing an image"));
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
        assert!(err.to_string().contains("missing a name"));
    }

    #[test]
    fn parse_invalid_yaml() {
        let err = parse("not valid yaml: [").unwrap_err();
        assert!(matches!(err, PipelineError::InvalidDefinition(_)));
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
}
