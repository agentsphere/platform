/// Template file contents, embedded at compile time.
const PLATFORM_YAML: &str = include_str!("templates/platform.yaml");
const DOCKERFILE: &str = include_str!("templates/Dockerfile");
const DOCKERFILE_TEST: &str = include_str!("templates/Dockerfile.test");
const DOCKERFILE_DEV: &str = include_str!("templates/Dockerfile.dev");
const DEPLOY_PRODUCTION: &str = include_str!("templates/deploy/production.yaml");
const CLAUDE_MD: &str = include_str!("templates/CLAUDE.md");
const README_TEMPLATE: &str = include_str!("templates/README.md");
const DEV_COMMAND: &str = include_str!("templates/.claude/commands/dev.md");

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
            path: "Dockerfile.test",
            content: DOCKERFILE_TEST.to_owned(),
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
        TemplateFile {
            path: ".claude/commands/dev.md",
            content: DEV_COMMAND.to_owned(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_files_count() {
        let files = project_template_files("test-project");
        assert_eq!(files.len(), 8);
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
        assert!(paths.contains(&"Dockerfile.test"));
        assert!(paths.contains(&"Dockerfile.dev"));
        assert!(paths.contains(&"deploy/production.yaml"));
        assert!(paths.contains(&"CLAUDE.md"));
        assert!(paths.contains(&"README.md"));
        assert!(paths.contains(&".claude/commands/dev.md"));
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
    fn template_dockerfile_test_has_pytest() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "Dockerfile.test").unwrap();
        assert!(f.content.contains("pytest"));
        assert!(f.content.contains("APP_HOST"));
    }

    #[test]
    fn template_pipeline_has_build_test() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == ".platform.yaml").unwrap();
        assert!(f.content.contains("build-test"));
        assert!(f.content.contains("Dockerfile.test"));
    }

    #[test]
    fn template_deploy_has_postgres() {
        let files = project_template_files("test");
        let f = files
            .iter()
            .find(|f| f.path == "deploy/production.yaml")
            .unwrap();
        assert!(f.content.contains("postgres"));
        assert!(f.content.contains("DATABASE_URL"));
        assert!(f.content.contains("readinessProbe"));
    }

    #[test]
    fn template_claude_md_has_dev_workflow() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("Development Workflow"));
        assert!(f.content.contains("Create Tests First"));
        assert!(f.content.contains("kubectl"));
    }

    #[test]
    fn template_claude_md_has_deploy_test_docs() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "CLAUDE.md").unwrap();
        assert!(f.content.contains("Deploy-Test Steps"));
        assert!(f.content.contains("deploy_test"));
    }

    #[test]
    fn template_dev_command_has_steps() {
        let files = project_template_files("test");
        let f = files
            .iter()
            .find(|f| f.path == ".claude/commands/dev.md")
            .unwrap();
        assert!(f.content.contains("$ARGUMENTS"));
        assert!(f.content.contains("STEP 1: READ CLAUDE.md"));
        assert!(f.content.contains("STEP 6: TEST LOCALLY"));
        assert!(f.content.contains("kubectl"));
        assert!(f.content.contains("auto_merge"));
    }

    #[test]
    fn template_dockerfile_is_python_app() {
        let files = project_template_files("test");
        let f = files.iter().find(|f| f.path == "Dockerfile").unwrap();
        assert!(f.content.contains("python"));
        assert!(f.content.contains("uvicorn"));
        assert!(f.content.contains("8080"));
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
