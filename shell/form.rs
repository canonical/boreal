use std::io::{self, Write};

use boreal_common::catalog;
use boreal_common::input::{MAX_BYTES, Map, Schema, Value, parse};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Attributes, Print, SetAttribute, SetAttributes};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};

type FieldId = usize;

struct Form<'a> {
    schema: &'a Schema,
    // Stable field IDs retain drafts while Esc revisits a selector or its
    // payload. The current row order is projected from the selected branches.
    fields: Vec<Field>,
    root: FieldId,
}

struct Field {
    path: String,
    description: Option<String>,
    kind: Kind,
    answer: Option<Answer>,
    draft: Option<String>,
    selection: usize,
    tagged_payload: bool,
}

enum Kind {
    Id(String),
    Scalar {
        scalar: Scalar,
        format: Option<String>,
        default: Option<Value>,
        optional: bool,
        validator: Option<Box<Schema>>,
    },
    Object(Vec<(String, FieldId)>),
    Select(Vec<Choice>),
}

#[derive(Clone, Copy)]
enum Scalar {
    String,
    Integer,
    Number,
    Boolean,
}

struct Choice {
    label: String,
    description: Option<String>,
    ghost: bool,
    output: Output,
}

enum Output {
    Value(Option<Value>),
    Child(FieldId),
    Tagged(String, FieldId),
}

enum Answer {
    Value(Option<Value>),
    Choice(usize),
}

fn unsupported(path: &str, reason: &str) -> String {
    let path = if path.is_empty() { "--input" } else { path };
    format!(
        "interactive input at {}: {reason}; use --input-json instead",
        clean(path)
    )
}

fn description(schema: &Value) -> Option<&str> {
    schema
        .get("description")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
}

fn pointer(reference: &str) -> Option<String> {
    let mut bytes = reference.strip_prefix('#')?.bytes();
    let mut decoded = Vec::new();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(b);
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (decoded.is_empty() || decoded.starts_with('/')).then_some(decoded)
}

fn path_join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}.{name}")
    }
}

fn literal(label: String, value: Option<Value>, ghost: bool) -> Choice {
    Choice {
        label,
        description: None,
        ghost,
        output: Output::Value(value),
    }
}

impl<'a> Form<'a> {
    fn new(
        schema: &'a Schema,
        id: Option<String>,
        default_id: String,
        initial: &Value,
    ) -> Result<Self, String> {
        let mut form = Self {
            schema,
            fields: Vec::new(),
            root: 0,
        };
        form.add("--id", Some("Workflow ID"), Kind::Id(default_id))?;
        if let Some(id) = id {
            catalog::validate_workflow_id(&id).map_err(|e| e.detail())?;
            form.fields[0].draft = Some(id.clone());
            form.fields[0].answer = Some(Answer::Value(Some(Value::String(id))));
        }
        form.root = form.build(schema.value(), "", None, false, 0)?;
        if !initial.as_object().is_some_and(Map::is_empty) {
            form.seed(form.root, initial)?;
        }
        Ok(form)
    }

    fn add(
        &mut self,
        path: &str,
        description: Option<&str>,
        kind: Kind,
    ) -> Result<FieldId, String> {
        if self.fields.len() >= 1024 {
            return Err(unsupported(path, "schema has too many form fields"));
        }
        let id = self.fields.len();
        self.fields.push(Field {
            path: path.to_owned(),
            description: description.map(str::to_owned),
            kind,
            answer: None,
            draft: None,
            selection: 0,
            tagged_payload: false,
        });
        Ok(id)
    }

    // Follow local JSON pointers only. Schemars puts doc comments and defaults
    // beside a reference; other siblings may change its shape or constraints.
    fn resolve(
        &self,
        mut raw: &'a Value,
        path: &str,
    ) -> Result<(&'a Value, Option<&'a str>, Option<&'a Value>), String> {
        let mut help = description(raw);
        let mut default = raw.get("default");
        for _ in 0..48 {
            let obj = raw
                .as_object()
                .ok_or_else(|| unsupported(path, "expected a typed schema"))?;
            if obj.contains_key("$id") || obj.contains_key("$dynamicRef") {
                return Err(unsupported(
                    path,
                    "schema resource IDs and dynamic references are not supported",
                ));
            }
            let Some(reference) = obj.get("$ref") else {
                return Ok((raw, help, default));
            };
            if obj.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "$ref"
                        | "$schema"
                        | "$defs"
                        | "definitions"
                        | "description"
                        | "default"
                        | "title"
                        | "examples"
                        | "deprecated"
                        | "readOnly"
                        | "writeOnly"
                        | "$comment"
                )
            }) {
                return Err(unsupported(path, "a reference also has schema constraints"));
            }
            raw = reference
                .as_str()
                .and_then(pointer)
                .and_then(|p| self.schema.value().pointer(&p))
                .ok_or_else(|| unsupported(path, "expected a local JSON pointer reference"))?;
            help = help.or_else(|| description(raw));
            default = default.or_else(|| raw.get("default"));
        }
        Err(unsupported(
            path,
            "reference is recursive or too deeply nested",
        ))
    }

    fn build(
        &mut self,
        raw: &'a Value,
        path: &str,
        inherited: Option<&'a str>,
        optional: bool,
        depth: usize,
    ) -> Result<FieldId, String> {
        if depth >= 48 {
            return Err(unsupported(
                path,
                "schema is recursive or too deeply nested",
            ));
        }
        let (raw, help, default) = self.resolve(raw, path)?;
        let help = help.or(inherited);
        let obj = raw.as_object().expect("resolved object schema");
        let fail = |reason| unsupported(path, reason);
        if [
            "allOf",
            "not",
            "if",
            "dependentSchemas",
            "dependentRequired",
            "dependencies",
            "patternProperties",
            "unevaluatedProperties",
            "propertyNames",
            "minProperties",
            "maxProperties",
        ]
        .iter()
        .any(|key| obj.contains_key(*key))
        {
            return Err(fail(
                "schema composition or object constraints are not supported",
            ));
        }
        if let Some(any) = obj.get("anyOf") {
            // Schemars uses this for Option<T> when T is a reference or union.
            let any = any
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or_else(|| fail("only nullable anyOf is supported"))?;
            let null = |v: &Value| {
                v.as_object()
                    .is_some_and(|o| o.len() == 1 && v["type"] == "null")
            };
            let inner = match (null(&any[0]), null(&any[1])) {
                (true, false) => &any[1],
                (false, true) => &any[0],
                _ => return Err(fail("only nullable anyOf is supported")),
            };
            let child = self.build(inner, path, help, false, depth + 1)?;
            return self.gate(path, help, child, default, optional, true);
        }
        let (ty, nullable) = match obj.get("type") {
            Some(Value::String(s)) => (Some(s.as_str()), false),
            Some(Value::Array(types)) if types.len() == 2 && types.iter().any(|t| t == "null") => {
                let ty = types
                    .iter()
                    .find(|t| *t != "null")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail("expected one non-null type"))?;
                (Some(ty), true)
            }
            None => (None, false),
            _ => {
                return Err(fail(
                    "expected a scalar, a fixed object, or an externally tagged enum",
                ));
            }
        };

        if let Some(one) = obj.get("oneOf") {
            let mut choices = Vec::new();
            for variant in one
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(|| fail("expected nonempty enum variants"))?
            {
                let (variant, variant_help, _) = self.resolve(variant, path)?;
                if let Some(units) = string_choices(variant).filter(|_| variant["type"] == "string")
                {
                    for unit in units {
                        let mut choice =
                            literal(unit.to_owned(), Some(Value::String(unit.to_owned())), false);
                        choice.description = variant_help.map(str::to_owned);
                        choices.push(choice);
                    }
                } else {
                    let (name, payload) = tagged_property(variant).ok_or_else(|| {
                        fail("oneOf must contain string units or externally tagged variants")
                    })?;
                    let variant_path = path_join(path, name);
                    let child = self.build(
                        payload,
                        &variant_path,
                        variant_help.or(help),
                        false,
                        depth + 1,
                    )?;
                    self.fields[child].tagged_payload = true;
                    choices.push(Choice {
                        label: name.to_owned(),
                        description: variant_help.map(str::to_owned),
                        ghost: false,
                        output: Output::Tagged(name.to_owned(), child),
                    });
                }
            }
            if choices
                .iter()
                .enumerate()
                .any(|(i, c)| choices[..i].iter().any(|other| other.label == c.label))
            {
                return Err(fail("enum variants must have distinct labels"));
            }
            return self.select_field(path, help, choices, default, optional, nullable);
        }

        if let Some(strings) = string_choices(raw) {
            let choices = strings
                .into_iter()
                .map(|s| literal(s.to_owned(), Some(Value::String(s.to_owned())), false))
                .collect();
            return self.select_field(path, help, choices, default, optional, nullable);
        }
        if obj.contains_key("enum") {
            return Err(fail("only string enums are supported"));
        }

        if ty == Some("object") || (ty.is_none() && obj.contains_key("properties")) {
            if obj.get("additionalProperties").is_some_and(|v| v != false) {
                return Err(fail("maps and additional properties are not supported"));
            }
            if obj.contains_key("const") {
                return Err(fail("constant objects are not supported"));
            }
            let properties = obj.get("properties").and_then(Value::as_object);
            let required = obj
                .get("required")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if required.iter().any(|name| {
                !name
                    .as_str()
                    .is_some_and(|name| properties.is_some_and(|props| props.contains_key(name)))
            }) {
                return Err(fail("a required key is not a declared property"));
            }
            let mut fields = Vec::with_capacity(properties.map_or(0, Map::len));
            for (name, prop) in properties.into_iter().flatten() {
                let child = self.build(
                    prop,
                    &path_join(path, name),
                    help,
                    !required.iter().any(|r| r == name.as_str()),
                    depth + 1,
                )?;
                fields.push((name.clone(), child));
            }
            if fields.is_empty() && !path.is_empty() {
                return self.select_field(
                    path,
                    help,
                    vec![literal("{}".into(), Some(Value::Object(Map::new())), false)],
                    default,
                    optional,
                    nullable,
                );
            }
            let child = self.add(path, help, Kind::Object(fields))?;
            return if optional || nullable || default.is_some() {
                self.gate(path, help, child, default, optional, nullable)
            } else {
                Ok(child)
            };
        }

        let scalar = match ty {
            Some("string") => Scalar::String,
            Some("integer") => Scalar::Integer,
            Some("number") => Scalar::Number,
            Some("boolean") => Scalar::Boolean,
            Some("null") => {
                return self.select_field(
                    path,
                    help,
                    vec![literal("null".into(), Some(Value::Null), false)],
                    default,
                    optional,
                    false,
                );
            }
            _ => {
                return Err(fail(
                    "expected a scalar, a fixed object, or an externally tagged enum",
                ));
            }
        };
        // Scalar constraints can be checked without sibling values, so an
        // invalid Enter leaves the same editor and its typed contents intact.
        let validator = [
            "const",
            "minLength",
            "maxLength",
            "pattern",
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ]
        .iter()
        .any(|key| obj.contains_key(*key))
        .then(|| Schema::new(raw.clone()).map(Box::new))
        .transpose()
        .map_err(|_| fail("could not validate scalar constraints"))?;
        let child = self.add(
            path,
            help,
            Kind::Scalar {
                scalar,
                format: obj.get("format").and_then(Value::as_str).map(str::to_owned),
                default: if nullable { None } else { default.cloned() },
                optional: optional && !nullable,
                validator,
            },
        )?;
        if nullable {
            self.gate(path, help, child, default, optional, true)
        } else {
            Ok(child)
        }
    }

    fn gate(
        &mut self,
        path: &str,
        help: Option<&str>,
        child: FieldId,
        default: Option<&Value>,
        optional: bool,
        nullable: bool,
    ) -> Result<FieldId, String> {
        self.select_field(
            path,
            help,
            vec![Choice {
                label: "(edit)".into(),
                description: None,
                ghost: false,
                output: Output::Child(child),
            }],
            default,
            optional,
            nullable,
        )
    }

    fn select_field(
        &mut self,
        path: &str,
        help: Option<&str>,
        mut choices: Vec<Choice>,
        default: Option<&Value>,
        optional: bool,
        nullable: bool,
    ) -> Result<FieldId, String> {
        if nullable
            && !choices
                .iter()
                .any(|c| matches!(c.output, Output::Value(Some(Value::Null))))
        {
            choices.insert(0, literal("null".into(), Some(Value::Null), false));
        }
        if optional {
            choices.insert(0, literal("(omit)".into(), None, default.is_none()));
        }
        let mut selection = 0;
        if let Some(value) = default {
            if let Some(i) = choices
                .iter()
                .position(|c| matches!(&c.output, Output::Value(Some(v)) if v == value))
            {
                choices[i].ghost = true;
                selection = i;
            } else if let Some((i, child, payload)) =
                choices
                    .iter()
                    .enumerate()
                    .find_map(|(i, c)| match &c.output {
                        Output::Tagged(name, child) => value
                            .as_object()
                            .filter(|o| o.len() == 1)
                            .and_then(|o| o.get(name))
                            .map(|payload| (i, *child, payload)),
                        _ => None,
                    })
            {
                self.seed(child, payload)?;
                choices[i].ghost = true;
                selection = i;
            } else {
                choices.insert(0, literal(display_value(value), Some(value.clone()), true));
            }
        }
        let id = self.add(path, help, Kind::Select(choices))?;
        self.fields[id].selection = selection;
        Ok(id)
    }

    fn seed(&mut self, id: FieldId, value: &Value) -> Result<(), String> {
        let field = &self.fields[id];
        let fail = || unsupported(&field.path, "prefilled input does not match the form");
        match &field.kind {
            Kind::Object(properties) => {
                let obj = value.as_object().ok_or_else(fail)?;
                if obj
                    .keys()
                    .any(|key| !properties.iter().any(|(name, _)| name == key))
                {
                    return Err(fail());
                }
                let children: Vec<_> = properties
                    .iter()
                    .filter_map(|(name, child)| obj.get(name).map(|v| (*child, v)))
                    .collect();
                for (child, value) in children {
                    self.seed(child, value)?;
                }
            }
            Kind::Select(choices) => {
                let (index, child) = choices
                    .iter()
                    .enumerate()
                    .find_map(|(i, c)| match &c.output {
                        Output::Value(Some(v)) if v == value => Some((i, None)),
                        Output::Tagged(name, child) => value
                            .as_object()
                            .filter(|o| o.len() == 1)
                            .and_then(|obj| obj.get(name))
                            .map(|v| (i, Some((*child, v)))),
                        _ => None,
                    })
                    .or_else(|| {
                        choices
                            .iter()
                            .enumerate()
                            .find_map(|(i, c)| match c.output {
                                Output::Child(child) => Some((i, Some((child, value)))),
                                _ => None,
                            })
                    })
                    .ok_or_else(fail)?;
                if let Some((child, value)) = child {
                    self.seed(child, value)?;
                }
                self.fields[id].selection = index;
                self.fields[id].answer = Some(Answer::Choice(index));
            }
            Kind::Scalar { scalar, .. } => {
                let draft = match scalar {
                    Scalar::String => value.as_str().ok_or_else(fail)?.to_owned(),
                    _ => value.to_string(),
                };
                // Invalid prefills are editable, rather than silently replaced.
                let valid = field
                    .parse_draft(Some(&draft))
                    .is_ok_and(|v| v.as_ref() == Some(value));
                self.fields[id].draft = Some(draft);
                if valid {
                    self.fields[id].answer = Some(Answer::Value(Some(value.clone())));
                }
            }
            Kind::Id(_) => unreachable!(),
        }
        Ok(())
    }

    fn steps(&self) -> Vec<FieldId> {
        let mut ids = vec![0];
        self.walk(self.root, &mut ids);
        ids
    }

    fn walk(&self, id: FieldId, ids: &mut Vec<FieldId>) {
        let field = &self.fields[id];
        if let Kind::Object(children) = &field.kind {
            for (_, child) in children {
                self.walk(*child, ids);
            }
            return;
        }
        ids.push(id);
        if let Some(Output::Child(child) | Output::Tagged(_, child)) = field.output() {
            self.walk(*child, ids);
        }
    }

    fn value(&self, id: FieldId) -> Option<Value> {
        let field = &self.fields[id];
        match &field.kind {
            Kind::Object(children) => Some(Value::Object(
                children
                    .iter()
                    .filter_map(|(k, child)| self.value(*child).map(|v| (k.clone(), v)))
                    .collect(),
            )),
            Kind::Select(_) => match field.output()? {
                Output::Value(value) => value.clone(),
                Output::Child(child) => self.value(*child),
                Output::Tagged(name, child) => Some(Value::Object(Map::from_iter([(
                    name.clone(),
                    self.value(*child)?,
                )]))),
            },
            _ => match &field.answer {
                Some(Answer::Value(v)) => v.clone(),
                _ => None,
            },
        }
    }

    fn submission(&self) -> Result<(String, String), String> {
        let id = self
            .value(0)
            .and_then(|id| id.as_str().map(str::to_owned))
            .ok_or("workflow ID is missing")?;
        let value = self.value(self.root).ok_or("workflow input is missing")?;
        let encoded = value.to_string();
        if encoded.len() > MAX_BYTES {
            return Err(format!("workflow input exceeds {MAX_BYTES} bytes"));
        }
        self.schema.validate(&value)?;
        Ok((id, encoded))
    }
}

fn string_choices(raw: &Value) -> Option<Vec<&str>> {
    if let Some(choices) = raw.get("enum").and_then(Value::as_array) {
        let strings: Vec<_> = choices
            .iter()
            .filter(|v| !v.is_null())
            .map(Value::as_str)
            .collect::<Option<_>>()?;
        return (!strings.is_empty()
            && (!choices.iter().any(Value::is_null)
                || raw
                    .get("type")
                    .and_then(Value::as_array)
                    .is_some_and(|t| t.iter().any(|v| v == "null"))))
        .then_some(strings);
    }
    raw.get("const").and_then(Value::as_str).map(|s| vec![s])
}

fn tagged_property(variant: &Value) -> Option<(&str, &Value)> {
    let obj = variant.as_object()?;
    let props = obj.get("properties")?.as_object()?;
    let (name, child) = props.iter().next()?;
    let required = obj.get("required")?.as_array()?;
    (obj.get("type")?.as_str()? == "object"
        && props.len() == 1
        && required.len() == 1
        && required[0] == name.as_str()
        && obj.get("additionalProperties") == Some(&Value::Bool(false)))
    .then_some((name, child))
}

impl Field {
    fn parse_draft(&self, draft: Option<&str>) -> Result<Option<Value>, String> {
        match &self.kind {
            Kind::Id(default) => {
                let id = draft.unwrap_or(default);
                catalog::validate_workflow_id(id).map_err(|e| e.detail())?;
                Ok(Some(Value::String(id.to_owned())))
            }
            Kind::Scalar {
                scalar,
                format,
                default,
                optional,
                validator,
            } => {
                let value = match draft {
                    Some(raw) => scalar.parse(raw, format.as_deref())?,
                    None if default.is_some() => {
                        let default = default.as_ref().expect("default checked");
                        if matches!(scalar, Scalar::String) {
                            default.as_str().ok_or("expected a string default")?;
                            default.clone()
                        } else {
                            scalar.parse(&default.to_string(), format.as_deref())?
                        }
                    }
                    None if *optional => return Ok(None),
                    None => scalar.parse("", format.as_deref())?,
                };
                if let Some(validator) = validator {
                    validator.validate(&value)?;
                }
                Ok(Some(value))
            }
            _ => unreachable!(),
        }
    }

    fn accept(&mut self) -> Result<(), String> {
        self.answer = Some(match &self.kind {
            Kind::Select(_) => Answer::Choice(self.selection),
            _ => Answer::Value(self.parse_draft(self.draft.as_deref())?),
        });
        Ok(())
    }

    fn output(&self) -> Option<&Output> {
        match (&self.kind, &self.answer) {
            (Kind::Select(c), Some(Answer::Choice(i))) => Some(&c[*i].output),
            _ => None,
        }
    }

    fn title(&self) -> &str {
        if self.path.is_empty() {
            "value"
        } else {
            &self.path
        }
    }

    fn label(&self) -> String {
        let name = self.title();
        let Kind::Scalar { scalar, format, .. } = &self.kind else {
            return name.to_owned();
        };
        let format = match format.as_deref() {
            Some("uint8") => "u8",
            Some("uint16") => "u16",
            Some("uint32") => "u32",
            Some("uint64") => "u64",
            Some("uint128") => "u128",
            Some("uint") => "usize",
            Some("int8") => "i8",
            Some("int16") => "i16",
            Some("int32") => "i32",
            Some("int64") => "i64",
            Some("int128") => "i128",
            Some("int") => "isize",
            Some("float") => "f32",
            Some("double") => "f64",
            _ => match scalar {
                Scalar::String => "string",
                Scalar::Integer => "integer",
                Scalar::Number => "number",
                Scalar::Boolean => "bool",
            },
        };
        if self.tagged_payload {
            name.to_owned()
        } else {
            format!("{name} ({format})")
        }
    }

    fn confirmed_text(&self) -> Option<String> {
        match (&self.answer, self.output()) {
            (_, Some(Output::Child(_) | Output::Tagged(_, _))) => None,
            (_, Some(Output::Value(value))) | (Some(Answer::Value(value)), _) => Some(
                value
                    .as_ref()
                    .map(display_value)
                    .unwrap_or_else(|| "(omitted)".into()),
            ),
            _ => None,
        }
    }
}

impl Scalar {
    fn parse(self, raw: &str, format: Option<&str>) -> Result<Value, String> {
        if matches!(self, Self::String) {
            return Ok(Value::String(raw.to_owned()));
        }
        let raw = raw.trim();
        let message = match self {
            Self::Integer => "enter an integer in range",
            Self::Number => "enter a finite JSON number",
            Self::Boolean => "enter true or false",
            Self::String => unreachable!(),
        };
        let value = parse(raw).map_err(|_| message.to_owned())?;
        let valid = match self {
            Self::Integer if value.is_number() && !raw.contains(['.', 'e', 'E']) => match format {
                Some("int8") => raw.parse::<i8>().is_ok(),
                Some("int16") => raw.parse::<i16>().is_ok(),
                Some("int32") => raw.parse::<i32>().is_ok(),
                Some("int64") => raw.parse::<i64>().is_ok(),
                Some("int128") => raw.parse::<i128>().is_ok(),
                Some("int") => raw.parse::<isize>().is_ok(),
                Some("uint8") => raw.parse::<u8>().is_ok(),
                Some("uint16") => raw.parse::<u16>().is_ok(),
                Some("uint32") => raw.parse::<u32>().is_ok(),
                Some("uint64") => raw.parse::<u64>().is_ok(),
                Some("uint128") => raw.parse::<u128>().is_ok(),
                Some("uint") => raw.parse::<usize>().is_ok(),
                _ => true,
            },
            Self::Integer => false,
            Self::Number => {
                value.is_number()
                    && if format == Some("float") {
                        raw.parse::<f32>().is_ok_and(f32::is_finite)
                    } else {
                        raw.parse::<f64>().is_ok_and(f64::is_finite)
                    }
            }
            Self::Boolean => value.is_boolean(),
            Self::String => unreachable!(),
        };
        if valid {
            Ok(value)
        } else {
            Err(message.into())
        }
    }
}

fn display_value(value: &Value) -> String {
    match value.as_str() {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => value.to_string(),
    }
}

struct Screen {
    restore_cooked_mode: bool,
}

impl Screen {
    fn enter() -> Result<Self, String> {
        let restore_cooked_mode =
            !terminal::is_raw_mode_enabled().map_err(|e| format!("enter form: {e}"))?;
        if restore_cooked_mode {
            terminal::enable_raw_mode().map_err(|e| format!("enter form: {e}"))?;
        }
        let guard = Self {
            restore_cooked_mode,
        };
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide,
            event::EnableBracketedPaste
        )
        .map_err(|e| format!("enter form: {e}"))?;
        Ok(guard)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let mut out = io::stdout().lock();
        let _ = execute!(
            out,
            event::DisableBracketedPaste,
            SetAttribute(Attribute::Reset),
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        if self.restore_cooked_mode {
            let _ = terminal::disable_raw_mode();
        }
    }
}

// Render terminal control characters as plain text, never as commands; keep
// submitted values separate from the display. Flatten docs to one help row.
fn clean(s: &str) -> String {
    s.chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => '�',
            c => c,
        })
        .collect()
}

fn cell_estimate(ch: char) -> usize {
    if ch.is_ascii() { 1 } else { 2 }
}

fn cells(s: &str) -> usize {
    s.chars().map(cell_estimate).sum()
}

fn clip(s: &str, budget: usize, tail: bool) -> String {
    if cells(s) <= budget {
        return s.to_owned();
    }
    if budget < 3 {
        return ".".repeat(budget);
    }
    let mut remaining = budget - 3;
    let mut fitting = |ch: &char| {
        let width = cell_estimate(*ch);
        if width > remaining {
            false
        } else {
            remaining -= width;
            true
        }
    };
    if tail {
        let chars: Vec<_> = s.chars().rev().take_while(&mut fitting).collect();
        format!("...{}", chars.into_iter().rev().collect::<String>())
    } else {
        format!("{}...", s.chars().take_while(fitting).collect::<String>())
    }
}

type Line = Vec<(Attributes, String)>;

fn plain(s: impl Into<String>) -> Line {
    vec![(Attributes::default(), s.into())]
}

fn text_line(label: &str, draft: Option<&str>, ghost: Option<&str>, width: usize) -> Line {
    let prefix = clip(&clean(label), width.saturating_sub(2) / 2, false);
    let space = width.saturating_sub(cells(&prefix) + 3);
    let mut line = plain(format!("{prefix}: "));
    if let Some(draft) = draft {
        line.push((Attributes::default(), clip(&clean(draft), space, true)));
        line.push((Attribute::Reverse.into(), " ".into()));
    } else if let Some(ghost) = ghost {
        line.push((Attribute::Dim.into(), clip(&clean(ghost), space, false)));
    } else {
        line.push((Attribute::Reverse.into(), " ".into()));
    }
    line
}

fn choice_line(label: &str, choices: &[Choice], active: usize, width: usize) -> Line {
    let prefix = clip(&clean(label), width.saturating_sub(2) / 2, false);
    let mut space = width.saturating_sub(cells(&prefix) + 4);
    let name = |c: &Choice| {
        if c.label.is_empty() {
            "\"\"".to_owned()
        } else {
            clean(&c.label)
        }
    };
    let attributes = |c: &Choice| {
        if c.ghost {
            Attribute::Dim.into()
        } else {
            Attributes::default()
        }
    };
    let selected = clip(&name(&choices[active]), space, false);
    let mut before = Vec::new();
    space = space.saturating_sub(cells(&selected));
    for choice in choices[..active].iter().rev() {
        let label = name(choice);
        let needed = cells(&label) + 3;
        if needed > space {
            break;
        }
        before.push((attributes(choice), format!("{label} / ")));
        space -= needed;
    }
    let mut line = plain(format!("{prefix}: ["));
    line.extend(before.into_iter().rev());
    line.push((attributes(&choices[active]) | Attribute::Reverse, selected));
    for choice in &choices[active + 1..] {
        let label = name(choice);
        let needed = cells(&label) + 3;
        if needed > space {
            break;
        }
        line.push((attributes(choice), format!(" / {label}")));
        space -= needed;
    }
    line.push((Attributes::default(), "]".into()));
    line
}

fn draw(lines: &[Line], width: usize) -> Result<(), String> {
    let mut out = io::stdout().lock();
    queue!(out, terminal::Clear(ClearType::All)).map_err(|e| e.to_string())?;
    for (row, line) in lines.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16)).map_err(|e| e.to_string())?;
        let mut space = width;
        for (attr, text) in line {
            let text = clip(&clean(text), space, false);
            queue!(
                out,
                SetAttributes(*attr),
                Print(&text),
                SetAttribute(Attribute::Reset)
            )
            .map_err(|e| e.to_string())?;
            space = space.saturating_sub(cells(&text));
        }
    }
    out.flush().map_err(|e| e.to_string())
}

fn size() -> (usize, usize) {
    let (width, height) = terminal::size().unwrap_or((80, 24));
    (
        if width == 0 {
            79
        } else {
            usize::from(width) - 1
        },
        if height == 0 { 24 } else { usize::from(height) },
    )
}

fn quit(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
}

fn rotate(key: KeyEvent, choice: &mut usize, len: usize) {
    match key.code {
        KeyCode::Left => *choice = if *choice == 0 { len - 1 } else { *choice - 1 },
        KeyCode::Right => *choice = (*choice + 1) % len,
        _ => {}
    }
}

fn edit(key: KeyEvent, draft: &mut Option<String>, limit: usize) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('u' | 'U') if ctrl => *draft = Some(String::new()),
        KeyCode::Backspace => {
            if let Some(text) = draft {
                text.pop();
            }
        }
        KeyCode::Char(c)
            if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && !c.is_control() =>
        {
            if draft.as_ref().map_or(0, String::len) + c.len_utf8() <= limit {
                draft.get_or_insert_with(String::new).push(c);
            }
        }
        _ => {}
    }
}

fn paste(text: &str, draft: &mut Option<String>, limit: usize) -> Result<(), String> {
    if text.len() > limit.saturating_sub(draft.as_ref().map_or(0, String::len)) {
        return Err(format!("pasted text exceeds the {limit}-byte field limit"));
    }
    draft.get_or_insert_with(String::new).push_str(text);
    Ok(())
}

fn field_line(field: &Field, label: &str, width: usize) -> Line {
    match &field.kind {
        Kind::Select(choices) => choice_line(label, choices, field.selection, width),
        Kind::Id(default) => text_line(label, field.draft.as_deref(), Some(default), width),
        Kind::Scalar {
            default, optional, ..
        } => {
            let ghost = default
                .as_ref()
                .map(display_value)
                .or_else(|| optional.then(|| "(omit)".into()));
            text_line(label, field.draft.as_deref(), ghost.as_deref(), width)
        }
        Kind::Object(_) => unreachable!(),
    }
}

fn render_form(
    form: &Form<'_>,
    steps: &[FieldId],
    active: usize,
    error: Option<&str>,
) -> Result<(), String> {
    let (width, height) = size();
    let active_id = steps.get(active).copied();
    let header = if active_id == Some(0) {
        plain("--id:")
    } else {
        plain(format!(
            "--id: {}",
            form.fields[0].confirmed_text().unwrap_or_default()
        ))
    };
    let mut body = Vec::new();
    let mut focus = 0;
    for &id in &steps[1..] {
        let field = &form.fields[id];
        let label = format!("  {}", field.label());
        if active_id == Some(id) {
            let choice_help = match &field.kind {
                Kind::Select(choices) => choices[field.selection].description.as_deref(),
                _ => None,
            };
            if let Some(help) = choice_help.or(field.description.as_deref()) {
                body.push(vec![(Attribute::Dim.into(), format!("  > {help}"))]);
            }
            focus = body.len();
            body.push(field_line(field, &label, width));
        } else if let Some(value) = field.confirmed_text() {
            body.push(plain(format!("{label}: {value}")));
        }
    }
    if active_id.is_none() {
        focus = body.len().saturating_sub(1);
    }
    let footer = if active_id.is_none() {
        "Enter to start · Esc to edit · Ctrl+C to cancel"
    } else if active_id.is_some_and(|id| matches!(form.fields[id].kind, Kind::Select(_))) {
        "←/→ choose · Enter accept · Esc back · Ctrl+C cancel"
    } else {
        "Enter accept · Esc back · Ctrl+C cancel"
    };
    // Keep the ID and input heading pinned when the fields exceed the screen.
    // A very small terminal prioritizes the current editor (or confirmation).
    if height < 5 {
        let mut lines = vec![match active_id {
            Some(0) => field_line(&form.fields[0], "--id", width),
            Some(_) => body.remove(focus),
            None => plain(footer),
        }];
        if let Some(error) = error.filter(|_| height > 1) {
            lines.push(plain(error));
        }
        return draw(&lines, width);
    }
    let capacity = height - 3 - usize::from(error.is_some());
    let start = focus.saturating_add(1).saturating_sub(capacity);
    if active_id == Some(0) {
        let mut lines = vec![
            header,
            plain("  > Workflow ID"),
            field_line(&form.fields[0], "  (top-level) (string)", width),
        ];
        if let Some(error) = error {
            lines.push(plain(format!("Error: {error}")));
        }
        lines.push(vec![(Attribute::Dim.into(), footer.into())]);
        return draw(&lines, width);
    }
    let mut lines = vec![header, plain("--input:")];
    lines.extend(body.into_iter().skip(start).take(capacity));
    if let Some(error) = error {
        lines.push(plain(format!("Error: {error}")));
    }
    lines.push(vec![(Attribute::Dim.into(), footer.into())]);
    draw(&lines, width)
}

fn summary(form: &Form<'_>) -> Result<(), String> {
    let (width, _) = size();
    let mut out = io::stdout().lock();
    let id = form.fields[0].confirmed_text().unwrap_or_default();
    writeln!(
        out,
        "{}",
        clip(&clean(&format!("--id: {id}")), width, false)
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "--input:").map_err(|e| e.to_string())?;
    let rows: Vec<_> = form
        .steps()
        .into_iter()
        .skip(1)
        .filter_map(|id| {
            let field = &form.fields[id];
            field
                .confirmed_text()
                .map(|value| format!("  {}: {value}", field.label()))
        })
        .collect();
    for row in &rows {
        writeln!(out, "{}", clip(&clean(row), width, false)).map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())
}

pub fn run(
    id: Option<String>,
    default_id: String,
    schema: &Schema,
    initial: Value,
) -> Result<Option<(String, String)>, String> {
    let mut form = Form::new(schema, id, default_id, &initial)?;
    let screen = Screen::enter()?;
    let mut steps = form.steps();
    let mut active = steps
        .iter()
        .position(|&id| form.fields[id].answer.is_none())
        .unwrap_or(steps.len());
    let mut error = None;
    loop {
        render_form(&form, &steps, active, error.as_deref())?;
        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if quit(key) {
                    return Ok(None);
                }
                match key.code {
                    KeyCode::Esc => {
                        if active == 0 {
                            return Ok(None);
                        }
                        active -= 1;
                        error = None;
                    }
                    KeyCode::Enter if key.kind == KeyEventKind::Press => {
                        if let Some(&id) = steps.get(active) {
                            match form.fields[id].accept() {
                                Ok(()) => {
                                    steps = form.steps();
                                    active += 1;
                                    while steps
                                        .get(active)
                                        .is_some_and(|&id| form.fields[id].answer.is_some())
                                    {
                                        active += 1;
                                    }
                                    error = None;
                                }
                                Err(e) => error = Some(e),
                            }
                        } else {
                            match form.submission() {
                                Ok(result) => {
                                    drop(screen);
                                    summary(&form)?;
                                    return Ok(Some(result));
                                }
                                Err(e) => error = Some(e),
                            }
                        }
                    }
                    _ => {
                        if let Some(&id) = steps.get(active) {
                            let field = &mut form.fields[id];
                            if let Kind::Select(choices) = &field.kind {
                                rotate(key, &mut field.selection, choices.len());
                            } else {
                                edit(key, &mut field.draft, if id == 0 { 255 } else { MAX_BYTES });
                            }
                            error = None;
                        }
                    }
                }
            }
            Event::Paste(text) => {
                if let Some(&id) = steps.get(active) {
                    let field = &mut form.fields[id];
                    if matches!(field.kind, Kind::Id(_) | Kind::Scalar { .. }) {
                        error = paste(
                            &text,
                            &mut field.draft,
                            if id == 0 { 255 } else { MAX_BYTES },
                        )
                        .err();
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn choose(label: &str, choices: &[String]) -> Result<Option<String>, String> {
    if choices.is_empty() {
        return Err("no choices available".into());
    }
    let options: Vec<_> = choices
        .iter()
        .map(|s| literal(s.clone(), None, false))
        .collect();
    let _screen = Screen::enter()?;
    let mut active = 0;
    loop {
        let (width, height) = size();
        let mut lines = vec![choice_line(label, &options, active, width)];
        if height > 1 {
            lines.push(vec![(
                Attribute::Dim.into(),
                "←/→ choose · Enter accept · Esc cancel".into(),
            )]);
        }
        draw(&lines, width)?;
        if let Event::Key(key) = event::read().map_err(|e| e.to_string())?
            && key.kind != KeyEventKind::Release
        {
            if quit(key) || key.code == KeyCode::Esc {
                return Ok(None);
            }
            if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press {
                return Ok(Some(choices[active].clone()));
            }
            rotate(key, &mut active, choices.len());
        }
    }
}

pub fn text(label: &str) -> Result<Option<String>, String> {
    let _screen = Screen::enter()?;
    let mut draft = None;
    let mut error = None;
    loop {
        let (width, height) = size();
        let mut lines = vec![text_line(label, draft.as_deref(), None, width)];
        if height > 1 {
            lines.push(if let Some(e) = &error {
                plain(e)
            } else {
                vec![(Attribute::Dim.into(), "Enter accept · Esc cancel".into())]
            });
        }
        draw(&lines, width)?;
        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if quit(key) || key.code == KeyCode::Esc {
                    return Ok(None);
                }
                if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press {
                    return Ok(Some(draft.unwrap_or_default()));
                }
                edit(key, &mut draft, 255);
                error = None;
            }
            Event::Paste(text) => error = paste(&text, &mut draft, 255).err(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boreal_common::input::json;

    #[test]
    fn schema_order_prefill_and_tagged_payload() {
        let schema = Schema::new(json!({
            "type": "object", "required": ["z", "nested", "a"],
            "properties": {
                "z": {"type": "integer", "format": "uint64"},
                "nested": {"$ref": "#/$defs/Parent", "description": "Parent docs"},
                "a": {"type": "boolean", "default": false}
            },
            "$defs": {"Parent": {"type": "object", "required": ["left"], "properties": {
                "left": {"oneOf": [
                    {"type": "string", "enum": ["Skip"]},
                    {"type": "object", "properties": {"Float": {"type": "number"}}, "required": ["Float"], "additionalProperties": false}
                ]}
            }}}
        })).unwrap();
        let mut form = Form::new(
            &schema,
            Some("wf".into()),
            "other".into(),
            &json!({"z": u64::MAX}),
        )
        .unwrap();
        let steps = form.steps();
        assert_eq!(
            steps
                .iter()
                .map(|&id| form.fields[id].title())
                .collect::<Vec<_>>(),
            ["--id", "z", "nested.left", "a"]
        );
        let left = steps[2];
        assert!(form.fields[steps[1]].answer.is_some());
        form.fields[left].selection = 1;
        form.fields[left].accept().unwrap();
        let payload = form.steps()[3];
        assert_eq!(form.fields[payload].title(), "nested.left.Float");
        assert_eq!(
            form.fields[payload].description.as_deref(),
            Some("Parent docs")
        );
        assert!(form.fields[left].confirmed_text().is_none());
        form.fields[payload].draft = Some("1.5".into());
        form.fields[payload].accept().unwrap();
        form.fields[steps[3]].accept().unwrap();
        assert_eq!(
            form.submission().unwrap(),
            (
                "wf".into(),
                r#"{"z":18446744073709551615,"nested":{"left":{"Float":1.5}},"a":false}"#.into()
            )
        );
        assert!(
            Scalar::Integer
                .parse("18446744073709551616", Some("uint64"))
                .is_err()
        );
    }

    #[test]
    fn invalid_enter_keeps_draft_and_default_is_explicit() {
        let schema = Schema::new(json!({"type":"integer", "minimum": 5, "default": 8})).unwrap();
        let mut form = Form::new(&schema, None, "generated".into(), &json!({})).unwrap();
        let field = &mut form.fields[form.root];
        field.draft = Some("4".into());
        assert!(field.accept().is_err());
        assert_eq!(field.draft.as_deref(), Some("4"));
        assert!(field.answer.is_none());
        field.draft = None;
        field.accept().unwrap();
        assert_eq!(form.value(form.root), Some(json!(8)));
    }

    #[test]
    fn unsupported_shapes_and_safe_display() {
        for raw in [
            json!({"type":"array","items":{"type":"string"}}),
            json!({"type":"object", "additionalProperties":{"type":"string"}}),
        ] {
            let schema = Schema::new(raw).unwrap();
            assert!(
                matches!(Form::new(&schema, None, "generated".into(), &json!({})), Err(e) if e.contains("--input-json"))
            );
        }
        assert_eq!(
            pointer("#/$defs/Left%20right~1"),
            Some("/$defs/Left right~1".into())
        );
        assert_eq!(clean("a\u{1b}[2J\u{9b}\r\nb"), "a�[2J�  b");
    }
}
