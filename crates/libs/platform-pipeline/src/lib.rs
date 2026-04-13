// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Pipeline execution engine: definition parsing, executor loop, trigger utilities.

pub mod config;
pub mod definition;
pub mod error;
pub mod executor;
pub mod state;
pub mod trigger;

pub use error::PipelineError;
pub use state::{PipelineServices, PipelineState};

/// Create a K8s-safe slug from a name.
pub fn slug(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// Convert a git branch name to a K8s-safe DNS label.
///
/// Rules:
/// - Lowercase all characters
/// - Replace `/`, `.`, `_`, `#`, ` ` with `-`
/// - Collapse multiple consecutive `-` into one
/// - Strip leading/trailing `-`
/// - Truncate to 63 characters (K8s DNS label limit)
/// - If empty after processing, return `"preview"`
pub fn slugify_branch(branch: &str) -> String {
    let slug: String = branch
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' | '#' | ' ' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c,
            _ => '-',
        })
        .collect();

    // Collapse multiple dashes
    let mut result = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    // Strip leading/trailing dashes, truncate
    let trimmed = result.trim_matches('-');
    let truncated = if trimmed.len() > 63 {
        // Truncate at 63, but don't end on a dash
        trimmed[..63].trim_end_matches('-')
    } else {
        trimmed
    };

    if truncated.is_empty() {
        "preview".to_string()
    } else {
        truncated.to_string()
    }
}

/// Pipeline status as a simple enum for shared status transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStatus {
    Pending,
    Running,
    Success,
    Failure,
    Cancelled,
}

impl PipelineStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (
                Self::Pending,
                Self::Running | Self::Cancelled | Self::Failure
            ) | (
                Self::Running,
                Self::Success | Self::Failure | Self::Cancelled
            )
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Failure | Self::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- PipelineStatus::parse --

    #[test]
    fn parse_pending() {
        assert_eq!(
            PipelineStatus::parse("pending"),
            Some(PipelineStatus::Pending)
        );
    }

    #[test]
    fn parse_running() {
        assert_eq!(
            PipelineStatus::parse("running"),
            Some(PipelineStatus::Running)
        );
    }

    #[test]
    fn parse_success() {
        assert_eq!(
            PipelineStatus::parse("success"),
            Some(PipelineStatus::Success)
        );
    }

    #[test]
    fn parse_failure() {
        assert_eq!(
            PipelineStatus::parse("failure"),
            Some(PipelineStatus::Failure)
        );
    }

    #[test]
    fn parse_cancelled() {
        assert_eq!(
            PipelineStatus::parse("cancelled"),
            Some(PipelineStatus::Cancelled)
        );
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(PipelineStatus::parse("unknown"), None);
        assert_eq!(PipelineStatus::parse(""), None);
        assert_eq!(PipelineStatus::parse("PENDING"), None);
    }

    // -- PipelineStatus::as_str roundtrip --

    #[test]
    fn as_str_roundtrip() {
        for (s, expected) in [
            ("pending", PipelineStatus::Pending),
            ("running", PipelineStatus::Running),
            ("success", PipelineStatus::Success),
            ("failure", PipelineStatus::Failure),
            ("cancelled", PipelineStatus::Cancelled),
        ] {
            let status = PipelineStatus::parse(s).unwrap();
            assert_eq!(status, expected);
            assert_eq!(status.as_str(), s);
        }
    }

    // -- PipelineStatus::can_transition_to --

    #[test]
    fn transition_pending_to_running() {
        assert!(PipelineStatus::Pending.can_transition_to(PipelineStatus::Running));
    }

    #[test]
    fn transition_pending_to_cancelled() {
        assert!(PipelineStatus::Pending.can_transition_to(PipelineStatus::Cancelled));
    }

    #[test]
    fn transition_pending_to_failure() {
        assert!(PipelineStatus::Pending.can_transition_to(PipelineStatus::Failure));
    }

    #[test]
    fn transition_pending_to_success_invalid() {
        assert!(!PipelineStatus::Pending.can_transition_to(PipelineStatus::Success));
    }

    #[test]
    fn transition_running_to_success() {
        assert!(PipelineStatus::Running.can_transition_to(PipelineStatus::Success));
    }

    #[test]
    fn transition_running_to_failure() {
        assert!(PipelineStatus::Running.can_transition_to(PipelineStatus::Failure));
    }

    #[test]
    fn transition_running_to_cancelled() {
        assert!(PipelineStatus::Running.can_transition_to(PipelineStatus::Cancelled));
    }

    #[test]
    fn transition_running_to_pending_invalid() {
        assert!(!PipelineStatus::Running.can_transition_to(PipelineStatus::Pending));
    }

    #[test]
    fn transition_terminal_states_cannot_transition() {
        for terminal in [
            PipelineStatus::Success,
            PipelineStatus::Failure,
            PipelineStatus::Cancelled,
        ] {
            for target in [
                PipelineStatus::Pending,
                PipelineStatus::Running,
                PipelineStatus::Success,
                PipelineStatus::Failure,
                PipelineStatus::Cancelled,
            ] {
                assert!(
                    !terminal.can_transition_to(target),
                    "{terminal:?} should not transition to {target:?}",
                );
            }
        }
    }

    // -- PipelineStatus::is_terminal --

    #[test]
    fn is_terminal_true_for_terminal_states() {
        assert!(PipelineStatus::Success.is_terminal());
        assert!(PipelineStatus::Failure.is_terminal());
        assert!(PipelineStatus::Cancelled.is_terminal());
    }

    #[test]
    fn is_terminal_false_for_non_terminal() {
        assert!(!PipelineStatus::Pending.is_terminal());
        assert!(!PipelineStatus::Running.is_terminal());
    }

    // -- slug --

    #[test]
    fn slug_simple_name() {
        assert_eq!(slug("MyProject"), "myproject");
    }

    #[test]
    fn slug_with_special_chars() {
        assert_eq!(slug("my-project_v2"), "my-project-v2");
    }

    #[test]
    fn slug_trims_dashes() {
        assert_eq!(slug("-leading-"), "leading");
    }

    // -- slugify_branch --

    #[test]
    fn slugify_feature_branch() {
        assert_eq!(slugify_branch("feature/login"), "feature-login");
    }

    #[test]
    fn slugify_dots_underscores_hashes() {
        assert_eq!(slugify_branch("release.1.0_rc#1"), "release-1-0-rc-1");
    }

    #[test]
    fn slugify_collapses_multiple_dashes() {
        assert_eq!(slugify_branch("a//b--c"), "a-b-c");
    }

    #[test]
    fn slugify_truncates_at_63() {
        let long = "a".repeat(100);
        let result = slugify_branch(&long);
        assert!(result.len() <= 63);
        assert_eq!(result, "a".repeat(63));
    }

    #[test]
    fn slugify_truncation_no_trailing_dash() {
        // 62 a's + "/" + "b" → 62 a's + "-" + "b" → 64 chars → truncated to 63
        // Last char at position 62 is "-" → trimmed
        let branch = format!("{}/b", "a".repeat(62));
        let result = slugify_branch(&branch);
        assert!(result.len() <= 63);
        assert!(!result.ends_with('-'));
    }

    #[test]
    fn slugify_empty_returns_preview() {
        assert_eq!(slugify_branch(""), "preview");
    }

    #[test]
    fn slugify_special_chars_only_returns_preview() {
        assert_eq!(slugify_branch("///"), "preview");
    }

    #[test]
    fn slugify_spaces() {
        assert_eq!(slugify_branch("my branch name"), "my-branch-name");
    }

    #[test]
    fn slugify_preserves_existing_dashes() {
        assert_eq!(slugify_branch("already-slugified"), "already-slugified");
    }
}
