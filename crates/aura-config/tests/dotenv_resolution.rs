//! Integration coverage for the mechanism the binaries rely on: a `.env`
//! loaded into the process environment supplies `{{ env.* }}` references
//! in config TOML files.
//!
//! `aura-web-server` and the standalone CLI call `dotenvy::dotenv()` at startup
//! before `load_config_with_paths`; this test exercises that load → resolve
//! path end to end with `dotenvy::from_path` (an explicit path keeps it
//! independent of the test's working directory).

use aura_config::LlmConfig;

#[test]
fn dotenv_supplies_env_vars_for_config_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let env_path = dir.path().join(".env");
    let cfg_path = dir.path().join("config.toml");

    std::fs::write(
        &env_path,
        "OPENAI_API_KEY=sk-test\n",
    )
    .unwrap();
    std::fs::write(
        &cfg_path,
        r#"
[agent]
name = "assistant"
system_prompt = "hi"

[agent.llm]
provider = "openai"
api_key = "{{ env.OPENAI_API_KEY }}"
model = "gpt-test"
"#,
    )
    .unwrap();

    // Clear first so `dotenvy` (which never overrides an existing var) is the
    // one that supplies them. Sole test in this binary — no concurrent env use.
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }

    dotenvy::from_path(&env_path).expect("load .env");

    let configs = aura_config::load_config(cfg_path.to_str().unwrap()).expect("load config");
    let config = &configs[0];

    match &config.agent.llm {
        LlmConfig::OpenAI { model, api_key, .. } => {
            assert_eq!(model, "gpt-test");
            assert_eq!(api_key, "sk-test");
        }
        other => panic!("expected OpenAI variant, got {other:?}"),
    }
}
