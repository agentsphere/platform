pub mod definition;
pub mod error;
pub mod executor;
pub mod trigger;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_simple_branch() {
        assert_eq!(slugify_branch("feature/add-login"), "feature-add-login");
    }

    #[test]
    fn slugify_complex_branch() {
        assert_eq!(
            slugify_branch("feature/CAPS_and.dots#hash"),
            "feature-caps-and-dots-hash"
        );
    }

    #[test]
    fn slugify_collapses_dashes() {
        assert_eq!(slugify_branch("a//b..c__d"), "a-b-c-d");
    }

    #[test]
    fn slugify_strips_edges() {
        assert_eq!(slugify_branch("/leading/"), "leading");
        assert_eq!(slugify_branch("---"), "preview");
    }

    #[test]
    fn slugify_truncates_to_63() {
        let long = "a".repeat(100);
        assert!(slugify_branch(&long).len() <= 63);
    }

    #[test]
    fn slugify_handles_empty() {
        assert_eq!(slugify_branch(""), "preview");
    }

    #[test]
    fn slugify_preserves_numbers() {
        assert_eq!(slugify_branch("release/v1.2.3"), "release-v1-2-3");
    }

    #[test]
    fn slugify_truncate_does_not_end_on_dash() {
        // 62 a's + dash + rest = when truncated to 63, should not end on dash
        let branch = format!("{}/-rest", "a".repeat(62));
        let result = slugify_branch(&branch);
        assert!(result.len() <= 63);
        assert!(!result.ends_with('-'));
    }

    #[test]
    fn slugify_all_special_chars() {
        assert_eq!(slugify_branch("!!!@@@"), "preview");
    }

    #[test]
    fn slugify_spaces() {
        assert_eq!(slugify_branch("my feature branch"), "my-feature-branch");
    }
}
