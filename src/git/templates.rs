/// Template file contents, embedded at compile time.
const PLATFORM_YAML: &str = include_str!("templates/platform.yaml");
const DOCKERFILE: &str = include_str!("templates/Dockerfile");
const DOCKERFILE_DEV: &str = include_str!("templates/Dockerfile.dev");
const DEPLOY_PRODUCTION: &str = include_str!("templates/deploy/production.yaml");
const CLAUDE_MD: &str = include_str!("templates/CLAUDE.md");
const README_TEMPLATE: &str = include_str!("templates/README.md");

/// A file to be committed as part of the project template.
pub struct TemplateFile {
    pub path: &'static str,
    pub content: String,
}

/// Generate the full set of template files for a new project.
///
/// The `project_name` is substituted into the README.md template.
pub fn project_template_files(project_name: &str) -> Vec<TemplateFile> {
    vec![
        TemplateFile {
            path: ".platform.yaml",
            content: PLATFORM_YAML.to_owned(),
        },
        TemplateFile {
            path: "Dockerfile",
            content: DOCKERFILE.to_owned(),
        },
        TemplateFile {
            path: "Dockerfile.dev",
            content: DOCKERFILE_DEV.to_owned(),
        },
        TemplateFile {
            path: "deploy/production.yaml",
            content: DEPLOY_PRODUCTION.to_owned(),
        },
        TemplateFile {
            path: "CLAUDE.md",
            content: CLAUDE_MD.to_owned(),
        },
        TemplateFile {
            path: "README.md",
            content: README_TEMPLATE.replace("{{project_name}}", project_name),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_files_count() {
        let files = project_template_files("test-project");
        assert_eq!(files.len(), 6);
    }

    #[test]
    fn template_readme_contains_project_name() {
        let files = project_template_files("my-awesome-app");
        let readme = files.iter().find(|f| f.path == "README.md").unwrap();
        assert!(readme.content.contains("my-awesome-app"));
        assert!(!readme.content.contains("{{project_name}}"));
    }

    #[test]
    fn template_paths_are_correct() {
        let files = project_template_files("test");
        let paths: Vec<&str> = files.iter().map(|f| f.path).collect();
        assert!(paths.contains(&".platform.yaml"));
        assert!(paths.contains(&"Dockerfile"));
        assert!(paths.contains(&"Dockerfile.dev"));
        assert!(paths.contains(&"deploy/production.yaml"));
        assert!(paths.contains(&"CLAUDE.md"));
        assert!(paths.contains(&"README.md"));
    }

    #[test]
    fn template_platform_yaml_has_kaniko() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("kaniko"));
    }

    #[test]
    fn template_claude_md_has_build_verification() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("Build Verification"));
        assert!(f.content.contains("platform-build-status"));
    }

    #[test]
    fn template_dev_dockerfile_extends_runner() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "Dockerfile.dev").unwrap();
        assert!(f.content.contains("platform-runner"));
    }

    #[test]
    fn template_platform_yaml_has_dev_image() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("dev_image"));
        assert!(f.content.contains("Dockerfile.dev"));
    }

    #[test]
    fn template_claude_md_has_dev_image_docs() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("Dev Image"));
        assert!(f.content.contains("dev_image"));
    }

    #[test]
    fn template_claude_md_has_visual_preview_section() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("Visual Preview"));
        assert!(f.content.contains("port 8000"));
        assert!(f.content.contains("PREVIEW_PORT"));
    }

    #[test]
    fn template_claude_md_has_vite_instructions() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("--host 0.0.0.0"));
        assert!(f.content.contains("--port 8000"));
    }

    #[test]
    fn template_claude_md_has_relative_base() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("base: './'"));
    }
}
