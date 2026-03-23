use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::error::PipelineError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Top-level `.platform.yaml` structure.
///
/// Supports both old form (`pipeline:` only) and new form with optional
/// top-level `flags:` and `deploy:` sections.
#[derive(Debug, Deserialize)]
pub struct PlatformFile {
    pub pipeline: PipelineDefinition,
    /// Top-level feature flags — per project, not per app.
    #[serde(default)]
    pub flags: Vec<FlagDef>,
    /// Deploy configuration with specs for canary/AB/rolling.
    #[serde(default)]
    pub deploy: Option<DeployConfig>,
}

/// Backward-compatible alias — used by existing code that refers to `PipelineFile`.
#[allow(dead_code)]
pub type PipelineFile = PlatformFile;

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

// ---------------------------------------------------------------------------
// Feature flag definition (top-level in .platform.yaml)
// ---------------------------------------------------------------------------

/// A feature flag definition in `.platform.yaml`. Pipeline registers these with
/// defaults on each build. Users toggle via UI/API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlagDef {
    pub key: String,
    pub default_value: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Deploy configuration
// ---------------------------------------------------------------------------

/// Top-level `deploy:` section of `.platform.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployConfig {
    /// Opt-in staging environment. Default: false.
    /// NOTE: prefer the `include_staging` project DB setting over this field.
    /// This field is kept for backwards compatibility but `gitops_sync` reads
    /// the DB setting, not this value.
    #[serde(default)]
    pub enable_staging: bool,
    /// Per-environment variable files from the code repo (paths relative to repo root).
    /// These are merged into the ops repo values during `gitops_sync`.
    #[serde(default)]
    pub variables: HashMap<String, String>,
    /// Array of deployment specs (canary, `ab_test`, rolling).
    #[serde(default)]
    pub specs: Vec<DeploySpec>,
}

/// A single deployment spec within `deploy.specs[]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploySpec {
    pub name: String,
    #[serde(rename = "type", default = "default_deploy_type")]
    pub deploy_type: String,
    /// Canary-specific config.
    #[serde(default)]
    pub canary: Option<CanaryConfig>,
    /// A/B test-specific config.
    #[serde(default)]
    pub ab_test: Option<AbTestConfig>,
    /// Whether this spec also deploys to staging (when `enable_staging=true`).
    #[serde(default)]
    pub include_staging: bool,
}

fn default_deploy_type() -> String {
    "rolling".into()
}

/// Canary deployment configuration within a deploy spec.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CanaryConfig {
    pub stable_service: String,
    pub canary_service: String,
    pub steps: Vec<u32>,
    #[serde(default = "default_canary_interval")]
    pub interval: u32,
    #[serde(default = "default_canary_min_requests")]
    pub min_requests: u64,
    #[serde(default = "default_canary_max_failures")]
    pub max_failures: u32,
    #[serde(default)]
    pub progress_gates: Vec<MetricGateConfig>,
    #[serde(default)]
    pub rollback_triggers: Vec<MetricGateConfig>,
}

fn default_canary_interval() -> u32 {
    120
}
fn default_canary_min_requests() -> u64 {
    100
}
fn default_canary_max_failures() -> u32 {
    3
}

/// Metric gate in canary config for progress/rollback.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricGateConfig {
    pub metric: String,
    #[serde(default)]
    pub name: Option<String>,
    pub condition: String,
    pub threshold: f64,
}

/// A/B test configuration within a deploy spec.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbTestConfig {
    pub control_service: String,
    pub treatment_service: String,
    #[serde(rename = "match")]
    pub match_rule: AbTestMatchConfig,
    pub success_metric: String,
    pub success_condition: String,
    #[serde(default = "default_ab_duration")]
    pub duration: u64,
    #[serde(default = "default_ab_min_samples")]
    pub min_samples: u64,
}

fn default_ab_duration() -> u64 {
    86400
}
fn default_ab_min_samples() -> u64 {
    1000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbTestMatchConfig {
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields consumed via serde + executor
pub struct StepDef {
    pub name: String,
    /// Explicit step type. When set, the platform generates the execution plan:
    /// - `imagebuild` — kaniko image build (platform manages registry/push creds)
    /// - `gitops_sync` — copy files to ops repo and publish `OpsRepoUpdated`
    /// - `deploy_watch` — poll `deploy_releases` until terminal phase
    ///
    /// When absent, falls back to legacy behavior (raw commands or `deploy_test`).
    #[serde(rename = "type", default)]
    pub step_type: Option<String>,
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
    /// Image build config (when `type: imagebuild`).
    #[serde(default, rename = "imageName")]
    pub image_name: Option<String>,
    /// Dockerfile path for imagebuild steps (default: Dockerfile).
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// Secret names to inject as kaniko build-arg (imagebuild only).
    #[serde(default)]
    pub secrets: Vec<String>,
    /// `GitOps` sync config (when type is `gitops_sync`).
    #[serde(default)]
    pub gitops: Option<GitopsSyncDef>,
    /// Deploy watch config (when type is `deploy_watch`).
    #[serde(default)]
    pub deploy_watch: Option<DeployWatchDef>,
    /// Quality gate: marks this step as a quality gate (UI/semantic only).
    #[serde(default)]
    pub gate: bool,
}

/// Configuration for a `gitops_sync` step.
#[derive(Debug, Deserialize, Serialize)]
pub struct GitopsSyncDef {
    /// Files/directories to copy from code repo to ops repo.
    #[serde(default)]
    pub copy: Vec<String>,
}

/// Configuration for a `deploy_watch` step.
#[derive(Debug, Deserialize, Serialize)]
pub struct DeployWatchDef {
    /// Which environment to watch (e.g. "staging", "production").
    pub environment: String,
    /// Timeout in seconds (default: 300).
    #[serde(default = "default_deploy_watch_timeout")]
    pub timeout: u64,
}

fn default_deploy_watch_timeout() -> u64 {
    300
}

/// Resolved step kind used by the executor to dispatch execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    /// Raw command execution in a container (legacy).
    Command,
    /// Platform-managed kaniko image build.
    ImageBuild,
    /// Deploy app to test namespace and run test image.
    DeployTest,
    /// Copy files to ops repo and publish `OpsRepoUpdated`.
    GitopsSync,
    /// Poll `deploy_releases` until terminal phase.
    DeployWatch,
}

impl StepDef {
    /// Determine the execution kind for this step.
    pub fn kind(&self) -> StepKind {
        match self.step_type.as_deref() {
            Some("imagebuild") => StepKind::ImageBuild,
            Some("gitops_sync") => StepKind::GitopsSync,
            Some("deploy_watch") => StepKind::DeployWatch,
            _ if self.deploy_test.is_some() => StepKind::DeployTest,
            _ => StepKind::Command,
        }
    }

    /// Whether this step executes inside the executor process (no K8s pod).
    #[allow(dead_code)]
    pub fn is_in_process(&self) -> bool {
        matches!(self.kind(), StepKind::GitopsSync | StepKind::DeployWatch)
    }
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
    /// Deprecated: prefer `wait_for_services` which uses K8s readiness probes.
    #[serde(default = "default_readiness_path")]
    pub readiness_path: String,
    /// Timeout in seconds for app to become ready (default: 120).
    #[serde(default = "default_readiness_timeout")]
    pub readiness_timeout: u32,
    /// Wait for these K8s services to be ready (via K8s readiness probes).
    /// When set, replaces readiness_path-based polling.
    #[serde(default)]
    pub wait_for_services: Vec<String>,
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
    let file = parse_platform_file(yaml)?;
    Ok(file.pipeline)
}

/// Parse a `.platform.yaml` into the full `PlatformFile` (pipeline + flags + deploy).
pub fn parse_platform_file(yaml: &str) -> Result<PlatformFile, PipelineError> {
    let file: PlatformFile =
        serde_yaml::from_str(yaml).map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;

    validate(&file.pipeline)?;
    validate_flags(&file.flags)?;
    if let Some(ref deploy) = file.deploy {
        validate_deploy_config(deploy)?;
    }
    Ok(file)
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

        match step.kind() {
            StepKind::ImageBuild => {
                if step.image_name.as_ref().is_none_or(String::is_empty) {
                    return Err(PipelineError::InvalidDefinition(format!(
                        "step '{}': type=imagebuild requires imageName",
                        step.name,
                    )));
                }
            }
            StepKind::DeployTest => {
                let dt = step.deploy_test.as_ref().unwrap();
                if !step.commands.is_empty() {
                    return Err(PipelineError::InvalidDefinition(format!(
                        "step '{}': deploy_test and commands are mutually exclusive",
                        step.name,
                    )));
                }
                validate_deploy_test(&step.name, dt)?;
            }
            StepKind::GitopsSync => {
                if step.gitops.as_ref().is_none_or(|g| g.copy.is_empty()) {
                    return Err(PipelineError::InvalidDefinition(format!(
                        "step '{}': type=gitops_sync requires gitops.copy with at least one entry",
                        step.name,
                    )));
                }
            }
            StepKind::DeployWatch => {
                if step.deploy_watch.is_none() {
                    return Err(PipelineError::InvalidDefinition(format!(
                        "step '{}': type=deploy_watch requires deploy_watch config",
                        step.name,
                    )));
                }
            }
            StepKind::Command => {
                // Legacy: raw command step needs an image
                if step.image.is_empty() {
                    return Err(PipelineError::InvalidDefinition(format!(
                        "step '{}' is missing an image",
                        step.name
                    )));
                }
            }
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

/// Validate feature flag definitions.
fn validate_flags(flags: &[FlagDef]) -> Result<(), PipelineError> {
    let mut seen = std::collections::HashSet::new();
    for flag in flags {
        if flag.key.is_empty() || flag.key.len() > 255 {
            return Err(PipelineError::InvalidDefinition(
                "flag key must be 1-255 characters".into(),
            ));
        }
        if !flag
            .key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(PipelineError::InvalidDefinition(format!(
                "flag key '{}': only alphanumeric, underscore, hyphen, and dot allowed",
                flag.key
            )));
        }
        if !seen.insert(&flag.key) {
            return Err(PipelineError::InvalidDefinition(format!(
                "duplicate flag key '{}'",
                flag.key
            )));
        }
    }
    Ok(())
}

/// Validate deploy config.
fn validate_deploy_config(deploy: &DeployConfig) -> Result<(), PipelineError> {
    let mut seen_names = std::collections::HashSet::new();
    for spec in &deploy.specs {
        if spec.name.is_empty() || spec.name.len() > 255 {
            return Err(PipelineError::InvalidDefinition(
                "deploy spec name must be 1-255 characters".into(),
            ));
        }
        if !seen_names.insert(&spec.name) {
            return Err(PipelineError::InvalidDefinition(format!(
                "duplicate deploy spec name '{}'",
                spec.name
            )));
        }
        match spec.deploy_type.as_str() {
            "rolling" => {}
            "canary" => {
                let canary = spec.canary.as_ref().ok_or_else(|| {
                    PipelineError::InvalidDefinition(format!(
                        "deploy spec '{}': type 'canary' requires canary config",
                        spec.name
                    ))
                })?;
                validate_canary_config(&spec.name, canary)?;
            }
            "ab_test" => {
                let ab = spec.ab_test.as_ref().ok_or_else(|| {
                    PipelineError::InvalidDefinition(format!(
                        "deploy spec '{}': type 'ab_test' requires ab_test config",
                        spec.name
                    ))
                })?;
                validate_ab_test_config(&spec.name, ab)?;
            }
            other => {
                return Err(PipelineError::InvalidDefinition(format!(
                    "deploy spec '{}': unknown type '{other}' (allowed: rolling, canary, ab_test)",
                    spec.name
                )));
            }
        }
    }
    Ok(())
}

/// Validate canary-specific config.
fn validate_canary_config(spec_name: &str, c: &CanaryConfig) -> Result<(), PipelineError> {
    if c.stable_service.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': canary.stable_service must not be empty"
        )));
    }
    if c.canary_service.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': canary.canary_service must not be empty"
        )));
    }
    if c.steps.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': canary.steps must not be empty"
        )));
    }
    // Steps must be ascending
    for window in c.steps.windows(2) {
        if window[0] >= window[1] {
            return Err(PipelineError::InvalidDefinition(format!(
                "deploy spec '{spec_name}': canary.steps must be strictly ascending"
            )));
        }
    }
    // Last step must be 100
    if *c.steps.last().unwrap() != 100 {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': canary.steps must end with 100"
        )));
    }
    // All steps must be 1-100
    for &step in &c.steps {
        if step == 0 || step > 100 {
            return Err(PipelineError::InvalidDefinition(format!(
                "deploy spec '{spec_name}': canary step values must be 1-100"
            )));
        }
    }
    if c.interval == 0 {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': canary.interval must be > 0"
        )));
    }
    for gate in &c.progress_gates {
        validate_metric_gate(spec_name, "progress_gate", gate)?;
    }
    for trigger in &c.rollback_triggers {
        validate_metric_gate(spec_name, "rollback_trigger", trigger)?;
    }
    Ok(())
}

/// Validate A/B test config.
fn validate_ab_test_config(spec_name: &str, ab: &AbTestConfig) -> Result<(), PipelineError> {
    if ab.control_service.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': ab_test.control_service must not be empty"
        )));
    }
    if ab.treatment_service.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': ab_test.treatment_service must not be empty"
        )));
    }
    if ab.success_metric.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': ab_test.success_metric must not be empty"
        )));
    }
    let valid_conditions = ["gt", "lt", "eq"];
    if !valid_conditions.contains(&ab.success_condition.as_str()) {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': ab_test.success_condition must be gt, lt, or eq"
        )));
    }
    if ab.duration == 0 {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': ab_test.duration must be > 0"
        )));
    }
    Ok(())
}

/// Validate a metric gate definition.
fn validate_metric_gate(
    spec_name: &str,
    gate_kind: &str,
    gate: &MetricGateConfig,
) -> Result<(), PipelineError> {
    if gate.metric.is_empty() {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': {gate_kind} metric must not be empty"
        )));
    }
    if gate.metric == "custom" && gate.name.as_ref().is_none_or(String::is_empty) {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': custom {gate_kind} requires a 'name' field"
        )));
    }
    let valid_conditions = ["gt", "lt", "eq"];
    if !valid_conditions.contains(&gate.condition.as_str()) {
        return Err(PipelineError::InvalidDefinition(format!(
            "deploy spec '{spec_name}': {gate_kind} condition must be gt, lt, or eq"
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
                step_type: None,
                image_name: None,
                dockerfile: None,
                secrets: vec![],
                gitops: None,
                deploy_watch: None,
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
                step_type: None,
                image_name: None,
                dockerfile: None,
                secrets: vec![],
                gitops: None,
                deploy_watch: None,
            },
        ];
        assert!(topological_layers(&steps).is_none());
    }

    // -- PlatformFile (flags + deploy) parsing --

    #[test]
    fn parse_platform_file_with_flags() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
flags:
  - key: new_checkout_flow
    default_value: false
    description: "New checkout UI flow"
  - key: dark_mode_v2
    default_value: false
"#;
        let file = parse_platform_file(yaml).unwrap();
        assert_eq!(file.flags.len(), 2);
        assert_eq!(file.flags[0].key, "new_checkout_flow");
        assert_eq!(file.flags[0].default_value, serde_json::json!(false));
        assert_eq!(
            file.flags[0].description.as_deref(),
            Some("New checkout UI flow")
        );
        assert_eq!(file.flags[1].key, "dark_mode_v2");
    }

    #[test]
    fn parse_platform_file_no_flags() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
";
        let file = parse_platform_file(yaml).unwrap();
        assert!(file.flags.is_empty());
        assert!(file.deploy.is_none());
    }

    #[test]
    fn parse_deploy_canary_config() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  enable_staging: false
  specs:
    - name: api
      type: canary
      canary:
        stable_service: api-stable
        canary_service: api-canary
        steps: [5, 20, 50, 80, 100]
        interval: 120
        min_requests: 100
        progress_gates:
          - metric: error_rate
            condition: lt
            threshold: 0.05
        rollback_triggers:
          - metric: error_rate
            condition: gt
            threshold: 0.50
"#;
        let file = parse_platform_file(yaml).unwrap();
        let deploy = file.deploy.unwrap();
        assert!(!deploy.enable_staging);
        assert_eq!(deploy.specs.len(), 1);
        let spec = &deploy.specs[0];
        assert_eq!(spec.name, "api");
        assert_eq!(spec.deploy_type, "canary");
        let canary = spec.canary.as_ref().unwrap();
        assert_eq!(canary.steps, vec![5, 20, 50, 80, 100]);
        assert_eq!(canary.progress_gates.len(), 1);
        assert_eq!(canary.rollback_triggers.len(), 1);
    }

    #[test]
    fn parse_deploy_ab_test_config() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: checkout-experiment
      type: ab_test
      ab_test:
        control_service: checkout-control
        treatment_service: checkout-treatment
        match:
          headers:
            x-experiment: treatment
        success_metric: custom/conversion_rate
        success_condition: gt
        duration: 86400
        min_samples: 1000
"#;
        let file = parse_platform_file(yaml).unwrap();
        let spec = &file.deploy.unwrap().specs[0];
        assert_eq!(spec.deploy_type, "ab_test");
        let ab = spec.ab_test.as_ref().unwrap();
        assert_eq!(ab.control_service, "checkout-control");
        assert_eq!(ab.treatment_service, "checkout-treatment");
        assert_eq!(
            ab.match_rule.headers.get("x-experiment"),
            Some(&"treatment".to_string())
        );
        assert_eq!(ab.success_metric, "custom/conversion_rate");
        assert_eq!(ab.duration, 86400);
    }

    #[test]
    fn parse_deploy_rolling_spec() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: rolling
";
        let file = parse_platform_file(yaml).unwrap();
        let spec = &file.deploy.unwrap().specs[0];
        assert_eq!(spec.deploy_type, "rolling");
        assert!(spec.canary.is_none());
        assert!(spec.ab_test.is_none());
    }

    #[test]
    fn validate_canary_steps_ascending() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: canary
      canary:
        stable_service: s
        canary_service: c
        steps: [50, 20, 100]
"#;
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("ascending")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_canary_steps_must_end_at_100() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: canary
      canary:
        stable_service: s
        canary_service: c
        steps: [10, 50]
"#;
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("end with 100")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_canary_requires_config() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: canary
";
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("requires canary config")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_ab_test_requires_config() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: exp
      type: ab_test
";
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("requires ab_test config")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_unknown_deploy_type() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: blue_green
";
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("unknown type")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_duplicate_spec_names() {
        let yaml = r"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: rolling
    - name: api
      type: rolling
";
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("duplicate deploy spec")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_flag_key_invalid_chars() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
flags:
  - key: "has spaces"
    default_value: false
"#;
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("alphanumeric")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_duplicate_flag_keys() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
flags:
  - key: my_flag
    default_value: false
  - key: my_flag
    default_value: true
"#;
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("duplicate flag")),
            "got: {err:?}"
        );
    }

    #[test]
    fn validate_custom_metric_gate_requires_name() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  specs:
    - name: api
      type: canary
      canary:
        stable_service: s
        canary_service: c
        steps: [100]
        progress_gates:
          - metric: custom
            condition: lt
            threshold: 0.05
"#;
        let err = parse_platform_file(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("custom") && msg.contains("name")),
            "got: {err:?}"
        );
    }

    #[test]
    fn parse_wait_for_services() {
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: registry/test:v1
        manifests: testinfra/
        wait_for_services: [api, postgres]
"#;
        let def = parse(yaml).unwrap();
        let dt = def.steps[0].deploy_test.as_ref().unwrap();
        assert_eq!(dt.wait_for_services, vec!["api", "postgres"]);
    }

    #[test]
    fn parse_full_platform_yaml() {
        let yaml = r#"
pipeline:
  on:
    push:
      branches: ["main"]
    mr:
      actions: [opened, synchronized]
  steps:
    - name: build-app
      image: gcr.io/kaniko-project/executor:debug
      commands:
        - /kaniko/executor --context=. --dockerfile=Dockerfile
    - name: build-test
      image: gcr.io/kaniko-project/executor:debug
      commands:
        - /kaniko/executor --context=. --dockerfile=Dockerfile.test
      only:
        events: [mr]
    - name: e2e
      depends_on: [build-app, build-test]
      gate: true
      deploy_test:
        test_image: $REGISTRY/$PROJECT/test:$COMMIT_SHA
        manifests: testinfra/
        wait_for_services: [api, postgres]
      only:
        events: [mr]
  dev_image:
    dockerfile: Dockerfile.dev

flags:
  - key: new_checkout_flow
    default_value: false
    description: "New checkout UI flow"
  - key: dark_mode_v2
    default_value: false

deploy:
  enable_staging: false
  specs:
    - name: api
      type: canary
      canary:
        stable_service: api-stable
        canary_service: api-canary
        steps: [5, 20, 50, 80, 100]
        interval: 120
        min_requests: 100
        progress_gates:
          - metric: error_rate
            condition: lt
            threshold: 0.05
          - metric: latency_p95
            condition: lt
            threshold: 500
          - metric: custom
            name: checkout_failures
            condition: lt
            threshold: 0.02
        rollback_triggers:
          - metric: error_rate
            condition: gt
            threshold: 0.50
"#;
        let file = parse_platform_file(yaml).unwrap();
        assert_eq!(file.pipeline.steps.len(), 3);
        assert_eq!(file.flags.len(), 2);
        let deploy = file.deploy.unwrap();
        assert!(!deploy.enable_staging);
        assert_eq!(deploy.specs.len(), 1);
        let canary = deploy.specs[0].canary.as_ref().unwrap();
        assert_eq!(canary.progress_gates.len(), 3);
        assert_eq!(canary.rollback_triggers.len(), 1);
        assert_eq!(canary.progress_gates[2].metric, "custom");
        assert_eq!(
            canary.progress_gates[2].name.as_deref(),
            Some("checkout_failures")
        );
    }

    // -- New step type parsing --

    #[test]
    fn parse_imagebuild_step() {
        let yaml = r#"
pipeline:
  steps:
    - name: build-app
      type: imagebuild
      imageName: app
      dockerfile: Dockerfile
      secrets:
        - MY_SECRET
"#;
        let def = parse(yaml).unwrap();
        let step = &def.steps[0];
        assert_eq!(step.step_type.as_deref(), Some("imagebuild"));
        assert_eq!(step.image_name.as_deref(), Some("app"));
        assert_eq!(step.dockerfile.as_deref(), Some("Dockerfile"));
        assert_eq!(step.secrets, vec!["MY_SECRET"]);
        assert_eq!(step.kind(), StepKind::ImageBuild);
    }

    #[test]
    fn imagebuild_missing_image_name_rejected() {
        let yaml = r#"
pipeline:
  steps:
    - name: build-app
      type: imagebuild
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("imageName")),
            "got: {err:?}"
        );
    }

    #[test]
    fn parse_gitops_sync_step() {
        let yaml = r#"
pipeline:
  steps:
    - name: sync
      type: gitops_sync
      gitops:
        copy: ["deploy/", ".platform.yaml"]
"#;
        let def = parse(yaml).unwrap();
        let step = &def.steps[0];
        assert_eq!(step.kind(), StepKind::GitopsSync);
        let gitops = step.gitops.as_ref().unwrap();
        assert_eq!(gitops.copy, vec!["deploy/", ".platform.yaml"]);
    }

    #[test]
    fn gitops_sync_missing_copy_rejected() {
        let yaml = r#"
pipeline:
  steps:
    - name: sync
      type: gitops_sync
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("gitops.copy")),
            "got: {err:?}"
        );
    }

    #[test]
    fn parse_deploy_watch_step() {
        let yaml = r#"
pipeline:
  steps:
    - name: watch
      type: deploy_watch
      deploy_watch:
        environment: staging
        timeout: 600
"#;
        let def = parse(yaml).unwrap();
        let step = &def.steps[0];
        assert_eq!(step.kind(), StepKind::DeployWatch);
        let dw = step.deploy_watch.as_ref().unwrap();
        assert_eq!(dw.environment, "staging");
        assert_eq!(dw.timeout, 600);
    }

    #[test]
    fn deploy_watch_missing_config_rejected() {
        let yaml = r#"
pipeline:
  steps:
    - name: watch
      type: deploy_watch
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, PipelineError::InvalidDefinition(ref msg) if msg.contains("deploy_watch")),
            "got: {err:?}"
        );
    }

    #[test]
    fn deploy_watch_default_timeout() {
        let yaml = r#"
pipeline:
  steps:
    - name: watch
      type: deploy_watch
      deploy_watch:
        environment: production
"#;
        let def = parse(yaml).unwrap();
        let dw = def.steps[0].deploy_watch.as_ref().unwrap();
        assert_eq!(dw.timeout, 300);
    }

    #[test]
    fn step_kind_infers_correctly() {
        // deploy_test inferred from field, not type
        let yaml = r#"
pipeline:
  steps:
    - name: e2e
      deploy_test:
        test_image: my-image:latest
        manifests: testinfra/
"#;
        let def = parse(yaml).unwrap();
        assert_eq!(def.steps[0].kind(), StepKind::DeployTest);

        // command step (no type, has image)
        let yaml2 = r"
pipeline:
  steps:
    - name: test
      image: alpine
      commands: ['echo hi']
";
        let def2 = parse(yaml2).unwrap();
        assert_eq!(def2.steps[0].kind(), StepKind::Command);
    }

    #[test]
    fn parse_deploy_variables() {
        let yaml = r#"
pipeline:
  steps:
    - name: build
      image: alpine
deploy:
  variables:
    staging: deploy/variables_staging.yaml
    production: deploy/variables_prod.yaml
  specs: []
"#;
        let pf = parse_platform_file(yaml).unwrap();
        let deploy = pf.deploy.unwrap();
        assert_eq!(
            deploy.variables.get("staging").unwrap(),
            "deploy/variables_staging.yaml"
        );
        assert_eq!(
            deploy.variables.get("production").unwrap(),
            "deploy/variables_prod.yaml"
        );
    }

    #[test]
    fn parse_full_demo_platform_yaml() {
        let yaml = r#"
pipeline:
  on:
    push:
      branches: ["main"]
    mr:
      actions: [opened, synchronized]
  steps:
    - name: build-app
      type: imagebuild
      imageName: app
      dockerfile: Dockerfile
    - name: build-canary
      type: imagebuild
      imageName: canary
      dockerfile: Dockerfile.canary
    - name: build-dev
      type: imagebuild
      imageName: dev
      dockerfile: Dockerfile.dev
    - name: build-test
      type: imagebuild
      imageName: test
      dockerfile: Dockerfile.test
      only:
        events: [mr]
    - name: e2e
      depends_on: [build-app, build-test]
      only:
        events: [mr]
      deploy_test:
        test_image: registry/test:sha
        manifests: testinfra/
        readiness_timeout: 120
        wait_for_services: [app, db]
    - name: sync-ops-repo
      type: gitops_sync
      depends_on: [build-app, build-canary]
      only:
        events: [push]
        branches: ["main"]
      gitops:
        copy: ["deploy/", ".platform.yaml"]
    - name: watch-deploy
      type: deploy_watch
      depends_on: [sync-ops-repo]
      only:
        events: [push]
        branches: ["main"]
      deploy_watch:
        environment: staging
        timeout: 300
flags:
  - key: new_checkout_flow
    default_value: false
  - key: dark_mode
    default_value: false
deploy:
  variables:
    staging: deploy/variables_staging.yaml
    production: deploy/variables_prod.yaml
  specs:
    - name: api
      type: canary
      canary:
        stable_service: app-stable
        canary_service: app-canary
        steps: [10, 25, 50, 100]
        interval: 120
        progress_gates:
          - metric: error_rate
            condition: lt
            threshold: 0.05
"#;
        let pf = parse_platform_file(yaml).unwrap();
        assert_eq!(pf.pipeline.steps.len(), 7);
        assert_eq!(pf.flags.len(), 2);
        assert!(pf.deploy.is_some());

        // Verify step kinds
        let kinds: Vec<StepKind> = pf.pipeline.steps.iter().map(|s| s.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::ImageBuild,
                StepKind::ImageBuild,
                StepKind::ImageBuild,
                StepKind::ImageBuild,
                StepKind::DeployTest,
                StepKind::GitopsSync,
                StepKind::DeployWatch,
            ]
        );
    }
}
