use regex::RegexBuilder;
use regex_syntax::ast::{self, Ast, ClassSet, ClassSetItem};
use serde_json::{Map, Value};

#[path = "decision_input_v2.rs"]
mod v2;
pub(super) use v2::{validate_nested_object, validate_nested_schema};

fn literal_compatible(l: &ast::Literal) -> Result<(), ()> {
    if l.c as u32 > 0xffff
        || matches!(
            l.kind,
            ast::LiteralKind::Octal
                | ast::LiteralKind::HexBrace(_)
                | ast::LiteralKind::HexFixed(ast::HexLiteralKind::UnicodeLong)
                | ast::LiteralKind::Special(
                    ast::SpecialLiteralKind::Bell | ast::SpecialLiteralKind::Space
                )
        )
    {
        return Err(());
    }
    Ok(())
}
fn ecma_class(p: &ast::ClassPerl) -> Result<Box<ast::ClassBracketed>, ()> {
    let body = match p.kind {
        ast::ClassPerlKind::Digit => "0-9",
        ast::ClassPerlKind::Word => "A-Za-z0-9_",
        ast::ClassPerlKind::Space => {
            r"\u0009-\u000D\u0020\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000\uFEFF"
        }
    };
    let parsed = ast::parse::Parser::new()
        .parse(&format!("[{}{body}]", if p.negated { "^" } else { "" }))
        .map_err(|_| ())?;
    if let Ast::ClassBracketed(c) = &parsed {
        Ok(c.clone())
    } else {
        Err(())
    }
}
fn class_compatible(c: &mut ClassSetItem) -> Result<(), ()> {
    match c {
        ClassSetItem::Literal(l) => literal_compatible(l),
        ClassSetItem::Range(r) => {
            literal_compatible(&r.start)?;
            literal_compatible(&r.end)
        }
        ClassSetItem::Perl(p) => {
            *c = ClassSetItem::Bracketed(ecma_class(p)?);
            Ok(())
        }
        ClassSetItem::Union(u) => {
            for item in &mut u.items {
                class_compatible(item)?;
            }
            Ok(())
        }
        _ => Err(()),
    }
}
fn ast_compatible(a: &mut Ast) -> Result<(), ()> {
    match a {
        Ast::Empty(_) => Ok(()),
        Ast::Literal(l) => literal_compatible(l),
        Ast::Assertion(a)
            if matches!(
                a.kind,
                ast::AssertionKind::StartLine | ast::AssertionKind::EndLine
            ) =>
        {
            Ok(())
        }
        Ast::ClassPerl(p) => {
            *a = Ast::ClassBracketed(ecma_class(p)?);
            Ok(())
        }
        Ast::ClassBracketed(c) => {
            if let ClassSet::Item(i) = &mut c.kind {
                class_compatible(i)
            } else {
                Err(())
            }
        }
        Ast::Repetition(r) => ast_compatible(&mut r.ast),
        Ast::Group(g) => {
            if matches!(g.kind, ast::GroupKind::CaptureName { .. })
                || g.flags().is_some_and(|f| !f.items.is_empty())
            {
                return Err(());
            }
            ast_compatible(&mut g.ast)
        }
        Ast::Alternation(v) => {
            for a in &mut v.asts {
                ast_compatible(a)?;
            }
            Ok(())
        }
        Ast::Concat(v) => {
            for a in &mut v.asts {
                ast_compatible(a)?;
            }
            Ok(())
        }
        // Dot/word-boundary/flags/properties/set-operations and non-BMP need a fuller contract.
        _ => Err(()),
    }
}
fn compatible_pattern(p: &str) -> Result<regex::Regex, ()> {
    let mut ast = ast::parse::Parser::new().parse(p).map_err(|_| ())?;
    ast_compatible(&mut ast)?;
    RegexBuilder::new(&ast.to_string())
        .size_limit(65536)
        .dfa_size_limit(65536)
        .build()
        .map_err(|_| ())
}

// Deliberately closed projection contract, not a partial permissive JSON Schema engine.
// Unsupported keywords/types and malformed constraints reject even for absent optional fields.
pub(super) fn validate_direct_object(schema: &Value, input: &Value) -> Result<(), ()> {
    direct_object(schema, Some(input))
}

pub(super) fn validate_direct_schema(schema: &Value) -> Result<(), ()> {
    direct_object(schema, None)
}

fn direct_object(schema: &Value, input: Option<&Value>) -> Result<(), ()> {
    if serde_json::to_vec(schema).map_err(|_| ())?.len() > 16384
        || serde_json::to_vec(&input).map_err(|_| ())?.len() > 4096
    {
        return Err(());
    }
    let schema = schema.as_object().ok_or(())?;
    if schema.keys().any(|k| {
        !matches!(
            k.as_str(),
            "type" | "properties" | "required" | "additionalProperties"
        )
    }) || schema.get("type") != Some(&Value::String("object".into()))
        || schema.get("additionalProperties") != Some(&Value::Bool(false))
    {
        return Err(());
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or(())?;
    let required = schema.get("required").and_then(Value::as_array).ok_or(())?;
    let object = input.map(|input| input.as_object().ok_or(())).transpose()?;
    if properties.len() > 64
        || required.len() > properties.len()
        || object.is_some_and(|object| object.keys().any(|k| !properties.contains_key(k)))
    {
        return Err(());
    }
    let mut seen = std::collections::HashSet::new();
    for key in required {
        let key = key.as_str().ok_or(())?;
        if !seen.insert(key)
            || !properties.contains_key(key)
            || object.is_some_and(|object| !object.contains_key(key))
        {
            return Err(());
        }
    }
    for (name, property) in properties {
        validate_property(property, object.and_then(|object| object.get(name)))?;
    }
    Ok(())
}
fn integer(v: &Value) -> Result<i128, ()> {
    v.as_i64()
        .map(i128::from)
        .or_else(|| v.as_u64().map(i128::from))
        .ok_or(())
}
fn length(s: &Map<String, Value>, key: &str) -> Result<Option<u64>, ()> {
    s.get(key)
        .map(|v| v.as_u64().filter(|n| *n <= 4096).ok_or(()))
        .transpose()
}
fn number(s: &Map<String, Value>, key: &str) -> Result<Option<i128>, ()> {
    s.get(key).map(integer).transpose()
}
fn has_type(v: &Value, kind: &str) -> bool {
    match kind {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => false,
    }
}
fn validate_property(schema: &Value, value: Option<&Value>) -> Result<(), ()> {
    let s = schema.as_object().ok_or(())?;
    let kind = s.get("type").and_then(Value::as_str).ok_or(())?;
    if !matches!(kind, "string" | "integer" | "boolean" | "null")
        || s.keys().any(|k| {
            !matches!(
                k.as_str(),
                "type"
                    | "enum"
                    | "minLength"
                    | "maxLength"
                    | "pattern"
                    | "minimum"
                    | "maximum"
                    | "exclusiveMinimum"
                    | "exclusiveMaximum"
                    | "multipleOf"
            )
        })
    {
        return Err(());
    }
    if kind != "string"
        && ["minLength", "maxLength", "pattern"]
            .iter()
            .any(|k| s.contains_key(*k))
    {
        return Err(());
    }
    if kind != "integer"
        && [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ]
        .iter()
        .any(|k| s.contains_key(*k))
    {
        return Err(());
    }
    let minimum_length = length(s, "minLength")?;
    let maximum_length = length(s, "maxLength")?;
    if minimum_length
        .zip(maximum_length)
        .is_some_and(|(a, b)| a > b)
    {
        return Err(());
    }
    let pattern = s
        .get("pattern")
        .map(|v| {
            let p = v.as_str().filter(|p| p.len() <= 1024).ok_or(())?;
            compatible_pattern(p)
        })
        .transpose()?;
    let minimum = number(s, "minimum")?;
    let maximum = number(s, "maximum")?;
    let exclusive_minimum = number(s, "exclusiveMinimum")?;
    let exclusive_maximum = number(s, "exclusiveMaximum")?;
    let multiple = number(s, "multipleOf")?;
    if minimum.zip(maximum).is_some_and(|(a, b)| a > b)
        || exclusive_minimum
            .zip(exclusive_maximum)
            .is_some_and(|(a, b)| a >= b)
        || multiple.is_some_and(|n| n <= 0)
    {
        return Err(());
    }
    let enumeration = s
        .get("enum")
        .map(|e| {
            e.as_array()
                .filter(|e| !e.is_empty() && e.len() <= 64)
                .ok_or(())
        })
        .transpose()?;
    if let Some(e) = enumeration {
        for (i, v) in e.iter().enumerate() {
            if !has_type(v, kind) || e[..i].contains(v) {
                return Err(());
            }
        }
    }
    if let Some(value) = value {
        if !has_type(value, kind) || enumeration.is_some_and(|e| !e.contains(value)) {
            return Err(());
        }
        if let Some(text) = value.as_str() {
            let count = text.chars().count() as u64;
            if text.chars().any(char::is_control)
                || text.contains("${")
                || text.contains("{{")
                || text.contains("$ref")
                || minimum_length.is_some_and(|n| count < n)
                || maximum_length.is_some_and(|n| count > n)
                || pattern
                    .as_ref()
                    .is_some_and(|p| text.chars().any(|c| c as u32 > 0xffff) || !p.is_match(text))
            {
                return Err(());
            }
        }
        if kind == "integer" {
            let n = integer(value)?;
            if minimum.is_some_and(|v| n < v)
                || maximum.is_some_and(|v| n > v)
                || exclusive_minimum.is_some_and(|v| n <= v)
                || exclusive_maximum.is_some_and(|v| n >= v)
                || multiple.is_some_and(|v| n % v != 0)
            {
                return Err(());
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn schema(property: Value) -> Value {
        json!({"type":"object","properties":{"field":property},"required":["field"],"additionalProperties":false})
    }
    #[test]
    fn declared_patterns_lengths_and_integer_constraints_are_enforced() {
        let s = schema(
            json!({"type":"string","pattern":"^allowed_only$","minLength":1,"maxLength":12}),
        );
        assert!(validate_direct_object(&s, &json!({"field":"allowed_only"})).is_ok());
        assert!(validate_direct_object(&s, &json!({"field":"account_001"})).is_err());
        let s = schema(
            json!({"type":"integer","minimum":2,"maximum":8,"exclusiveMinimum":2,"exclusiveMaximum":8,"multipleOf":2}),
        );
        for n in [0, 2, 3, 8, 10] {
            assert!(validate_direct_object(&s, &json!({"field":n})).is_err());
        }
        assert!(validate_direct_object(&s, &json!({"field":4})).is_ok());
        assert!(validate_direct_object(&s, &json!({"field":4.0})).is_err());
    }
    #[test]
    fn missing_malformed_and_unknown_root_and_field_constraints_reject() {
        for p in [
            json!({}),
            json!({"type":["string","null"]}),
            json!({"type":"string","minLength":"1"}),
            json!({"type":"string","maxLength":-1}),
            json!({"type":"string","pattern":"["}),
            json!({"type":"string","pattern":1}),
            json!({"type":"integer","minimum":"1"}),
            json!({"type":"integer","maximum":null}),
            json!({"type":"integer","multipleOf":0}),
            json!({"type":"string","format":"email"}),
            json!({"type":"string","enum":[]}),
            json!({"type":"boolean","minLength":1}),
            json!({"type":"string","minLength":4,"maxLength":2}),
        ] {
            assert!(validate_direct_object(&schema(p.clone()), &json!({"field":"x"})).is_err());
            let mut s = schema(p);
            s["required"] = json!([]);
            assert!(validate_direct_object(&s, &json!({})).is_err());
        }
        for (k, v) in [
            ("allOf", json!([])),
            ("anyOf", json!([])),
            ("oneOf", json!([])),
            ("not", json!({})),
            ("minProperties", json!(1)),
            ("required", json!("field")),
            ("additionalProperties", json!(true)),
            ("type", json!(null)),
        ] {
            let mut s = schema(json!({"type":"string"}));
            s[k] = v;
            assert!(validate_direct_object(&s, &json!({"field":"x"})).is_err());
        }
    }
    #[test]
    fn ecmascript_digit_word_and_space_classes_never_broaden_with_unicode() {
        for (pattern, good, bad) in [
            (r"^\d+$", "12", "١٢"),
            (r"^\w+$", "word_1", "汉字"),
            (r"^[\d]+$", "12", "١٢"),
            (r"^[\w]+$", "word", "汉字"),
            (r"^\S+$", "汉字", "\u{feff}"),
            (r"^[^\s]+$", "汉字", "\u{3000}"),
        ] {
            let s = schema(json!({"type":"string","pattern":pattern}));
            assert!(
                validate_direct_object(&s, &json!({"field":good})).is_ok(),
                "{pattern}"
            );
            assert!(
                validate_direct_object(&s, &json!({"field":bad})).is_err(),
                "{pattern}"
            );
        }
        for p in [
            r"^\bword\b$",
            r"(?i)word",
            r"^\pL+$",
            r"^.$",
            r"[a&&b]",
            r"\a",
            r"\u{61}",
        ] {
            assert!(validate_direct_object(
                &schema(json!({"type":"string","pattern":p})),
                &json!({"field":"a"})
            )
            .is_err());
        }
    }
}
