use std::path::Path;

use sqlx::PgPool;

/// Seed global commands from `.md` files in the given directory.
///
/// Each file becomes a global command: filename stem = command name, file contents = prompt template.
/// An optional sibling `.json` file provides metadata (`description`, `persistent_session`).
///
/// Idempotent: uses `ON CONFLICT DO NOTHING` — never overwrites admin edits.
#[tracing::instrument(skip(pool), err)]
pub async fn seed_commands(pool: &PgPool, seed_path: &Path) -> Result<(), anyhow::Error> {
    if !seed_path.exists() {
        tracing::debug!(path = %seed_path.display(), "seed commands path does not exist, skipping");
        return Ok(());
    }

    let mut count = 0u32;
    let entries = std::fs::read_dir(seed_path)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };

        let template = std::fs::read_to_string(&path)?;
        if template.is_empty() {
            continue;
        }

        // Optional metadata from sibling .json file
        let (description, persistent_session) = read_metadata(&path);

        // Insert as global command (workspace_id=NULL, project_id=NULL).
        // ON CONFLICT DO NOTHING: never overwrite admin edits.
        sqlx::query(
            r#"INSERT INTO platform_commands (workspace_id, project_id, name, description, prompt_template, persistent_session)
               VALUES (NULL, NULL, $1, $2, $3, $4)
               ON CONFLICT (
                   COALESCE(workspace_id, '00000000-0000-0000-0000-000000000000'::uuid),
                   COALESCE(project_id,   '00000000-0000-0000-0000-000000000000'::uuid),
                   name
               ) DO NOTHING"#,
        )
        .bind(&name)
        .bind(&description)
        .bind(&template)
        .bind(persistent_session)
        .execute(pool)
        .await?;

        count += 1;
    }

    if count > 0 {
        tracing::info!(count, "seeded global commands");
    }
    Ok(())
}

/// Read optional metadata from a sibling `.json` file.
fn read_metadata(md_path: &Path) -> (String, bool) {
    let json_path = md_path.with_extension("json");
    if !json_path.exists() {
        return (String::new(), false);
    }

    #[derive(serde::Deserialize)]
    struct Meta {
        #[serde(default)]
        description: String,
        #[serde(default)]
        persistent_session: bool,
    }

    match std::fs::read_to_string(&json_path) {
        Ok(content) => match serde_json::from_str::<Meta>(&content) {
            Ok(meta) => (meta.description, meta.persistent_session),
            Err(e) => {
                tracing::warn!(path = %json_path.display(), error = %e, "invalid command metadata JSON");
                (String::new(), false)
            }
        },
        Err(e) => {
            tracing::warn!(path = %json_path.display(), error = %e, "failed to read command metadata");
            (String::new(), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_metadata_missing_file() {
        let (desc, persistent) = read_metadata(Path::new("/nonexistent/test.md"));
        assert!(desc.is_empty());
        assert!(!persistent);
    }

    #[test]
    fn read_metadata_from_json() {
        let dir = std::env::temp_dir().join(format!("cmd-seed-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let md_path = dir.join("dev.md");
        std::fs::write(&md_path, "template").unwrap();
        let json_path = dir.join("dev.json");
        std::fs::write(
            &json_path,
            r#"{"description": "Full dev workflow", "persistent_session": true}"#,
        )
        .unwrap();

        let (desc, persistent) = read_metadata(&md_path);
        assert_eq!(desc, "Full dev workflow");
        assert!(persistent);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
