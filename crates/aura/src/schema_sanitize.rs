use std::collections::HashSet;

use serde_json::Value;
use tracing::warn;

/// Inlines all `$ref` pointers by replacing them with the referenced `$defs` definitions,
/// then strips `$defs` and `$schema` from the root.
///
/// Only handles local refs of the form `#/$defs/Name`. External or unresolvable refs
/// are left in place. Circular reference chains are detected and left unresolved.
///
/// When a `$ref` has sibling properties (e.g. `description`), the sibling takes precedence
/// over the same key in the inlined definition (JSON Schema 2020-12 semantics).
pub fn inline_refs(schema: &mut Value) -> &mut Value {
    let defs = match extract_defs(schema) {
        Some(d) => d,
        None => {
            strip_schema_noise(schema);
            return schema;
        }
    };

    let mut visited = HashSet::new();
    resolve_refs_recursive(schema, &defs, &mut visited);

    strip_schema_noise(schema);
    schema
}

fn extract_defs(schema: &Value) -> Option<serde_json::Map<String, Value>> {
    schema
        .as_object()
        .and_then(|map| map.get("$defs"))
        .and_then(|v| v.as_object())
        .cloned()
}

fn resolve_refs_recursive(
    node: &mut Value,
    defs: &serde_json::Map<String, Value>,
    visited: &mut HashSet<String>,
) {
    let map = match node.as_object_mut() {
        Some(m) => m,
        None => return,
    };

    if let Some(ref_val) = map.get("$ref").and_then(|v| v.as_str().map(String::from)) {
        let name = match ref_val.strip_prefix("#/$defs/") {
            Some(n) => n.to_string(),
            None => return, // external ref — leave in place
        };

        if visited.contains(&name) {
            warn!("Circular $ref detected for '{name}', leaving unresolved");
            return;
        }

        let definition = match defs.get(&name) {
            Some(d) => d.clone(),
            None => return, // missing def — leave in place
        };

        // Resolve nested refs within the definition itself
        let mut resolved_def = definition;
        visited.insert(name.clone());
        resolve_refs_recursive(&mut resolved_def, defs, visited);
        visited.remove(&name);

        // Collect sibling keys (everything except $ref) — these win on conflict
        let siblings: serde_json::Map<String, Value> = map
            .iter()
            .filter(|(k, _)| k.as_str() != "$ref")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Start with the inlined definition, overlay siblings
        if let Some(def_map) = resolved_def.as_object_mut() {
            for (k, v) in &siblings {
                def_map.insert(k.clone(), v.clone());
            }
            *map = def_map.clone();
        }

        return;
    }

    // No $ref — recurse into sub-schemas
    let keys_with_arrays: Vec<String> = ["anyOf", "oneOf", "allOf"]
        .iter()
        .filter(|k| map.contains_key(**k))
        .map(|k| k.to_string())
        .collect();

    for key in keys_with_arrays {
        if let Some(Value::Array(arr)) = map.get_mut(&key) {
            for item in arr.iter_mut() {
                resolve_refs_recursive(item, defs, visited);
            }
        }
    }

    let prop_keys: Vec<String> = map
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();

    for key in prop_keys {
        if let Some(Value::Object(properties)) = map.get_mut("properties")
            && let Some(prop_schema) = properties.get_mut(&key)
        {
            resolve_refs_recursive(prop_schema, defs, visited);
        }
    }

    if let Some(items) = map.get_mut("items") {
        resolve_refs_recursive(items, defs, visited);
    }
}

fn strip_schema_noise(schema: &mut Value) {
    if let Some(map) = schema.as_object_mut() {
        map.remove("$defs");
        map.remove("$schema");
    }
}

/// Recursively sets `additionalProperties: false` on all object schemas in a JSON Schema.
///
/// This function is designed to make JSON schemas compatible with OpenAI's strict mode,
/// which requires all object schemas to have `additionalProperties` set to `false`.
///
/// The function processes schemas that meet any of these conditions:
/// - Have a `required` field (even if empty)
/// - Have an empty `properties` object
/// - Already have an `additionalProperties` field (will override to `false`)
///
/// # Recursive Processing
///
/// The function recursively processes:
/// - `anyOf` arrays - each alternative schema
/// - `properties` - each property schema
/// - `items` - array item schemas
///
/// Non-object types (strings, integers, null, etc.) are left unchanged.
///
/// # Arguments
///
/// * `schema` - A mutable reference to a JSON schema value
///
/// # Returns
///
/// Returns a mutable reference to the modified schema for method chaining.
///
/// # Examples
///
/// Basic usage with a simple object:
///
/// ```ignore
/// use serde_json::json;
/// use crate::schema_sanitize::recursive_set_additional_properties_false;
///
/// let mut schema = json!({
///     "type": "object",
///     "properties": {
///         "name": {"type": "string"}
///     },
///     "required": ["name"]
/// });
///
/// recursive_set_additional_properties_false(&mut schema);
///
/// assert_eq!(schema["additionalProperties"], false);
/// ```
///
/// Handling anyOf with multiple object types:
///
/// ```ignore
/// use serde_json::json;
/// use crate::schema_sanitize::recursive_set_additional_properties_false;
///
/// let mut schema = json!({
///     "anyOf": [
///         {
///             "type": "object",
///             "properties": {"foo": {"type": "string"}},
///             "required": ["foo"]
///         },
///         {
///             "type": "object",
///             "properties": {"bar": {"type": "integer"}},
///             "required": ["bar"]
///         }
///     ]
/// });
///
/// recursive_set_additional_properties_false(&mut schema);
///
/// assert_eq!(schema["anyOf"][0]["additionalProperties"], false);
/// assert_eq!(schema["anyOf"][1]["additionalProperties"], false);
/// ```
///
/// Overriding existing `additionalProperties: true`:
///
/// ```ignore
/// use serde_json::json;
/// use crate::schema_sanitize::recursive_set_additional_properties_false;
///
/// let mut schema = json!({
///     "type": "object",
///     "additionalProperties": true
/// });
///
/// recursive_set_additional_properties_false(&mut schema);
///
/// assert_eq!(schema["additionalProperties"], false);
/// ```
pub fn recursive_set_additional_properties_false(schema: &mut Value) -> &mut Value {
    // Normalize const: null to type: null (MCP quirk)
    normalize_const_null(schema);

    // Only process dictionary/object values
    if let &mut Value::Object(ref mut map) = schema {
        // Handle edge case: type: "object" with no properties field
        // This is common in MCP schemas for zero-argument tools
        let is_object_without_properties = map
            .get("type")
            .and_then(|v| v.as_str())
            .map(|t| t == "object")
            .unwrap_or(false)
            && !map.contains_key("properties");

        if is_object_without_properties {
            // Add empty properties object for OpenAI compatibility
            map.insert(
                "properties".to_string(),
                Value::Object(serde_json::Map::new()),
            );
        }

        // Check if 'required' is a key at the current level or if the schema has empty
        // properties, in which case additionalProperties still needs to be specified.
        let should_add_additional_properties = map.contains_key("required")
            || (map
                .get("properties")
                .and_then(|v| v.as_object())
                .map(|obj| obj.is_empty())
                .unwrap_or(false))
            // Since Pydantic 2.11, it will always add `additionalProperties: true`
            // for arbitrary dictionary schemas.
            // If it is already set to true, we need to override it to false
            || map.contains_key("additionalProperties");

        if should_add_additional_properties {
            map.insert("additionalProperties".to_string(), Value::Bool(false));
        }

        // Recursively check combinators, properties, and items

        // Process anyOf - iterate through array of schemas
        if let Some(Value::Array(any_of_array)) = map.get_mut("anyOf") {
            for sub_schema in any_of_array.iter_mut() {
                recursive_set_additional_properties_false(sub_schema);
            }
        }

        // Process oneOf - iterate through array of schemas
        if let Some(Value::Array(one_of_array)) = map.get_mut("oneOf") {
            for sub_schema in one_of_array.iter_mut() {
                recursive_set_additional_properties_false(sub_schema);
            }
        }

        // Process allOf - iterate through array of schemas
        if let Some(Value::Array(all_of_array)) = map.get_mut("allOf") {
            for sub_schema in all_of_array.iter_mut() {
                recursive_set_additional_properties_false(sub_schema);
            }
        }

        // Process properties - iterate through object of property schemas
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            for sub_schema in properties.values_mut() {
                recursive_set_additional_properties_false(sub_schema);
            }
        }

        // Process items - single schema or array of schemas
        if let Some(items) = map.get_mut("items") {
            recursive_set_additional_properties_false(items);
        }
    }

    schema
}

/// Fixes incomplete `required` arrays at all nesting levels to comply with OpenAI's structured outputs.
///
/// OpenAI's structured outputs require that all properties must be in the `required` array at every level.
/// This function recursively transforms schemas by:
/// 1. Making all properties nullable (wrapping in `anyOf` with null if not already nullable)
/// 2. Adding all property names to the `required` array
///
/// This transformation is applied recursively to:
/// - Root level objects
/// - Nested objects in properties
/// - Objects in array items
/// - Objects in anyOf/allOf/oneOf branches
///
/// # Arguments
///
/// * `schema` - A mutable reference to a JSON schema value
///
/// # Returns
///
/// Returns a mutable reference to the modified schema for method chaining.
///
/// # Examples
///
/// Basic usage with empty required:
///
/// ```ignore
/// use serde_json::json;
/// use crate::schema_sanitize::fix_empty_root_required;
///
/// let mut schema = json!({
///     "type": "object",
///     "properties": {
///         "context": {
///             "type": "string",
///             "default": ""
///         }
///     },
///     "required": []
/// });
///
/// fix_empty_root_required(&mut schema);
///
/// // Now all properties are required and nullable
/// assert_eq!(schema["required"], json!(["context"]));
/// assert!(schema["properties"]["context"]["anyOf"].is_array());
/// ```
///
/// Already nullable fields are preserved:
///
/// ```ignore
/// use serde_json::json;
/// use crate::schema_sanitize::fix_empty_root_required;
///
/// let mut schema = json!({
///     "type": "object",
///     "properties": {
///         "field": {
///             "anyOf": [{"type": "string"}, {"type": "null"}]
///         }
///     },
///     "required": []
/// });
///
/// fix_empty_root_required(&mut schema);
///
/// assert_eq!(schema["required"], json!(["field"]));
/// // anyOf structure preserved
/// assert_eq!(schema["properties"]["field"]["anyOf"].as_array().unwrap().len(), 2);
/// ```
pub fn fix_empty_root_required(schema: &mut Value) -> &mut Value {
    fix_required_recursive(schema);
    schema
}

/// Recursively fixes missing required properties in all object schemas
fn fix_required_recursive(schema: &mut Value) {
    if let &mut Value::Object(ref mut map) = schema {
        // Get properties if they exist
        if let Some(Value::Object(properties)) = map.get("properties") {
            // Get all property names
            let all_property_names: std::collections::HashSet<String> =
                properties.keys().cloned().collect();

            // Get existing required array
            let existing_required: std::collections::HashSet<String> = map
                .get("required")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            // Find missing properties (properties not in required array)
            let missing_properties: Vec<String> = all_property_names
                .difference(&existing_required)
                .cloned()
                .collect();

            // Only process if there are missing properties
            if !missing_properties.is_empty() {
                // Get mutable access to properties
                if let Some(Value::Object(properties)) = map.get_mut("properties") {
                    // Make all missing properties nullable
                    for prop_name in &missing_properties {
                        if let Some(prop_schema) = properties.get_mut(prop_name)
                            && !is_nullable(prop_schema)
                        {
                            make_nullable(prop_schema);
                        }
                    }

                    // Build new required array with all properties
                    let mut new_required: Vec<Value> = existing_required
                        .iter()
                        .map(|s| Value::String(s.clone()))
                        .collect();

                    for prop_name in missing_properties {
                        new_required.push(Value::String(prop_name));
                    }

                    // Update required array
                    map.insert("required".to_string(), Value::Array(new_required));
                }
            }
        }

        // Recursively process nested schemas
        // Check in properties
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            for (_prop_name, prop_schema) in properties.iter_mut() {
                fix_required_recursive(prop_schema);
            }
        }

        // Check in items (for arrays)
        if let Some(items_schema) = map.get_mut("items") {
            fix_required_recursive(items_schema);
        }

        // Check in anyOf
        if let Some(Value::Array(any_of)) = map.get_mut("anyOf") {
            for item in any_of.iter_mut() {
                fix_required_recursive(item);
            }
        }

        // Check in allOf
        if let Some(Value::Array(all_of)) = map.get_mut("allOf") {
            for item in all_of.iter_mut() {
                fix_required_recursive(item);
            }
        }

        // Check in oneOf
        if let Some(Value::Array(one_of)) = map.get_mut("oneOf") {
            for item in one_of.iter_mut() {
                fix_required_recursive(item);
            }
        }
    }
}

/// Checks if a schema is already nullable (contains null type)
fn is_nullable(schema: &Value) -> bool {
    if let Value::Object(map) = schema {
        // Check anyOf for null type
        if let Some(Value::Array(any_of)) = map.get("anyOf") {
            return any_of.iter().any(|item| {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "null")
                    .unwrap_or(false)
            });
        }

        // Check if type is an array containing "null"
        if let Some(Value::Array(types)) = map.get("type") {
            return types
                .iter()
                .any(|t| t.as_str().map(|s| s == "null").unwrap_or(false));
        }
    }

    false
}

/// Normalizes `const: null` to `type: "null"` for OpenAI compatibility
fn normalize_const_null(schema: &mut Value) {
    if let Value::Object(map) = schema {
        // If schema has const: null, convert to type: "null"
        if let Some(const_val) = map.get("const")
            && const_val.is_null()
        {
            map.remove("const");
            map.remove("nullable"); // Also remove nullable flag
            map.insert("type".to_string(), Value::String("null".to_string()));
        }

        // Recursively process nested structures
        if let Some(Value::Array(any_of)) = map.get_mut("anyOf") {
            for item in any_of.iter_mut() {
                normalize_const_null(item);
            }
        }
        if let Some(Value::Array(all_of)) = map.get_mut("allOf") {
            for item in all_of.iter_mut() {
                normalize_const_null(item);
            }
        }
        if let Some(Value::Array(one_of)) = map.get_mut("oneOf") {
            for item in one_of.iter_mut() {
                normalize_const_null(item);
            }
        }
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            for prop in properties.values_mut() {
                normalize_const_null(prop);
            }
        }
        if let Some(items) = map.get_mut("items") {
            normalize_const_null(items);
        }
    }
}

/// Makes a schema nullable by wrapping it in anyOf with null
fn make_nullable(schema: &mut Value) {
    if let Value::Object(map) = schema {
        // Fields to preserve at top level (not moved into anyOf)
        let description = map.remove("description");
        let default = map.remove("default");
        let title = map.remove("title");

        // Check if anyOf already exists
        if let Some(&mut Value::Array(ref mut any_of)) = map.get_mut("anyOf") {
            // anyOf exists, add null if not present
            let has_null = any_of.iter().any(|item| {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "null")
                    .unwrap_or(false)
            });

            if !has_null {
                any_of.push(serde_json::json!({"type": "null"}));
            }
        } else {
            // Create new anyOf with current schema and null
            // Move ALL remaining fields into the first anyOf element
            let current_schema = Value::Object(map.clone());
            map.clear();

            let mut any_of_array = Vec::new();
            any_of_array.push(current_schema);
            any_of_array.push(serde_json::json!({"type": "null"}));

            map.insert("anyOf".to_string(), Value::Array(any_of_array));
        }

        // Restore metadata fields at top level
        if let Some(desc) = description {
            map.insert("description".to_string(), desc);
        }
        if let Some(def) = default {
            map.insert("default".to_string(), def);
        }
        if let Some(t) = title {
            map.insert("title".to_string(), t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_simple_object_with_required() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            },
            "required": ["name"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_empty_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {}
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_existing_additional_properties_override() {
        let mut schema = json!({
            "type": "object",
            "additionalProperties": true
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_anyof_with_multiple_objects() {
        let mut schema = json!({
            "properties": {
                "my_arg": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"foo": {"type": "string"}},
                            "required": ["foo"]
                        },
                        {
                            "type": "object",
                            "properties": {"bar": {"type": "integer"}},
                            "required": ["bar"]
                        }
                    ]
                }
            },
            "required": ["my_arg"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "properties": {
                "my_arg": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"foo": {"type": "string"}},
                            "required": ["foo"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {"bar": {"type": "integer"}},
                            "required": ["bar"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["my_arg"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_anyof_with_non_object_types() {
        let mut schema = json!({
            "anyOf": [
                {"type": "string"},
                {"type": "integer"},
                {
                    "type": "object",
                    "required": ["foo"]
                }
            ]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "anyOf": [
                {"type": "string"},
                {"type": "integer"},
                {
                    "type": "object",
                    "properties": {},
                    "required": ["foo"],
                    "additionalProperties": false
                }
            ]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_anyof_with_null_type() {
        let mut schema = json!({
            "properties": {
                "arg2": {
                    "anyOf": [
                        {
                            "type": "object",
                            "additionalProperties": true
                        },
                        {"type": "null"}
                    ]
                }
            }
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "properties": {
                "arg2": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        },
                        {"type": "null"}
                    ]
                }
            }
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_nested_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "arg1": {
                    "type": "object",
                    "properties": {
                        "nested_arg1": {"type": "integer"},
                        "nested_arg2": {"type": "string"}
                    },
                    "required": ["nested_arg1", "nested_arg2"]
                }
            },
            "required": ["arg1"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "arg1": {
                    "type": "object",
                    "properties": {
                        "nested_arg1": {"type": "integer"},
                        "nested_arg2": {"type": "string"}
                    },
                    "required": ["nested_arg1", "nested_arg2"],
                    "additionalProperties": false
                }
            },
            "required": ["arg1"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_items_in_array() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "items_list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["x"]
                    }
                }
            },
            "required": ["items_list"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "items_list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {},
                        "required": ["x"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items_list"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_deeply_nested_anyof() {
        let mut schema = json!({
            "properties": {
                "level1": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "level2": {
                                    "anyOf": [
                                        {
                                            "type": "object",
                                            "properties": {
                                                "level3": {"type": "string"}
                            },
                                            "required": ["level3"]
                                        }
                                    ]
                                }
                            },
                            "required": ["level2"]
                        }
                    ]
                }
            },
            "required": ["level1"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "properties": {
                "level1": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "level2": {
                                    "anyOf": [
                                        {
                                            "type": "object",
                                            "properties": {
                                                "level3": {"type": "string"}
                                            },
                                            "required": ["level3"],
                                            "additionalProperties": false
                                        }
                                    ]
                                }
                            },
                            "required": ["level2"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["level1"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_non_object_schema_unchanged() {
        let mut schema = json!({
            "type": "string"
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "string"
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_no_args_function() {
        // Matches the Python test: test_convert_to_openai_function_no_args
        let mut schema = json!({
            "type": "object",
            "properties": {}
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_complex_union_case() {
        // Based on test_convert_to_openai_function_strict_union_of_objects_arg_type
        let mut schema = json!({
            "properties": {
                "my_arg": {
                    "anyOf": [
                        {
                            "properties": {"foo": {"title": "Foo", "type": "string"}},
                            "required": ["foo"],
                            "title": "NestedA",
                            "type": "object"
                        },
                        {
                            "properties": {"bar": {"title": "Bar", "type": "integer"}},
                            "required": ["bar"],
                            "title": "NestedB",
                            "type": "object"
                        },
                        {
                            "properties": {"baz": {"title": "Baz", "type": "boolean"}},
                            "required": ["baz"],
                            "title": "NestedC",
                            "type": "object"
                        }
                    ]
                }
            },
            "required": ["my_arg"],
            "type": "object"
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "properties": {
                "my_arg": {
                    "anyOf": [
                        {
                            "properties": {"foo": {"title": "Foo", "type": "string"}},
                            "required": ["foo"],
                            "title": "NestedA",
                            "type": "object",
                            "additionalProperties": false
                        },
                        {
                            "properties": {"bar": {"title": "Bar", "type": "integer"}},
                            "required": ["bar"],
                            "title": "NestedB",
                            "type": "object",
                            "additionalProperties": false
                        },
                        {
                            "properties": {"baz": {"title": "Baz", "type": "boolean"}},
                            "required": ["baz"],
                            "title": "NestedC",
                            "type": "object",
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["my_arg"],
            "type": "object",
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_mixed_anyof_properties_items() {
        let mut schema = json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": {
                        "arr": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["x"]
                            }
                        }
                    },
                    "required": ["arr"]
                }
            ]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": {
                        "arr": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {},
                                "required": ["x"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["arr"],
                    "additionalProperties": false
                }
            ]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_real_world_mcp_schema_with_anyof_and_refs() {
        // Real schema from Mezmo MCP server - export_logs_relative_time tool
        // This tests: anyOf with null, $ref (should be dereferenced first), nullable fields
        let mut schema = json!({
            "type": "object",
            "properties": {
                "relative_time": {
                    "description": "How long since now, e.g. last 5 minutes",
                    "type": "string"
                },
                "pipeline_id": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "default": null,
                    "description": "Optional pipeline ID to filter logs"
                }
            },
            "required": ["relative_time"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "relative_time": {
                    "description": "How long since now, e.g. last 5 minutes",
                    "type": "string"
                },
                "pipeline_id": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "default": null,
                    "description": "Optional pipeline ID to filter logs"
                }
            },
            "required": ["relative_time"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_real_world_mcp_schema_empty_properties() {
        // Real schema from Mezmo MCP server - list_pipelines tool
        let mut schema = json!({
            "type": "object",
            "properties": {}
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_real_world_mcp_schema_with_additional_properties_true() {
        // Real schema with additionalProperties: true (Pydantic 2.11+ quirk)
        let mut schema = json!({
            "type": "object",
            "properties": {
                "metadata": {
                    "anyOf": [
                        {
                            "type": "object",
                            "additionalProperties": true
                        },
                        {"type": "null"}
                    ],
                    "default": null
                }
            },
            "required": ["metadata"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "metadata": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        },
                        {"type": "null"}
                    ],
                    "default": null
                }
            },
            "required": ["metadata"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_real_world_mcp_schema_complex_nested() {
        // Complex nested schema from ingest_log_entries
        let mut schema = json!({
            "type": "object",
            "properties": {
                "log_entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "message": {"type": "string"},
                            "level": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "null"}
                                ]
                            }
                        },
                        "required": ["message"]
                    }
                }
            },
            "required": ["log_entries"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "log_entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "message": {"type": "string"},
                            "level": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "null"}
                                ]
                            }
                        },
                        "required": ["message"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["log_entries"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    // Tests for fix_empty_root_required

    #[test]
    fn test_fix_empty_required_simple() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "string",
                    "default": ""
                }
            },
            "required": []
        });

        fix_empty_root_required(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "context": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "default": ""
                }
            },
            "required": ["context"]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_fix_empty_required_already_nullable() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "pipeline_id": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "default": null
                }
            },
            "required": []
        });

        fix_empty_root_required(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "pipeline_id": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "default": null
                }
            },
            "required": ["pipeline_id"]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_fix_empty_required_preserves_metadata() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "num": {
                    "title": "Num",
                    "description": "Number of accounts",
                    "type": "integer",
                    "default": 10
                }
            },
            "required": []
        });

        fix_empty_root_required(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "num": {
                    "title": "Num",
                    "description": "Number of accounts",
                    "anyOf": [
                        {"type": "integer"},
                        {"type": "null"}
                    ],
                    "default": 10
                }
            },
            "required": ["num"]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_fix_empty_required_logdna_kafka_retention() {
        // Real LogDNA Control schema with partial required array
        let mut schema = json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "string",
                    "description": "K8s Context to target",
                    "default": ""
                },
                "kafka": {
                    "type": "boolean",
                    "description": "Query kafka for information",
                    "default": false
                },
                "topic": {
                    "type": "string"
                }
            },
            "required": ["topic"]
        });

        fix_empty_root_required(&mut schema);

        // Verify transformations
        // 1. context and kafka should be nullable now
        assert!(schema["properties"]["context"].get("anyOf").is_some());
        assert!(schema["properties"]["kafka"].get("anyOf").is_some());
        // topic should remain as-is (already required)
        assert_eq!(schema["properties"]["topic"]["type"], "string");

        // 2. All properties should be in required array (order may vary)
        let required = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(required.len(), 3);
        assert!(required.contains("topic"));
        assert!(required.contains("context"));
        assert!(required.contains("kafka"));
    }

    #[test]
    fn test_fix_empty_required_logdna_pipeline_get() {
        // Real LogDNA Control schema with empty required
        let mut schema = json!({
            "type": "object",
            "properties": {
                "account": {
                    "title": "Account",
                    "description": "Log Analysis Account ID",
                    "default": "",
                    "type": "string"
                },
                "context": {
                    "title": "Context",
                    "description": "Kubernetes context to use",
                    "default": "",
                    "type": "string"
                },
                "pipeline_id": {
                    "title": "Pipeline Id",
                    "description": "The ID of the pipeline",
                    "default": "",
                    "type": "string"
                }
            },
            "required": []
        });

        fix_empty_root_required(&mut schema);

        // Note: Order in required array may vary due to HashMap iteration
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 3);
        assert!(required.contains(&json!("account")));
        assert!(required.contains(&json!("context")));
        assert!(required.contains(&json!("pipeline_id")));

        // Verify structure for each property
        assert_eq!(
            schema["properties"]["account"]["anyOf"],
            json!([{"type": "string"}, {"type": "null"}])
        );
        assert_eq!(schema["properties"]["account"]["title"], "Account");
        assert_eq!(
            schema["properties"]["account"]["description"],
            "Log Analysis Account ID"
        );
        assert_eq!(schema["properties"]["account"]["default"], "");

        assert_eq!(
            schema["properties"]["context"]["anyOf"],
            json!([{"type": "string"}, {"type": "null"}])
        );
        assert_eq!(schema["properties"]["context"]["title"], "Context");
        assert_eq!(schema["properties"]["context"]["default"], "");

        assert_eq!(
            schema["properties"]["pipeline_id"]["anyOf"],
            json!([{"type": "string"}, {"type": "null"}])
        );
        assert_eq!(schema["properties"]["pipeline_id"]["title"], "Pipeline Id");
        assert_eq!(schema["properties"]["pipeline_id"]["default"], "");
    }

    #[test]
    fn test_fix_empty_required_with_type_conflict() {
        // LogDNA pattern: anyOf with null AND separate type field
        let mut schema = json!({
            "type": "object",
            "properties": {
                "context": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "description": "Kubernetes Context",
                    "default": "",
                    "type": "string"
                }
            },
            "required": []
        });

        fix_empty_root_required(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "context": {
                    "anyOf": [
                        {"type": "string"},
                        {"type": "null"}
                    ],
                    "description": "Kubernetes Context",
                    "default": "",
                    "type": "string"
                }
            },
            "required": ["context"]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_combined_sanitization_mezmo() {
        // Full pipeline: fix_empty_root_required + recursive_set_additional_properties_false
        let mut schema = json!({
            "type": "object",
            "properties": {
                "pipeline_id": {
                    "type": "string",
                    "description": "Pipeline ID"
                },
                "log_entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "message": {"type": "string"}
                        },
                        "required": ["message"]
                    }
                }
            },
            "required": ["pipeline_id", "log_entries"]
        });

        recursive_set_additional_properties_false(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "pipeline_id": {
                    "type": "string",
                    "description": "Pipeline ID"
                },
                "log_entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "message": {"type": "string"}
                        },
                        "required": ["message"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["pipeline_id", "log_entries"],
            "additionalProperties": false
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn test_combined_sanitization_logdna() {
        // Full pipeline for LogDNA schema
        let mut schema = json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "string",
                    "default": ""
                },
                "kafka": {
                    "type": "boolean",
                    "default": false
                }
            },
            "required": []
        });

        fix_empty_root_required(&mut schema);
        recursive_set_additional_properties_false(&mut schema);

        // Note: Order in required array may vary due to HashMap iteration
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert!(required.contains(&json!("context")));
        assert!(required.contains(&json!("kafka")));

        // Verify structure
        assert_eq!(
            schema["properties"]["context"]["anyOf"],
            json!([{"type": "string"}, {"type": "null"}])
        );
        assert_eq!(schema["properties"]["context"]["default"], "");

        assert_eq!(
            schema["properties"]["kafka"]["anyOf"],
            json!([{"type": "boolean"}, {"type": "null"}])
        );
        assert_eq!(schema["properties"]["kafka"]["default"], false);

        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn test_object_type_without_properties_field() {
        // Edge case: MCP tools with zero arguments often have type: object
        // but no properties field at all (not even an empty object)
        // This is the format from older MCP schemas like list_pipelines
        let mut schema = json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "EmptyObject",
            "description": "This is commonly used for representing empty objects in MCP messages."
        });

        recursive_set_additional_properties_false(&mut schema);

        // Expected behavior for OpenAI: should have both properties and additionalProperties
        // Note: $schema is NOT stripped by recursive_set_additional_properties_false (that's inline_refs' job)
        let expected = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "EmptyObject",
            "description": "This is commonly used for representing empty objects in MCP messages."
        });
        assert_eq!(schema, expected);
    }

    #[test]
    fn test_oneof_traversal_in_additional_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "agg": {
                    "oneOf": [
                        {"type": "string", "enum": ["count"]},
                        {
                            "type": "object",
                            "properties": {"avg": {"type": "string"}},
                            "required": ["avg"]
                        }
                    ]
                }
            },
            "required": ["agg"]
        });

        recursive_set_additional_properties_false(&mut schema);

        // The object variant inside oneOf must get additionalProperties: false
        assert_eq!(
            schema["properties"]["agg"]["oneOf"][1]["additionalProperties"],
            false
        );
    }

    // ==================== inline_refs tests ====================

    #[test]
    fn test_inline_refs_basic() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "mode": {"$ref": "#/$defs/Mode"}
            },
            "required": ["mode"],
            "$defs": {
                "Mode": {"type": "string", "enum": ["fast", "slow"]}
            }
        });

        inline_refs(&mut schema);

        let expected = json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["fast", "slow"]}
            },
            "required": ["mode"]
        });
        assert_eq!(schema, expected);
    }

    #[test]
    fn test_inline_refs_with_sibling_description() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "agg": {
                    "$ref": "#/$defs/Agg",
                    "description": "Local description wins"
                }
            },
            "required": ["agg"],
            "$defs": {
                "Agg": {
                    "oneOf": [
                        {"type": "string", "enum": ["count"]},
                        {"type": "object", "properties": {"avg": {"type": "string"}}, "required": ["avg"]}
                    ],
                    "description": "Definition description loses"
                }
            }
        });

        inline_refs(&mut schema);

        assert_eq!(
            schema["properties"]["agg"]["description"],
            "Local description wins"
        );
        assert!(schema["properties"]["agg"]["oneOf"].is_array());
        assert!(schema.get("$defs").is_none());
    }

    #[test]
    fn test_inline_refs_sibling_no_conflict() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "field": {
                    "$ref": "#/$defs/MyType",
                    "description": "Added at ref site"
                }
            },
            "required": ["field"],
            "$defs": {
                "MyType": {"type": "string", "enum": ["a", "b"]}
            }
        });

        inline_refs(&mut schema);

        assert_eq!(schema["properties"]["field"]["description"], "Added at ref site");
        assert_eq!(schema["properties"]["field"]["type"], "string");
        assert_eq!(schema["properties"]["field"]["enum"], json!(["a", "b"]));
    }

    #[test]
    fn test_inline_refs_nested() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "outer": {"$ref": "#/$defs/Outer"}
            },
            "required": ["outer"],
            "$defs": {
                "Outer": {
                    "type": "object",
                    "properties": {
                        "inner": {"$ref": "#/$defs/Inner"}
                    },
                    "required": ["inner"]
                },
                "Inner": {"type": "string", "enum": ["x", "y"]}
            }
        });

        inline_refs(&mut schema);

        assert_eq!(schema["properties"]["outer"]["properties"]["inner"]["type"], "string");
        assert_eq!(
            schema["properties"]["outer"]["properties"]["inner"]["enum"],
            json!(["x", "y"])
        );
        assert!(schema.get("$defs").is_none());
    }

    #[test]
    fn test_inline_refs_circular() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "node": {"$ref": "#/$defs/A"}
            },
            "required": ["node"],
            "$defs": {
                "A": {
                    "type": "object",
                    "properties": {"child": {"$ref": "#/$defs/B"}},
                    "required": ["child"]
                },
                "B": {
                    "type": "object",
                    "properties": {"back": {"$ref": "#/$defs/A"}},
                    "required": ["back"]
                }
            }
        });

        // Should not panic
        inline_refs(&mut schema);

        // A is resolved, B is resolved, but the circular back-ref in B stays as $ref
        assert!(schema["properties"]["node"]["properties"]["child"]["properties"]["back"]
            .get("$ref")
            .is_some());
    }

    #[test]
    fn test_inline_refs_no_defs() {
        let mut schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        });
        let original = schema.clone();

        inline_refs(&mut schema);

        assert_eq!(schema, original);
    }

    #[test]
    fn test_inline_refs_missing_def() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "field": {"$ref": "#/$defs/DoesNotExist"}
            },
            "required": ["field"],
            "$defs": {}
        });

        inline_refs(&mut schema);

        // $ref left in place when definition is missing
        assert_eq!(
            schema["properties"]["field"]["$ref"],
            "#/$defs/DoesNotExist"
        );
    }

    #[test]
    fn test_inline_refs_external_ref() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "field": {"$ref": "https://example.com/schema.json#/Foo"}
            },
            "required": ["field"],
            "$defs": {}
        });

        inline_refs(&mut schema);

        // External ref left in place
        assert_eq!(
            schema["properties"]["field"]["$ref"],
            "https://example.com/schema.json#/Foo"
        );
    }

    #[test]
    fn test_inline_refs_strips_schema_annotation() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "required": ["x"]
        });

        inline_refs(&mut schema);

        assert!(schema.get("$schema").is_none());
    }

    #[test]
    fn test_inline_refs_multiple_refs_to_same_def() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "a": {"$ref": "#/$defs/Metric", "description": "First use"},
                "b": {"$ref": "#/$defs/Metric", "description": "Second use"}
            },
            "required": ["a", "b"],
            "$defs": {
                "Metric": {"type": "string", "enum": ["p50", "p95", "p99"]}
            }
        });

        inline_refs(&mut schema);

        assert_eq!(schema["properties"]["a"]["description"], "First use");
        assert_eq!(schema["properties"]["b"]["description"], "Second use");
        assert_eq!(schema["properties"]["a"]["enum"], json!(["p50", "p95", "p99"]));
        assert_eq!(schema["properties"]["b"]["enum"], json!(["p50", "p95", "p99"]));
    }

    #[test]
    fn test_inline_refs_inside_items() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "items": {"$ref": "#/$defs/Entry"}
                }
            },
            "required": ["entries"],
            "$defs": {
                "Entry": {
                    "type": "object",
                    "properties": {"msg": {"type": "string"}},
                    "required": ["msg"]
                }
            }
        });

        inline_refs(&mut schema);

        assert_eq!(schema["properties"]["entries"]["items"]["type"], "object");
        assert_eq!(
            schema["properties"]["entries"]["items"]["properties"]["msg"]["type"],
            "string"
        );
        assert!(schema.get("$defs").is_none());
    }

    #[test]
    fn test_inline_refs_full_pipeline_group_logs() {
        // Real-world group_logs_by_field aggregation schema through all 3 passes
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "GroupLogsByFieldArgs",
            "type": "object",
            "properties": {
                "field": {"type": "string", "description": "Field to group by"},
                "aggregation": {
                    "$ref": "#/$defs/Aggregation",
                    "description": "Aggregation metric. Examples:\n- \"count\" for counting"
                },
                "query": {"type": ["string", "null"], "default": null}
            },
            "required": ["field", "aggregation"],
            "$defs": {
                "Aggregation": {
                    "oneOf": [
                        {"type": "string", "enum": ["count"]},
                        {
                            "type": "object",
                            "properties": {"avg": {"type": "string"}},
                            "required": ["avg"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {"p95": {"type": "string"}},
                            "required": ["p95"],
                            "additionalProperties": false
                        }
                    ]
                }
            }
        });

        // Run full pipeline
        inline_refs(&mut schema);
        fix_empty_root_required(&mut schema);
        recursive_set_additional_properties_false(&mut schema);

        // $ref and $defs gone
        assert!(schema.get("$defs").is_none());
        assert!(schema.get("$schema").is_none());
        assert!(schema["properties"]["aggregation"].get("$ref").is_none());

        // oneOf is inlined with description preserved
        let agg = &schema["properties"]["aggregation"];
        assert!(agg["oneOf"].is_array());
        assert_eq!(agg["oneOf"].as_array().unwrap().len(), 3);
        assert!(agg["description"].as_str().unwrap().contains("Aggregation metric"));

        // Object variants inside oneOf got additionalProperties: false
        assert_eq!(agg["oneOf"][1]["additionalProperties"], false);
        assert_eq!(agg["oneOf"][2]["additionalProperties"], false);

        // Root got additionalProperties: false
        assert_eq!(schema["additionalProperties"], false);

        // query (optional) was made nullable + required
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
    }
}
