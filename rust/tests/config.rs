//! config env-override tests — Inv 5: `PIFORGE_API_KEY` (env) wins over the
//! toml `api_key`, and the default `"dummy"` is preserved when neither is set.
//!
//! This file is its own integration-test binary, so the `PIFORGE_*` env
//! manipulation here cannot race with other suites; a process-local mutex
//! serializes this file's own tests because `env::set_var` is process-global.
use std::sync::Mutex;

use piforge::config;

// Serialize env access within this binary (env vars are process-global).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn write_temp_toml(name: &str, body: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn api_key_env_overrides_toml() {
    let _g = ENV_LOCK.lock().unwrap();
    let path = write_temp_toml("piforge_test_apikey_env.toml", "[server]\napi_key = \"from-toml\"\n");
    std::env::set_var("PIFORGE_API_KEY", "from-env");
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    std::env::remove_var("PIFORGE_API_KEY");
    assert_eq!(cfg.server.api_key, "from-env", "env must win over toml");
}

#[test]
fn api_key_env_empty_does_not_override() {
    let _g = ENV_LOCK.lock().unwrap();
    let path = write_temp_toml(
        "piforge_test_apikey_empty.toml",
        "[server]\napi_key = \"from-toml\"\n",
    );
    std::env::set_var("PIFORGE_API_KEY", ""); // empty must not clobber the toml value
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    std::env::remove_var("PIFORGE_API_KEY");
    assert_eq!(cfg.server.api_key, "from-toml", "empty env must not override");
}

#[test]
fn api_key_unset_falls_back_to_toml_then_default() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_API_KEY");
    // toml value is used when present and env is absent.
    let path = write_temp_toml("piforge_test_apikey_toml.toml", "[server]\napi_key = \"from-toml\"\n");
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.server.api_key, "from-toml");
    // default "dummy" when neither toml nor env sets it (local llama-server path).
    let cfg = config::load("none").unwrap();
    assert_eq!(cfg.server.api_key, "dummy", "default key must stay dummy");
}

// --- Provider presets: `provider = "<name>"` fills base_url + model from a
// built-in registry; explicit toml/env values win; unknown name errors. ---

#[test]
fn provider_preset_fills_base_url_and_model() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_API_KEY");
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    let path = write_temp_toml(
        "piforge_test_provider.toml",
        "[server]\nprovider = \"zai-coding\"\n",
    );
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.server.base_url, "https://api.z.ai/api/coding/paas/v4");
    assert_eq!(cfg.server.model, "glm-4.6");
}

#[test]
fn provider_preset_explicit_model_wins() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    // An explicit model overrides the preset's default model; base_url still from preset.
    let path = write_temp_toml(
        "piforge_test_provider_model.toml",
        "[server]\nprovider = \"zai-coding\"\nmodel = \"glm-4.7\"\n",
    );
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.server.base_url, "https://api.z.ai/api/coding/paas/v4");
    assert_eq!(cfg.server.model, "glm-4.7");
}

#[test]
fn provider_preset_explicit_base_url_wins() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    // An explicit base_url overrides the preset; manual endpoint wins.
    let path = write_temp_toml(
        "piforge_test_provider_url.toml",
        "[server]\nprovider = \"openai\"\nbase_url = \"https://my-gateway.example/v1\"\n",
    );
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.server.base_url, "https://my-gateway.example/v1");
}

#[test]
fn provider_unknown_errors_with_known_list() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    let path = write_temp_toml(
        "piforge_test_provider_bad.toml",
        "[server]\nprovider = \"nope\"\n",
    );
    let err = config::load(path.to_str().unwrap()).unwrap_err().to_string();
    assert!(err.contains("unknown server.provider"), "{err}");
    assert!(err.contains("zai-coding"), "must list known providers: {err}");
}

#[test]
fn provider_unset_keeps_manual_mode() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    // No provider => base_url/model used as-is (the local/manual path unchanged).
    let path = write_temp_toml(
        "piforge_test_provider_none.toml",
        "[server]\nbase_url = \"http://127.0.0.1:9000/v1\"\nmodel = \"my-local\"\n",
    );
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.server.base_url, "http://127.0.0.1:9000/v1");
    assert_eq!(cfg.server.model, "my-local");
}

#[test]
fn provider_preset_via_env() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_BASE_URL");
    std::env::set_var("PIFORGE_PROVIDER", "openai");
    let cfg = config::load("none").unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    assert_eq!(cfg.server.base_url, "https://api.openai.com/v1");
    assert_eq!(cfg.server.model, "gpt-4o");
}

#[test]
fn provider_preset_case_insensitive() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    let path = write_temp_toml(
        "piforge_test_provider_ci.toml",
        "[server]\nprovider = \"ZAI-Coding\"\n",
    );
    let cfg = config::load(path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.server.base_url, "https://api.z.ai/api/coding/paas/v4");
    assert_eq!(cfg.server.model, "glm-4.6");
}

#[test]
fn env_base_url_beats_provider_preset() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    // PIFORGE_BASE_URL (env) must win over the provider preset's base_url.
    std::env::set_var("PIFORGE_PROVIDER", "openai");
    std::env::set_var("PIFORGE_BASE_URL", "https://env-override.example/v1");
    let cfg = config::load("none").unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    assert_eq!(cfg.server.base_url, "https://env-override.example/v1");
    assert_eq!(cfg.server.model, "gpt-4o", "preset model still applies");
}

#[test]
fn env_model_override_works() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("PIFORGE_PROVIDER");
    std::env::remove_var("PIFORGE_BASE_URL");
    std::env::set_var("PIFORGE_MODEL", "glm-4.7");
    let cfg = config::load("none").unwrap();
    std::env::remove_var("PIFORGE_MODEL");
    assert_eq!(cfg.server.model, "glm-4.7");
}
