use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

const PROBE_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// Returns whether `model` at `base_url` accepts image inputs.
/// Checks the on-disk cache first; probes the endpoint on cache miss.
/// Falls back to `false` on any I/O or network error.
#[must_use]
pub fn model_supports_vision_with_probe(model: &str, base_url: &str) -> bool {
    let cache_key = format!("{model}@{base_url}");

    // Cache hit
    if let Some(cached) = read_cache().get(&cache_key).copied() {
        return cached;
    }

    // Try OpenAI-compat endpoint first, then Anthropic-compat endpoint
    let result = probe_vision_openai(model, base_url) || probe_vision_anthropic(model, base_url);
    write_cache(&cache_key, result);
    result
}

/// Probe using OpenAI /chat/completions format (Ollama, LM Studio, vLLM, etc.)
fn probe_vision_openai(model: &str, base_url: &str) -> bool {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:image/png;base64,{PROBE_PNG_B64}")
                    }
                },
                {"type": "text", "text": "1"}
            ]
        }],
        "max_tokens": 1,
        "stream": false
    });

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    match client.post(&url).bearer_auth(&api_key).json(&body).send() {
        Ok(resp) => matches!(resp.status().as_u16(), 200..=299 | 400),
        Err(_) => false,
    }
}

/// Probe using Anthropic /v1/messages format (local Anthropic-compat servers)
fn probe_vision_anthropic(model: &str, base_url: &str) -> bool {
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let body = json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": PROBE_PNG_B64
                    }
                },
                {"type": "text", "text": "1"}
            ]
        }]
    });

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    match client
        .post(&url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
    {
        Ok(resp) => matches!(resp.status().as_u16(), 200..=299 | 400),
        Err(_) => false,
    }
}

fn cache_path() -> PathBuf {
    if let Some(config_home) = std::env::var_os("Himalaya_CONFIG_HOME") {
        return PathBuf::from(config_home).join("vision_probe_cache.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".Himalaya")
            .join("vision_probe_cache.json");
    }
    std::env::temp_dir().join("Himalaya-vision-probe-cache.json")
}

fn read_cache() -> HashMap<String, bool> {
    let path = cache_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_cache(key: &str, value: bool) {
    let path = cache_path();
    let mut cache = read_cache();
    cache.insert(key.to_string(), value);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn cache_round_trip() {
        let _guard = crate::test_support::env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "vision-probe-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let _config_home = EnvVarGuard::set_path("Himalaya_CONFIG_HOME", &temp_root);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let key = format!("test-model-{nanos}@http://127.0.0.1:19999/v1");

        // Write
        write_cache(&key, true);

        // Read back
        let cache = read_cache();
        assert_eq!(cache.get(&key).copied(), Some(true));

        drop(_config_home);
        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
    }
}
