//! JSON Schema generation for the agent TOML config surface.
//!
//! The generated schema is a linting layer in front of the parser, not a
//! replacement for it: it is stricter than serde on unknown keys (serde
//! silently ignores them on most tables; the schema rejects them so editors
//! and CI catch typos) and looser on constrained string values (which may
//! carry `{{ env.VAR }}` templates in the raw file). Cross-field rules stay
//! in `Config::validate`.
//!
//! The checked-in copy lives at `schema/aura-config.schema.json`; the
//! `schema_file_up_to_date` test is the drift gate.

use serde_json::{Map, Value, json};

/// Unanchored match for `{{ env.VAR }}` / `{{ env.VAR | default: '...' }}`.
/// Env resolution substitutes these before parsing, so a constrained string
/// field must accept a raw config that still carries one (the quickstart
/// templates even the `provider` discriminant).
const ENV_TEMPLATE_PATTERN: &str =
    r"\{\{\s*env\.[A-Z_][A-Z0-9_]*(\s*\|\s*default:\s*'[^']*')?\s*\}\}";

/// Generate the JSON Schema (draft 2020-12) for the agent config TOML surface.
pub fn config_schema() -> Value {
    let schema = schemars::schema_for!(crate::Config);
    let mut value = serde_json::to_value(schema).expect("schema serializes to JSON");
    for_each_schema(&mut value, &mut collapse_nullable);
    for_each_schema(&mut value, &mut widen_template_strings);
    for_each_schema(&mut value, &mut one_of_to_any_of);
    close_schemas(&mut value, false);
    add_memory_path_alias(&mut value);
    strip_volatile_defaults(&mut value);
    value
        .as_object_mut()
        .expect("root schema is an object")
        .insert("title".to_owned(), json!("AURA agent configuration"));
    value
}

/// Apply `f` to every subschema, post-order (children before the node itself,
/// so a rewrite that nests the node's original contents is not re-visited).
fn for_each_schema<F: FnMut(&mut Map<String, Value>)>(value: &mut Value, f: &mut F) {
    let Value::Object(map) = value else { return };
    for key in [
        "additionalProperties",
        "unevaluatedProperties",
        "items",
        "not",
    ] {
        if let Some(child) = map.get_mut(key) {
            for_each_schema(child, f);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(children)) = map.get_mut(key) {
            for child in children {
                for_each_schema(child, f);
            }
        }
    }
    for key in ["properties", "$defs", "patternProperties"] {
        if let Some(Value::Object(children)) = map.get_mut(key) {
            for child in children.values_mut() {
                for_each_schema(child, f);
            }
        }
    }
    f(map);
}

/// TOML cannot express `null`, so the nullable wrappers schemars emits for
/// `Option` fields (`anyOf` with a null branch, `type` arrays with "null")
/// never admit anything a config file can contain; they only obscure
/// validator errors ("not valid under anyOf" instead of the offending key)
/// and editor completion. Collapse them to the non-null schema.
fn collapse_nullable(map: &mut Map<String, Value>) {
    if let Some(Value::Array(branches)) = map.get("anyOf")
        && branches.len() == 2
        && let Some(null_pos) = branches
            .iter()
            .position(|b| b.get("type") == Some(&json!("null")))
    {
        let Some(Value::Array(mut branches)) = map.remove("anyOf") else {
            unreachable!("checked above");
        };
        branches.remove(null_pos);
        if let Value::Object(branch) = branches.remove(0) {
            for (key, value) in branch {
                map.entry(key).or_insert(value);
            }
        }
    }
    if let Some(Value::Array(types)) = map.get_mut("type")
        && types.len() == 2
        && let Some(null_pos) = types.iter().position(|t| t == "null")
    {
        types.remove(null_pos);
        let only = types.remove(0);
        map.insert("type".to_owned(), only);
    }
}

/// Rewrite a value-constrained string schema (`enum`/`const`/`pattern`) into
/// an `anyOf` that also accepts an env-template string.
fn widen_template_strings(map: &mut Map<String, Value>) {
    let is_string = map.get("type").is_some_and(|t| t == "string");
    let constrained =
        map.contains_key("enum") || map.contains_key("const") || map.contains_key("pattern");
    if !is_string || !constrained {
        return;
    }
    if map
        .get("pattern")
        .is_some_and(|p| p == ENV_TEMPLATE_PATTERN)
    {
        return;
    }
    let mut original = std::mem::take(map);
    if let Some(description) = original.remove("description") {
        map.insert("description".to_owned(), description);
    }
    map.insert(
        "anyOf".to_owned(),
        json!([original, { "type": "string", "pattern": ENV_TEMPLATE_PATTERN }]),
    );
}

/// Template-widened tag consts can match more than one variant of a tagged
/// enum, so exactly-one (`oneOf`) semantics must relax to at-least-one.
/// Rewrites every `oneOf`: the only producers here are schemars' tagged-enum
/// output, where at-least-one is safe.
fn one_of_to_any_of(map: &mut Map<String, Value>) {
    if let Some(variants) = map.remove("oneOf") {
        map.insert("anyOf".to_owned(), variants);
    }
}

/// Close every object schema against unknown keys (serde silently ignores
/// them on most tables; the schema rejects them so editors and CI catch
/// typos). A schema that composes (`$ref` or a combinator alongside
/// `properties`, e.g. the flattened `[[vector_stores]]` entry) gets
/// `unevaluatedProperties` so the closure spans the composed parts; a plain
/// object gets `additionalProperties: false`.
///
/// `keep_open` marks a combinator branch of a composing schema: the branch
/// shares its parent's instance location, so closing it would reject the
/// parent's own keys — the parent's `unevaluatedProperties` is the closure.
fn close_schemas(value: &mut Value, keep_open: bool) {
    let Value::Object(map) = value else { return };
    const COMBINATORS: [&str; 3] = ["anyOf", "oneOf", "allOf"];
    let has_properties = map.contains_key("properties");
    let composes = map.contains_key("$ref") || COMBINATORS.iter().any(|key| map.contains_key(*key));

    if has_properties && !keep_open {
        if composes {
            map.entry("unevaluatedProperties").or_insert(json!(false));
        } else if !map.contains_key("additionalProperties")
            && !map.contains_key("patternProperties")
        {
            map.insert("additionalProperties".to_owned(), json!(false));
        }
    }

    // An open node's nested combinator branches still share the ancestor's
    // instance location, so openness propagates through them.
    let branches_open = keep_open || (has_properties && composes);
    for key in COMBINATORS {
        if let Some(Value::Array(children)) = map.get_mut(key) {
            for child in children {
                close_schemas(child, branches_open);
            }
        }
    }
    for key in [
        "additionalProperties",
        "unevaluatedProperties",
        "items",
        "not",
    ] {
        if let Some(child) = map.get_mut(key) {
            close_schemas(child, false);
        }
    }
    for key in ["properties", "$defs", "patternProperties"] {
        if let Some(Value::Object(children)) = map.get_mut(key) {
            for child in children.values_mut() {
                close_schemas(child, false);
            }
        }
    }
}

/// `created_at` defaults to the wall clock, which schemars evaluates at
/// generation time; drop it so the schema is deterministic.
fn strip_volatile_defaults(value: &mut Value) {
    value
        .pointer_mut("/$defs/AgentConfig/properties/created_at")
        .and_then(Value::as_object_mut)
        .expect("AgentConfig.created_at missing from schema")
        .remove("default");
}

/// `memory_dir` accepts the serde alias `memory_path` at `[orchestration]`
/// and `[orchestration.artifacts]` (top-level `memory_dir` has no alias);
/// schemars does not emit aliases, so add the property here.
fn add_memory_path_alias(value: &mut Value) {
    for def_name in ["OrchestrationConfig", "ArtifactsConfig"] {
        let properties = value
            .pointer_mut(&format!("/$defs/{def_name}/properties"))
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("{def_name} properties missing from schema"));
        let memory_dir = properties
            .get("memory_dir")
            .unwrap_or_else(|| panic!("{def_name}.memory_dir missing from schema"))
            .clone();
        properties.insert("memory_path".to_owned(), memory_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn validator() -> jsonschema::Validator {
        jsonschema::validator_for(&config_schema()).expect("generated schema compiles")
    }

    fn toml_to_json(contents: &str) -> Value {
        let value: toml::Value = toml::from_str(contents).expect("valid TOML");
        serde_json::to_value(value).expect("TOML converts to JSON")
    }

    fn validation_errors(validator: &jsonschema::Validator, contents: &str) -> Vec<String> {
        validator
            .iter_errors(&toml_to_json(contents))
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect()
    }

    const BASE: &str = r#"
[agent]
name = "a"
system_prompt = "s"

[agent.llm]
provider = "openai"
api_key = "k"
model = "m"
"#;

    #[test]
    fn schema_file_up_to_date() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/aura-config.schema.json");
        let rendered = format!(
            "{}\n",
            serde_json::to_string_pretty(&config_schema()).expect("schema renders")
        );
        if std::env::var_os("AURA_UPDATE_SCHEMA").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}; regenerate with `AURA_UPDATE_SCHEMA=1 cargo test -p aura-config schema_file_up_to_date`",
                path.display()
            )
        });
        assert_eq!(
            on_disk, rendered,
            "schema/aura-config.schema.json is stale; regenerate with `AURA_UPDATE_SCHEMA=1 cargo test -p aura-config schema_file_up_to_date`"
        );
    }

    #[test]
    fn shipped_configs_validate_raw() {
        let validator = validator();
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let dirs = [
            repo_root.join("configs"),
            repo_root.join("examples/minimal"),
            repo_root.join("examples/complete"),
        ];
        let single_files = [
            repo_root.join("quickstart.toml"),
            repo_root.join("examples/reference.toml"),
            repo_root.join("crates/aura-web-server/tests/test-config.toml"),
        ];

        let mut paths = Vec::new();
        for dir in &dirs {
            for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{dir:?}: {e}")) {
                let path = entry.unwrap().path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    paths.push(path);
                }
            }
        }
        paths.extend(single_files);

        let mut failures = Vec::new();
        for path in &paths {
            let contents = std::fs::read_to_string(path).unwrap();
            for error in validation_errors(&validator, &contents) {
                failures.push(format!("{}: {error}", path.display()));
            }
        }
        assert!(
            failures.is_empty(),
            "Some shipped configs failed schema validation:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn accepts_valid_shapes() {
        let validator = validator();
        let cases: [(&str, String); 7] = [
            (
                "hitl webhook route",
                format!(
                    "{BASE}\n[hitl]\nrequire_approval = [\"dangerous_*\"]\n\n[hitl.route]\nmode = \"webhook\"\nurl = \"https://example.com/hook\"\n\n[hitl.route.headers]\nx-api-key = \"k\"\n"
                ),
            ),
            (
                "hitl conversational route",
                format!("{BASE}\n[hitl.route]\nmode = \"conversational\"\ntimeout_secs = 30\n"),
            ),
            (
                "vector store with embedding model",
                format!(
                    "{BASE}\n[[vector_stores]]\nname = \"docs\"\ntype = \"in_memory\"\n\n[vector_stores.embedding_model]\nprovider = \"openai\"\napi_key = \"k\"\nmodel = \"text-embedding-3-small\"\n"
                ),
            ),
            (
                "flat legacy orchestration fields",
                format!(
                    "{BASE}\n[orchestration]\nenabled = true\nmemory_path = \"/tmp/x\"\nresult_artifact_threshold = 100\n"
                ),
            ),
            (
                "artifacts sub-table memory_path alias",
                format!("{BASE}\n[orchestration.artifacts]\nmemory_path = \"/tmp/x\"\n"),
            ),
            (
                "whole-float int (Helm toToml form)",
                BASE.replace("model = \"m\"", "model = \"m\"\nmax_tokens = 8000.0"),
            ),
            (
                "scratchpad tool entry map",
                format!(
                    "{BASE}\n[mcp.servers.calc]\ntransport = \"stdio\"\ncmd = [\"calc\"]\n\n[mcp.servers.calc.scratchpad.\"tool_*\"]\nmin_tokens = 100\n"
                ),
            ),
        ];
        for (name, contents) in &cases {
            let errors = validation_errors(&validator, contents);
            assert!(errors.is_empty(), "{name} should validate: {errors:?}");
        }
    }

    #[test]
    fn rejects_invalid_shapes() {
        let validator = validator();
        let cases: [(&str, String); 7] = [
            (
                "typo in vector store entry",
                format!(
                    "{BASE}\n[[vector_stores]]\nname = \"docs\"\ntype = \"qdrant\"\nurl = \"http://q\"\ncollection_name = \"c\"\nurls = \"typo\"\n\n[vector_stores.embedding_model]\nprovider = \"openai\"\napi_key = \"k\"\nmodel = \"m\"\n"
                ),
            ),
            (
                "unknown top-level table",
                format!("{BASE}\n[extra_table]\nx = 1\n"),
            ),
            (
                "typo in [orchestration]",
                format!("{BASE}\n[orchestration]\nmax_planing_cycles = 3\n"),
            ),
            (
                "unknown provider",
                BASE.replace("provider = \"openai\"", "provider = \"openia\""),
            ),
            (
                "string max_tokens",
                BASE.replace("model = \"m\"", "model = \"m\"\nmax_tokens = \"8000\""),
            ),
            (
                "legacy top-level [llm]",
                "[llm]\nprovider = \"openai\"\napi_key = \"k\"\nmodel = \"m\"\n\n[agent]\nname = \"a\"\nsystem_prompt = \"s\"\n".to_owned(),
            ),
            (
                "typo in worker table",
                format!(
                    "{BASE}\n[orchestration.worker.w]\ndescription = \"d\"\npreamble = \"p\"\nmcp_fitler = []\n"
                ),
            ),
        ];
        for (name, contents) in &cases {
            assert!(
                !validator.is_valid(&toml_to_json(contents)),
                "{name} should fail schema validation"
            );
        }
    }

    #[test]
    fn env_template_forms_match_the_resolver() {
        let _env_lock = crate::test_env_lock::lock();
        unsafe { std::env::set_var("SCHEMA_TEST_PROVIDER", "openai") };
        let validator = validator();
        let forms = [
            "{{ env.SCHEMA_TEST_PROVIDER }}",
            "{{env.SCHEMA_TEST_PROVIDER}}",
            "{{ env.SCHEMA_TEST_PROVIDER | default: 'openai' }}",
        ];
        for form in forms {
            let contents = BASE.replace("provider = \"openai\"", &format!("provider = \"{form}\""));
            let resolved = crate::resolve_env_vars(&contents).expect("resolver accepts the form");
            assert!(
                !resolved.contains("env."),
                "resolver substitutes {form}: {resolved}"
            );
            let errors = validation_errors(&validator, &contents);
            assert!(
                errors.is_empty(),
                "schema accepts resolver-accepted form {form}: {errors:?}"
            );
        }
    }

    #[test]
    fn accepts_env_template_in_constrained_strings() {
        let validator = validator();
        let templated = BASE
            .replace(
                "provider = \"openai\"",
                "provider = \"{{ env.LLM_PROVIDER }}\"",
            )
            .replace("api_key = \"k\"", "api_key = \"{{ env.LLM_API_KEY }}\"");
        let errors = validation_errors(&validator, &templated);
        assert!(
            errors.is_empty(),
            "templated discriminant should validate: {errors:?}"
        );
    }

    #[test]
    fn lenient_bool_accepts_string_form() {
        let validator = validator();
        let contents = BASE.replace("name = \"a\"", "name = \"a\"\nhidden = \"true\"");
        let errors = validation_errors(&validator, &contents);
        assert!(
            errors.is_empty(),
            "hidden = \"true\" should validate: {errors:?}"
        );
        let bad = BASE.replace("name = \"a\"", "name = \"a\"\nhidden = \"yes\"");
        assert!(
            !validator.is_valid(&toml_to_json(&bad)),
            "hidden = \"yes\" should fail schema validation"
        );
    }
}
