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
