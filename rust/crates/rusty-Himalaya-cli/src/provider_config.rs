/// Persist the interactive model-selection wizard result so the user does not
/// have to re-run it on every CLI launch.
///
/// Model and base URL (non-secrets) are saved alongside the project config in
/// `.Himalaya/provider.json`; the API key is stored in a permission-restricted
/// credential file under the Himalaya config home (chmod 600 on Unix).
use std::collections::BTreeMap;
use std::fs;
use std::io;

use serde::{Deserialize, Serialize};

use crate::model_selector::ModelSelection;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderConfig {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    /// Named profiles saved by the wizard under user-specified names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profiles: BTreeMap<String, ProviderProfile>,
}

/// A named profile bundling model + base URL (api key lives in credentials).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// The project-local file that stores the model wizard choice (model + base_url).
pub fn provider_config_path(project_dir: &std::path::Path) -> std::path::PathBuf {
    project_dir.join(".Himalaya").join("provider.json")
}

/// The credential file under the Himalaya config home for the API key
/// (`$Himalaya_CONFIG_HOME` or `$HOME/.Himalaya`). This mirrors
/// `runtime::config::default_config_home()` (which is private to `runtime`).
pub fn provider_credentials_path() -> std::path::PathBuf {
    let home = std::env::var_os("Himalaya_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".Himalaya"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".Himalaya"));
    home.join("provider_credentials.json")
}

/// Persist the wizard result: model + base_url go to the project-local
/// provider.json; API key goes to the permission-restricted credentials file.
/// On success the env vars are also set so the current session picks them up
/// immediately.
pub fn persist_wizard_selection(
    selection: &ModelSelection,
    project_dir: &std::path::Path,
) -> io::Result<()> {
    // --- project-local config (no secrets) ---
    let config_dir = project_dir.join(".Himalaya");
    fs::create_dir_all(&config_dir)?;
    let config_path = provider_config_path(project_dir);
    let provider = ProviderConfig {
        model: selection.model.clone(),
        base_url: selection.base_url.clone(),
        api_key: None, // never write the key into the project dir
        profiles: BTreeMap::new(),
    };
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&provider)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    )?;

    // --- credential file (key, restricted permissions) ---
    if let Some(key) = &selection.api_key {
        let creds_path = provider_credentials_path();
        if let Some(parent) = creds_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &creds_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "api_key": key,
            }))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&creds_path, fs::Permissions::from_mode(0o600))?;
        }
    }

    Ok(())
}

/// Load a previously persisted wizard selection from the project-local
/// provider.json, and try to read the API key from the credentials file.
/// Returns `None` when neither file exists or they cannot be parsed.
pub fn load_wizard_selection(project_dir: &std::path::Path) -> Option<ModelSelection> {
    let config_path = provider_config_path(project_dir);
    let config: ProviderConfig =
        serde_json::from_str(&fs::read_to_string(&config_path).ok()?).ok()?;
    let api_key = load_api_key_from_credentials();

    Some(ModelSelection {
        model: config.model,
        base_url: config.base_url,
        api_key,
    })
}

fn load_api_key_from_credentials() -> Option<String> {
    let creds_path = provider_credentials_path();
    if !creds_path.exists() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&creds_path).ok()?)
        .ok()
        .and_then(|v| v.get("api_key")?.as_str().map(|s| s.to_string()))
}

/// List all saved profiles (including the last wizard selection as "default").
/// Returns an empty map when no provider.json exists.
pub fn list_profiles(project_dir: &std::path::Path) -> BTreeMap<String, ProviderProfile> {
    let config_path = provider_config_path(project_dir);
    let Ok(config): Result<ProviderConfig, _> =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap_or_default())
    else {
        return BTreeMap::new();
    };
    let mut profiles = config.profiles;
    // Always include the last wizard selection as the "default" profile.
    profiles
        .entry("default".to_string())
        .or_insert(ProviderProfile {
            model: config.model,
            base_url: config.base_url,
        });
    profiles
}

/// Apply a saved profile (by name) to the current environment and return the
/// model to switch to. Returns `None` when the profile does not exist.
pub fn apply_profile(name: &str, project_dir: &std::path::Path) -> Option<ModelSelection> {
    if name == "default" {
        return load_wizard_selection(project_dir);
    }
    let profiles = list_profiles(project_dir);
    let profile = profiles.get(name)?;
    let api_key = load_api_key_from_credentials();
    Some(ModelSelection {
        model: profile.model.clone(),
        base_url: profile.base_url.clone(),
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn persist_and_load_roundtrip_model_and_base_url() {
        let root = std::env::temp_dir().join(format!("himalaya-pc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".Himalaya")).expect("config dir");

        let selection = ModelSelection {
            model: "gpt-4o".to_string(),
            base_url: Some("https://api.example.com".to_string()),
            api_key: None,
        };
        persist_wizard_selection(&selection, &root).expect("persist");

        let loaded = load_wizard_selection(&root).expect("load");
        assert_eq!(loaded.model, "gpt-4o");
        assert_eq!(loaded.base_url.as_deref(), Some("https://api.example.com"));
    }

    #[test]
    fn persist_api_key_and_load_with_restored_env() {
        let root = std::env::temp_dir().join(format!("himalaya-pc-key-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".Himalaya")).expect("config dir");

        // Point credentials to a temp dir so the test is hermetic.
        let creds_root = root.join("creds-home");
        fs::create_dir_all(&creds_root).expect("creds dir");
        std::env::set_var(
            "Himalaya_CONFIG_HOME",
            creds_root.to_string_lossy().to_string(),
        );

        let selection = ModelSelection {
            model: "opus".to_string(),
            base_url: Some("https://api.anthropic.com".to_string()),
            api_key: Some("sk-test-key-123".to_string()),
        };
        persist_wizard_selection(&selection, &root).expect("persist");

        let loaded = load_wizard_selection(&root).expect("load");
        assert_eq!(loaded.model, "opus");
        assert_eq!(loaded.api_key.as_deref(), Some("sk-test-key-123"));

        std::env::remove_var("Himalaya_CONFIG_HOME");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_returns_none_when_no_persisted_config() {
        let root = std::env::temp_dir().join(format!("himalaya-pc-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(load_wizard_selection(&root).is_none());
    }
}
