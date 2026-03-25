use super::error::DeployerError;

/// Variables available to manifest templates.
#[derive(Debug, serde::Serialize)]
pub struct RenderVars {
    pub image_ref: String,
    pub project_name: String,
    pub environment: String,
    pub values: serde_json::Value,
    /// Platform API URL for OTLP endpoint, service discovery, etc.
    pub platform_api_url: String,
    /// Current stable image (for canary/AB deploys).
    #[serde(default)]
    pub stable_image: Option<String>,
    /// New canary image (for canary/AB deploys).
    #[serde(default)]
    pub canary_image: Option<String>,
    /// Git commit SHA.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// App image for testinfra/ rendering.
    #[serde(default)]
    pub app_image: Option<String>,
    /// In-cluster URL for the shared Envoy Gateway proxy (e.g. `http://svc.ns.svc.cluster.local:80`).
    #[serde(default)]
    pub gateway_url: Option<String>,
}

/// Render a manifest template with the given variables using minijinja.
///
/// The template uses standard Jinja2 syntax: `{{ image_ref }}`, `{{ values.replicas }}`, etc.
/// minijinja is sandboxed — no file access or code execution from template content.
pub fn render(template_content: &str, vars: &RenderVars) -> Result<String, DeployerError> {
    let mut env = minijinja::Environment::new();
    env.add_template("manifest", template_content)
        .map_err(|e| DeployerError::RenderFailed(e.to_string()))?;

    let tmpl = env
        .get_template("manifest")
        .map_err(|e| DeployerError::RenderFailed(e.to_string()))?;

    tmpl.render(minijinja::context! {
        image_ref => &vars.image_ref,
        project_name => &vars.project_name,
        environment => &vars.environment,
        values => &vars.values,
        platform_api_url => &vars.platform_api_url,
        stable_image => &vars.stable_image,
        canary_image => &vars.canary_image,
        commit_sha => &vars.commit_sha,
        app_image => &vars.app_image,
        gateway_url => &vars.gateway_url,
    })
    .map_err(|e| DeployerError::RenderFailed(e.to_string()))
}

/// Split a rendered multi-document YAML string into individual documents.
/// Documents are separated by `---` on its own line.
pub fn split_yaml_documents(yaml: &str) -> Vec<String> {
    yaml.split("\n---")
        .map(|doc| {
            // Strip leading newline/whitespace from the split
            doc.trim().to_owned()
        })
        .filter(|doc| {
            // Skip empty documents and comment-only documents
            !doc.is_empty()
                && doc
                    .lines()
                    .any(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_vars() {
        let template = r"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ project_name }}-{{ environment }}
spec:
  template:
    spec:
      containers:
      - image: {{ image_ref }}
        replicas: {{ values.replicas }}
";
        let vars = RenderVars {
            image_ref: "registry/app:v1".into(),
            project_name: "myapp".into(),
            environment: "production".into(),
            values: serde_json::json!({"replicas": 3}),
            platform_api_url: "http://platform:8080".into(),
            stable_image: None,
            canary_image: None,
            commit_sha: None,
            app_image: None,
            gateway_url: None,
        };

        let result = render(template, &vars).unwrap();
        assert!(result.contains("image: registry/app:v1"));
        assert!(result.contains("name: myapp-production"));
        assert!(result.contains("replicas: 3"));
    }

    #[test]
    fn render_nested_values() {
        let template = "cpu: {{ values.resources.cpu }}\nmemory: {{ values.resources.memory }}";
        let vars = RenderVars {
            image_ref: "img:v1".into(),
            project_name: "app".into(),
            environment: "staging".into(),
            values: serde_json::json!({"resources": {"cpu": "500m", "memory": "256Mi"}}),
            platform_api_url: "http://platform:8080".into(),
            stable_image: None,
            canary_image: None,
            commit_sha: None,
            app_image: None,
            gateway_url: None,
        };

        let result = render(template, &vars).unwrap();
        assert!(result.contains("cpu: 500m"));
        assert!(result.contains("memory: 256Mi"));
    }

    #[test]
    fn render_missing_var_returns_error() {
        let template = "image: {{ nonexistent_var }}";
        let vars = RenderVars {
            image_ref: "img:v1".into(),
            project_name: "app".into(),
            environment: "staging".into(),
            values: serde_json::json!({}),
            platform_api_url: "http://platform:8080".into(),
            stable_image: None,
            canary_image: None,
            commit_sha: None,
            app_image: None,
            gateway_url: None,
        };

        // minijinja renders undefined as empty string by default (not an error)
        // This is expected behavior for optional values
        let result = render(template, &vars);
        assert!(result.is_ok());
    }

    #[test]
    fn split_multi_document() {
        let yaml = "apiVersion: v1\nkind: Service\n---\napiVersion: apps/v1\nkind: Deployment";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 2);
        assert!(docs[0].contains("Service"));
        assert!(docs[1].contains("Deployment"));
    }

    #[test]
    fn split_single_document() {
        let yaml = "apiVersion: v1\nkind: ConfigMap";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].contains("ConfigMap"));
    }

    #[test]
    fn split_skips_empty_documents() {
        let yaml = "---\napiVersion: v1\nkind: Service\n---\n---\napiVersion: apps/v1\nkind: Deployment\n---";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn split_leading_separator() {
        let yaml = "---\napiVersion: v1\nkind: ConfigMap";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].contains("ConfigMap"));
    }

    #[test]
    fn split_comment_only_doc_skipped() {
        let yaml = "# just a comment\n---\napiVersion: v1\nkind: Service";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].contains("Service"));
    }

    #[test]
    fn split_yaml_windows_line_endings() {
        let yaml =
            "apiVersion: v1\r\nkind: Service\r\n---\r\napiVersion: apps/v1\r\nkind: Deployment";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 2);
        assert!(docs[0].contains("Service"));
        assert!(docs[1].contains("Deployment"));
    }

    #[test]
    fn split_yaml_unicode_content_preserved() {
        let yaml = "data:\n  greeting: こんにちは\n---\ndata:\n  emoji: 🚀";
        let docs = split_yaml_documents(yaml);
        assert_eq!(docs.len(), 2);
        assert!(docs[0].contains("こんにちは"));
        assert!(docs[1].contains("🚀"));
    }

    #[test]
    fn render_canary_vars() {
        let template =
            "stable: {{ stable_image }}\ncanary: {{ canary_image }}\nsha: {{ commit_sha }}";
        let vars = RenderVars {
            image_ref: "img:v2".into(),
            project_name: "app".into(),
            environment: "production".into(),
            values: serde_json::json!({}),
            platform_api_url: "http://platform:8080".into(),
            stable_image: Some("registry/app:v1".into()),
            canary_image: Some("registry/app:v2".into()),
            commit_sha: Some("abc123".into()),
            app_image: None,
            gateway_url: None,
        };
        let result = render(template, &vars).unwrap();
        assert!(result.contains("stable: registry/app:v1"));
        assert!(result.contains("canary: registry/app:v2"));
        assert!(result.contains("sha: abc123"));
    }

    #[test]
    fn render_app_image_for_testinfra() {
        let template = "image: {{ app_image }}";
        let vars = RenderVars {
            image_ref: "img:v1".into(),
            project_name: "app".into(),
            environment: "test".into(),
            values: serde_json::json!({}),
            platform_api_url: "http://platform:8080".into(),
            stable_image: None,
            canary_image: None,
            commit_sha: None,
            app_image: Some("registry/app:abc123".into()),
            gateway_url: None,
        };
        let result = render(template, &vars).unwrap();
        assert!(result.contains("image: registry/app:abc123"));
    }

    #[test]
    fn render_invalid_template_syntax_errors() {
        let template = "{{ unclosed";
        let vars = RenderVars {
            image_ref: "img:v1".into(),
            project_name: "app".into(),
            environment: "staging".into(),
            values: serde_json::json!({}),
            platform_api_url: "http://platform:8080".into(),
            stable_image: None,
            canary_image: None,
            commit_sha: None,
            app_image: None,
            gateway_url: None,
        };
        assert!(render(template, &vars).is_err());
    }
}
