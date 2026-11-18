use std::sync::Arc;

pub use serde_json::{Map, Value, json};

pub const MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Schema {
    value: Value,
    validator: Arc<jsonschema::Validator>,
}

impl PartialEq for Schema {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

struct Offline;

impl jsonschema::Retrieve for Offline {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema references are disabled".into())
    }
}

impl Schema {
    pub fn new(value: Value) -> Result<Self, String> {
        let validator = jsonschema::draft202012::options()
            .with_retriever(Offline)
            .build(&value)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            value,
            validator: Arc::new(validator),
        })
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn validate(&self, value: &Value) -> Result<(), String> {
        self.validator.validate(value).map_err(|e| {
            let path = e.instance_path().to_string();
            let at = if path.is_empty() { "/" } else { &path };
            // Validator messages can include the submitted value.
            format!("input at {at:?} does not match the workflow schema")
        })
    }
}

pub fn parse(raw: &str) -> Result<Value, String> {
    if raw.len() > MAX_BYTES {
        return Err(format!("workflow input exceeds {MAX_BYTES} bytes"));
    }
    serde_json::from_str(raw).map_err(|e| {
        format!(
            "invalid JSON input at line {}, column {}",
            e.line(),
            e.column()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validates_unsigned_range_and_rejects_external_refs() {
        let schema =
            Schema::new(json!({"type":"integer", "minimum":0, "maximum":u64::MAX})).unwrap();
        let max = "18446744073709551615";
        let value = parse(max).unwrap();
        assert_eq!(value.to_string(), max);
        assert!(schema.validate(&value).is_ok());
        assert!(
            schema
                .validate(&parse("18446744073709551616").unwrap())
                .is_err()
        );
        assert!(Schema::new(json!({"$ref":"file:///not-a-local-schema.json"})).is_err());
    }
}
