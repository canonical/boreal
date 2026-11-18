use std::fmt::Write as _;

use schemars::{JsonSchema, Schema};
use serde_json::Value;

use crate::valid_name;

pub struct WorkerSchema {
    name: &'static str,
    workflow: &'static str,
    description: &'static str,
    schema: fn() -> Schema,
}

impl WorkerSchema {
    pub fn new<T: JsonSchema>(
        name: &'static str,
        workflow: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            workflow,
            description,
            schema: || schemars::schema_for!(T),
        }
    }
}

pub fn emit_schema(workers: &[WorkerSchema]) -> Result<(), String> {
    print!("{}", render_schema(workers)?);
    Ok(())
}

pub fn render_schema(workers: &[WorkerSchema]) -> Result<String, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = String::new();
    for worker in workers {
        if !valid_name(worker.name) {
            return Err(format!(
                "schema key '{}' must match ^[a-z][a-z0-9_-]*$",
                worker.name
            ));
        }
        if !seen.insert(worker.name) {
            return Err(format!("duplicate schema key '{}'", worker.name));
        }
        if worker.workflow.trim().is_empty()
            || worker
                .workflow
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(format!(
                "worker '{}' requires a non-empty workflow name",
                worker.name
            ));
        }
        let args = project_args(worker.name, (worker.schema)())?;
        write_worker(&mut out, worker, &args);
    }
    if out.is_empty() {
        out.push_str("{}\n");
    }
    Ok(out)
}

struct Arg {
    name: String,
    type_name: &'static str,
    description: String,
    choices: Vec<String>,
    default: Option<Value>,
    required: bool,
}

fn project_args(worker: &str, schema: Schema) -> Result<Vec<Arg>, String> {
    let root = resolve_ref(worker, &schema, schema.as_value())?;
    let obj = root
        .as_object()
        .ok_or_else(|| format!("worker '{worker}' input schema must be an object"))?;
    if obj.contains_key("oneOf") || obj.contains_key("anyOf") || obj.contains_key("allOf") {
        return Err(format!(
            "worker '{worker}' input must be a flat object, not a union"
        ));
    }
    reject_additional(worker, obj.get("additionalProperties"))?;
    let required = match obj.get("required") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => {
            return Err(format!("worker '{worker}' required must be a list"));
        }
    };
    let properties = obj
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("worker '{worker}' input schema is missing properties"))?;
    if properties.is_empty() && !required.is_empty() {
        return Err(format!(
            "worker '{worker}' declares required fields without properties"
        ));
    }
    let mut args = Vec::with_capacity(properties.len());
    let mut seen_optional = false;
    for (name, prop) in properties {
        if !valid_arg_name(name) {
            return Err(format!(
                "argument '{name}' on '{worker}' must match ^[a-z][a-z0-9_]*$"
            ));
        }
        let resolved = resolve_ref(worker, &schema, prop)?;
        let arg = project_arg(worker, name, prop, resolved)?;
        let is_required = required.iter().any(|item| item == name) && arg.default.is_none();
        if is_required && seen_optional {
            return Err(format!(
                "required argument '{name}' follows an optional argument on '{worker}'"
            ));
        }
        if !is_required && arg.default.is_none() {
            return Err(format!(
                "optional argument '{name}' on '{worker}' needs a default"
            ));
        }
        if !is_required {
            seen_optional = true;
        }
        args.push(Arg {
            required: is_required,
            ..arg
        });
    }
    Ok(args)
}

fn project_arg(worker: &str, name: &str, raw: &Value, resolved: &Value) -> Result<Arg, String> {
    let obj = resolved
        .as_object()
        .ok_or_else(|| format!("argument '{name}' on '{worker}' must be a schema object"))?;
    let raw_obj = raw.as_object();
    if obj.contains_key("oneOf") || obj.contains_key("anyOf") || obj.contains_key("allOf") {
        return Err(format!(
            "argument '{name}' on '{worker}' must be a scalar, not a union"
        ));
    }
    reject_additional(worker, obj.get("additionalProperties"))?;
    if obj.contains_key("properties") || obj.get("type").and_then(Value::as_str) == Some("object") {
        return Err(format!(
            "argument '{name}' on '{worker}' must be a scalar, not an object"
        ));
    }
    if obj.get("type").and_then(Value::as_str) == Some("array") || obj.contains_key("items") {
        return Err(format!(
            "argument '{name}' on '{worker}' must be a scalar, not an array"
        ));
    }
    let description = obj
        .get("description")
        .or_else(|| raw_obj.and_then(|raw| raw.get("description")))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let default = obj
        .get("default")
        .or_else(|| raw_obj.and_then(|raw| raw.get("default")))
        .cloned();
    if let Some(Value::Array(choices)) = obj.get("enum") {
        let mut values = Vec::with_capacity(choices.len());
        for choice in choices {
            let Some(text) = choice.as_str().filter(|s| !s.trim().is_empty()) else {
                return Err(format!(
                    "argument '{name}' on '{worker}' enum choices must be non-empty strings"
                ));
            };
            if values.iter().any(|existing| existing == text) {
                return Err(format!(
                    "argument '{name}' on '{worker}' has duplicate enum choice '{text}'"
                ));
            }
            values.push(text.to_owned());
        }
        if values.is_empty() {
            return Err(format!(
                "argument '{name}' on '{worker}' enum requires choices"
            ));
        }
        return Ok(Arg {
            name: name.to_owned(),
            type_name: "enum",
            description,
            choices: values,
            default,
            required: false,
        });
    }
    let type_name = match obj.get("type").and_then(Value::as_str) {
        Some("string") => "string",
        Some("integer") => "int",
        Some("number") => "float",
        Some("boolean") => "bool",
        Some(other) => {
            return Err(format!(
                "argument '{name}' on '{worker}' has unsupported type '{other}'"
            ));
        }
        None => {
            return Err(format!(
                "argument '{name}' on '{worker}' is missing a scalar type"
            ));
        }
    };
    Ok(Arg {
        name: name.to_owned(),
        type_name,
        description,
        choices: Vec::new(),
        default,
        required: false,
    })
}

fn resolve_ref<'a>(worker: &str, root: &'a Schema, value: &'a Value) -> Result<&'a Value, String> {
    let Some(obj) = value.as_object() else {
        return Ok(value);
    };
    let Some(reference) = obj.get("$ref").and_then(Value::as_str) else {
        return Ok(value);
    };
    if !reference.starts_with("#/") {
        return Err(format!(
            "worker '{worker}' has an unsupported $ref '{reference}'"
        ));
    }
    root.pointer(reference)
        .ok_or_else(|| format!("worker '{worker}' $ref '{reference}' does not resolve"))
}

fn reject_additional(worker: &str, value: Option<&Value>) -> Result<(), String> {
    match value {
        None | Some(Value::Bool(false)) | Some(Value::Null) => Ok(()),
        _ => Err(format!(
            "worker '{worker}' must not allow additional properties"
        )),
    }
}

fn write_worker(out: &mut String, worker: &WorkerSchema, args: &[Arg]) {
    let _ = writeln!(out, "{}:", worker.name);
    let _ = writeln!(out, "  workflow: {}", worker.workflow);
    if !worker.description.is_empty() {
        let _ = writeln!(out, "  description: {}", quote(&worker.description));
    }
    if args.is_empty() {
        let _ = writeln!(out, "  args: []");
        return;
    }
    let _ = writeln!(out, "  args:");
    for arg in args {
        let _ = writeln!(out, "    - name: {}", arg.name);
        let _ = writeln!(out, "      type: {}", arg.type_name);
        if !arg.description.is_empty() {
            let _ = writeln!(out, "      description: {}", quote(&arg.description));
        }
        if arg.type_name == "enum" {
            let _ = writeln!(out, "      choices:");
            for choice in &arg.choices {
                let _ = writeln!(out, "        - {}", quote(choice));
            }
        }
        if !arg.required
            && let Some(default) = &arg.default
        {
            let _ = writeln!(out, "      default: {}", yaml_scalar(default));
        }
    }
}

fn yaml_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => quote(text),
        Value::Bool(flag) => {
            if *flag {
                "true".into()
            } else {
                "false".into()
            }
        }
        Value::Number(number) => number.to_string(),
        other => quote(&other.to_string()),
    }
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn valid_arg_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use validator::Validate;

    fn default_wait() -> u64 {
        20
    }

    #[derive(Serialize, Deserialize, JsonSchema, Validate)]
    struct DemoInput {
        /// Recipient name
        name: String,
        /// Timer delay in seconds
        #[serde(default = "default_wait")]
        #[validate(range(min = 1, max = 7200))]
        wait_seconds: u64,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    enum Mode {
        Fast,
        Accurate,
    }

    fn default_mode() -> Mode {
        Mode::Fast
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct TrainInput {
        dataset: String,
        #[serde(default = "default_mode")]
        mode: Mode,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct Nested {
        inner: DemoInput,
    }

    #[test]
    fn test_render_schema_projects_flat_args_and_ignores_validator_bounds() {
        let yaml = render_schema(&[
            WorkerSchema::new::<DemoInput>("greet", "DemoWorkflow", "Run the greeting workflow"),
            WorkerSchema::new::<TrainInput>("count", "CountWorkflow", ""),
        ])
        .unwrap();
        assert!(
            yaml.contains("greet:\n  workflow: DemoWorkflow\n"),
            "{yaml}"
        );
        assert!(
            yaml.contains("description: \"Run the greeting workflow\""),
            "{yaml}"
        );
        assert!(yaml.contains("- name: name\n      type: string"), "{yaml}");
        assert!(
            yaml.contains("- name: wait_seconds\n      type: int"),
            "{yaml}"
        );
        assert!(yaml.contains("default: 20"), "{yaml}");
        assert!(!yaml.contains("min:"), "{yaml}");
        assert!(
            yaml.contains("count:\n  workflow: CountWorkflow\n"),
            "{yaml}"
        );
        assert!(yaml.contains("type: enum"), "{yaml}");
        let name_at = yaml.find("- name: name").unwrap();
        let wait_at = yaml.find("- name: wait_seconds").unwrap();
        assert!(name_at < wait_at, "{yaml}");
    }

    #[test]
    fn test_render_schema_rejects_nested_objects() {
        let err =
            render_schema(&[WorkerSchema::new::<Nested>("bad", "BadWorkflow", "")]).unwrap_err();
        assert!(err.contains("scalar"), "{err}");
    }
}
