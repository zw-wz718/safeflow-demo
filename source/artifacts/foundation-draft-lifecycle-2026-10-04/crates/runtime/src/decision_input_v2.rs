use serde_json::Value;

pub(crate) fn validate_nested_schema(schema: &Value) -> Result<(), ()> {
    if serde_json::to_vec(schema).map_err(|_| ())?.len() > 16384 {
        return Err(());
    }
    object(schema, None, 0, &mut 0)
}

pub(crate) fn validate_nested_object(schema: &Value, input: &Value) -> Result<(), ()> {
    if serde_json::to_vec(schema).map_err(|_| ())?.len() > 16384
        || serde_json::to_vec(input).map_err(|_| ())?.len() > 4096
    {
        return Err(());
    }
    object(schema, Some(input), 0, &mut 0)
}

fn object(
    schema: &Value,
    input: Option<&Value>,
    depth: usize,
    fields: &mut usize,
) -> Result<(), ()> {
    let s = schema.as_object().ok_or(())?;
    if depth > 1
        || s.get("type").and_then(Value::as_str) != Some("object")
        || s.get("additionalProperties") != Some(&Value::Bool(false))
        || s.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "properties" | "required" | "additionalProperties" | "minProperties"
            )
        })
        || (depth == 0 && (s.contains_key("minProperties") || !s.contains_key("required")))
    {
        return Err(());
    }
    let properties = s.get("properties").and_then(Value::as_object).ok_or(())?;
    *fields = fields.checked_add(properties.len()).ok_or(())?;
    if *fields > 64 {
        return Err(());
    }
    let value = input.map(|value| value.as_object().ok_or(())).transpose()?;
    if value.is_some_and(|value| value.keys().any(|key| !properties.contains_key(key))) {
        return Err(());
    }
    let minimum = s
        .get("minProperties")
        .map(|value| value.as_u64().filter(|n| *n <= 64).ok_or(()))
        .transpose()?;
    if minimum.is_some_and(|n| {
        n > properties.len() as u64 || value.is_some_and(|value| (value.len() as u64) < n)
    }) {
        return Err(());
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(required) = s.get("required") {
        for name in required.as_array().ok_or(())? {
            let name = name.as_str().ok_or(())?;
            if !seen.insert(name)
                || !properties.contains_key(name)
                || value.is_some_and(|value| !value.contains_key(name))
            {
                return Err(());
            }
        }
    }
    for (name, schema) in properties {
        let input = value.and_then(|value| value.get(name));
        if schema.get("type").and_then(Value::as_str) == Some("object") {
            object(schema, input, depth + 1, fields)?;
        } else {
            super::validate_property(schema, input)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn startup_schema_check_uses_existing_projection_grammar_without_input() {
        let direct = json!({"type":"object","required":["id"],"additionalProperties":false,"properties":{"id":{"type":"string","minLength":1}}});
        assert!(super::super::validate_direct_schema(&direct).is_ok());
        assert!(validate_nested_schema(&direct).is_ok());
        assert!(validate_nested_object(&direct, &json!({})).is_err());
        let mut nested = direct.clone();
        nested["properties"]["changes"] = json!({"type":"object","additionalProperties":false,"minProperties":1,"properties":{"amount":{"type":"string"}}});
        assert!(validate_nested_schema(&nested).is_ok());
        assert!(super::super::validate_direct_schema(&nested).is_err());
        for key in ["$ref", "oneOf", "format"] {
            let mut invalid = direct.clone();
            invalid["properties"]["id"][key] = json!("unsupported");
            assert!(validate_nested_schema(&invalid).is_err());
            assert!(super::super::validate_direct_schema(&invalid).is_err());
        }
        for required in [json!(["absent"]), json!(["id", "id"])] {
            let mut invalid = direct.clone();
            invalid["required"] = required;
            assert!(validate_nested_schema(&invalid).is_err());
            assert!(super::super::validate_direct_schema(&invalid).is_err());
        }
    }

    #[test]
    fn nested_required_is_optional_but_constraints_are_closed() {
        let schema = json!({"type":"object","required":["changes"],"additionalProperties":false,"properties":{
            "changes":{"type":"object","minProperties":1,"additionalProperties":false,"properties":{"amount":{"type":"string","minLength":1}}}}});
        assert!(validate_nested_object(&schema, &json!({"changes":{"amount":"300.00"}})).is_ok());
        assert!(super::super::validate_direct_object(
            &schema,
            &json!({"changes":{"amount":"300.00"}})
        )
        .is_err());
        for input in [
            json!({"changes":{}}),
            json!({"changes":null}),
            json!({"changes":{"amount":null}}),
            json!({"changes":{"unknown":"x"}}),
        ] {
            assert!(validate_nested_object(&schema, &input).is_err());
        }
        let mut bad = schema.clone();
        bad["properties"]["changes"]["properties"]["optional"] =
            json!({"type":"string","unknown":true});
        assert!(validate_nested_object(&bad, &json!({"changes":{"amount":"1"}})).is_err());
        bad["properties"]["changes"]["properties"]["optional"] =
            json!({"type":"object","required":[],"properties":{},"additionalProperties":false});
        assert!(validate_nested_object(&bad, &json!({"changes":{"amount":"1"}})).is_err());
    }

    #[test]
    fn nested_optional_schema_cannot_hide_unsupported_rules_or_exceed_tree_budget() {
        let base = json!({"type":"object","required":[],"additionalProperties":false,"properties":{
            "changes":{"type":"object","additionalProperties":false,"properties":{"amount":{"type":"string"}}}}});
        for rule in [
            json!(["amount", "amount"]),
            json!(["missing"]),
            json!("amount"),
        ] {
            let mut schema = base.clone();
            schema["properties"]["changes"]["required"] = rule;
            assert!(validate_nested_object(&schema, &json!({})).is_err());
        }
        for key in [
            "$ref",
            "oneOf",
            "patternProperties",
            "unevaluatedProperties",
        ] {
            let mut schema = base.clone();
            schema["properties"]["changes"][key] = json!({});
            assert!(validate_nested_object(&schema, &json!({})).is_err());
        }
        let mut oversized = base.clone();
        let fields = oversized["properties"]["changes"]["properties"]
            .as_object_mut()
            .unwrap();
        for n in 0..64 {
            fields.insert(format!("optional_{n}"), json!({"type":"string"}));
        }
        assert!(validate_nested_object(&oversized, &json!({})).is_err());
        assert!(
            validate_nested_object(&base, &json!({"changes":{"amount":"x".repeat(4097)}})).is_err()
        );
        let mut oversized = base.clone();
        oversized["properties"]["changes"]["properties"]["amount"]["enum"] =
            json!(["x".repeat(16385)]);
        assert!(validate_nested_object(&oversized, &json!({})).is_err());
    }
}
