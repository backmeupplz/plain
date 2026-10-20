//! forge's JSON cheatcodes: what `vm.parseJson*`, `vm.keyExists`, `vm.parseJsonKeys` select and decode, and what
//! `vm.serialize*` and `vm.writeJson` write. Keys keep the order they have in the text (serde_json's preserve_order),
//! as in forge.
use alloy_dyn_abi::{DynSolType, DynSolValue};
use alloy_primitives::{Address, B256, I256, U256};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The values a JSONPath selects: `$` (or `.`) the root, `.key` / `['key']` children, `[n]` (negative from the end),
/// `[a,b]`, `[start:end]`, `*` and `..key` (any depth). A path not starting with `$` starts at the root.
pub fn select<'a>(root: &'a Value, path: &str) -> Result<Vec<&'a Value>, String> {
    let path = if path == "." { "$" } else { path };
    let mut rest = path.strip_prefix('$').unwrap_or(path);
    let mut cur = vec![root];
    let bad = || format!("unsupported or invalid JSONPath {path:?}");
    while !rest.is_empty() {
        let deep = rest.starts_with("..");
        if deep {
            rest = &rest[2..];
            // every node at or below the current ones; the step after `..` then applies to each
            let mut all = vec![];
            for v in cur {
                descend(v, &mut all);
            }
            cur = all;
            if rest.starts_with('[') {
                continue;
            }
        } else if let Some(r) = rest.strip_prefix('.') {
            rest = r;
        }
        let (step, next) = if let Some(r) = rest.strip_prefix('[') {
            let end = bracket_end(r).ok_or_else(bad)?;
            (Step::parse(&r[..end]).ok_or_else(bad)?, &r[end + 1..])
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            if end == 0 {
                return Err(bad());
            }
            let key = &rest[..end];
            (if key == "*" { Step::All } else { Step::Keys(vec![key.to_string()]) }, &rest[end..])
        };
        rest = next;
        cur = cur.into_iter().flat_map(|v| step.apply(v)).collect();
    }
    Ok(cur)
}

fn descend<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(v);
    match v {
        Value::Array(a) => a.iter().for_each(|x| descend(x, out)),
        Value::Object(o) => o.values().for_each(|x| descend(x, out)),
        _ => {}
    }
}

/// The index of the `]` closing a bracket whose content starts `s`, skipping quoted keys.
fn bracket_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ']') => return Some(i),
            _ => {}
        }
    }
    None
}

enum Step {
    All,
    Keys(Vec<String>),
    Indices(Vec<i64>),
    Slice(Option<i64>, Option<i64>),
}

impl Step {
    fn parse(s: &str) -> Option<Step> {
        let s = s.trim();
        if s == "*" {
            return Some(Step::All);
        }
        if s.starts_with('?') || s.starts_with('(') {
            return None; // filters and expressions
        }
        if let Some((a, b)) = s.split_once(':') {
            let n = |x: &str| if x.trim().is_empty() { Some(None) } else { x.trim().parse().ok().map(Some) };
            return Some(Step::Slice(n(a)?, n(b.split(':').next()?)?));
        }
        let parts: Vec<&str> = s.split(',').map(str::trim).collect();
        if parts.iter().all(|p| p.starts_with(['\'', '"'])) {
            return Some(Step::Keys(parts.iter().map(|p| p[1..p.len().saturating_sub(1).max(1)].to_string()).collect()));
        }
        parts.iter().map(|p| p.parse().ok()).collect::<Option<Vec<i64>>>().map(Step::Indices)
    }

    fn apply<'a>(&self, v: &'a Value) -> Vec<&'a Value> {
        let at = |a: &'a [Value], i: i64| if i < 0 { a.len().checked_sub(i.unsigned_abs() as usize).and_then(|j| a.get(j)) } else { a.get(i as usize) };
        match (self, v) {
            (Step::All, Value::Array(a)) => a.iter().collect(),
            (Step::All, Value::Object(o)) => o.values().collect(),
            (Step::Keys(ks), Value::Object(o)) => ks.iter().filter_map(|k| o.get(k)).collect(),
            (Step::Indices(is), Value::Array(a)) => is.iter().filter_map(|i| at(a, *i)).collect(),
            (Step::Slice(s, e), Value::Array(a)) => {
                let n = a.len() as i64;
                let clamp = |x: i64| if x < 0 { (n + x).max(0) } else { x.min(n) } as usize;
                let (s, e) = (clamp(s.unwrap_or(0)), clamp(e.unwrap_or(n)));
                a.get(s..e.max(s)).map(|x| x.iter().collect()).unwrap_or_default()
            }
            _ => vec![],
        }
    }
}

/// The fields of every struct in `sources` ("Contract.Struct" or "Struct" -> member names, in order): forge orders an
/// object's values by the struct with the same member names when decoding it untyped.
pub fn struct_defs<'a>(sources: impl Iterator<Item = &'a str>) -> BTreeMap<String, Vec<String>> {
    use crate::rewrite::{K, lex};
    let mut out = BTreeMap::new();
    for src in sources {
        let toks = lex(src);
        let t = |i: usize| toks.get(i).map_or("", |x| &src[x.s..x.e]);
        let mut owner: Vec<(String, usize)> = vec![]; // (contract, depth its body is at)
        let mut depth = 0;
        let mut i = 0;
        while i < toks.len() {
            match t(i) {
                "contract" | "library" | "interface" if toks.get(i + 1).is_some_and(|x| x.k == K::Ident) => owner.push((t(i + 1).to_string(), depth + 1)),
                "struct" if toks.get(i + 1).is_some_and(|x| x.k == K::Ident) && t(i + 2) == "{" => {
                    let name = t(i + 1).to_string();
                    let (mut fields, mut j, mut nest, mut last) = (vec![], i + 3, 0, "");
                    while j < toks.len() && !(nest == 0 && t(j) == "}") {
                        match t(j) {
                            "(" | "[" | "{" => nest += 1,
                            ")" | "]" | "}" => nest -= 1,
                            ";" if nest == 0 => fields.push(last.to_string()),
                            _ if toks[j].k == K::Ident => last = t(j),
                            _ => {}
                        }
                        j += 1;
                    }
                    let key = match owner.last().filter(|(_, d)| *d == depth) {
                        Some((c, _)) => format!("{c}.{name}"),
                        None => name,
                    };
                    out.insert(key, fields);
                    i = j;
                }
                "{" => depth += 1,
                "}" => {
                    if owner.last().is_some_and(|(_, d)| *d == depth) {
                        owner.pop();
                    }
                    depth -= 1;
                }
                _ => {}
            }
            i += 1;
        }
    }
    out
}

/// forge's untyped decoding (`vm.parseJson`): null is bytes32(0), numbers are uint256 (int256 if negative), "0x"
/// strings are an address (20 bytes), a bytes32 (32) or bytes, numeric strings past 64 bits are numbers, objects are
/// tuples ordered like the struct with the same member names (else by key).
pub fn token(v: &Value, defs: &BTreeMap<String, Vec<String>>) -> Result<DynSolValue, String> {
    Ok(match v {
        Value::Null => DynSolValue::FixedBytes(B256::ZERO, 32),
        Value::Bool(b) => DynSolValue::Bool(*b),
        Value::Array(a) => DynSolValue::Array(a.iter().map(|x| token(x, defs)).collect::<Result<_, _>>()?),
        Value::Object(o) => {
            let keys: std::collections::BTreeSet<&str> = o.keys().map(String::as_str).collect();
            let order: Vec<&str> = match defs.values().find(|f| f.len() == keys.len() && f.iter().map(String::as_str).collect::<std::collections::BTreeSet<_>>() == keys) {
                Some(f) => f.iter().map(String::as_str).collect(),
                None => keys.into_iter().collect(),
            };
            DynSolValue::Tuple(order.iter().map(|k| token(&o[*k], defs)).collect::<Result<_, _>>()?)
        }
        Value::Number(n) => {
            // whole numbers only; serde_json writes big ones as 1e20, which reads back in full from the f64
            let unsupported = || format!("unsupported JSON number: {n}");
            let f = n.as_f64().filter(|f| f.fract() == 0.0).ok_or_else(unsupported)?;
            let s = n.to_string();
            let s = if s.contains('e') { format!("{f}") } else { s };
            match (s.parse::<U256>(), s.parse::<I256>()) {
                (Ok(u), _) => DynSolValue::Uint(u, 256),
                (_, Ok(i)) => DynSolValue::Int(i, 256),
                _ => return Err(unsupported()),
            }
        }
        Value::String(s) => {
            if let Some(h) = s.strip_prefix("0x") {
                if h.len() == 39 {
                    return Err(format!("Cannot parse \"{h}\" as an address. If you want to specify address, prepend zero to the value."));
                }
                let h = if h.len() % 2 == 1 { format!("0{h}") } else { h.to_string() };
                if let Ok(b) = hex::decode(&h) {
                    return Ok(match b.len() {
                        20 => DynSolValue::Address(Address::from_slice(&b)),
                        32 => DynSolValue::FixedBytes(B256::from_slice(&b), 32),
                        _ => DynSolValue::Bytes(b),
                    });
                }
            }
            match (s.parse::<I256>(), s.parse::<U256>()) {
                (Ok(i), _) if i64::try_from(i).is_err() => DynSolValue::Int(i, 256),
                (_, Ok(u)) if u64::try_from(u).is_err() => DynSolValue::Uint(u, 256),
                _ => DynSolValue::String(s.clone()),
            }
        }
    })
}

/// A string as `ty`, as forge's parse and env cheatcodes read it: alloy's coercion (decimal or 0x numbers, units
/// like "1 ether", checksummed addresses), plus 1/0/true/false for bools, and "0x" required on hex.
pub fn parse_value(s: &str, ty: &DynSolType) -> Result<DynSolValue, String> {
    ty.coerce_str(s).or_else(|e| match ty {
        DynSolType::Bool if s == "1" || s.eq_ignore_ascii_case("true") => Ok(DynSolValue::Bool(true)),
        DynSolType::Bool if s == "0" || s.eq_ignore_ascii_case("false") => Ok(DynSolValue::Bool(false)),
        DynSolType::Int(_) | DynSolType::Uint(_) | DynSolType::FixedBytes(_) | DynSolType::Bytes if !s.starts_with("0x") && !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Err(format!("failed parsing {s:?} as type `{ty}`: missing hex prefix (\"0x\") for hex string"))
        }
        _ => Err(format!("failed parsing {s:?} as type `{ty}`: {e}")),
    })
}

/// A JSON value as `ty` (the typed `vm.parseJson*`): strings and numbers coerced as `parse_value` does.
pub fn coerce(v: &Value, ty: &DynSolType) -> Result<DynSolValue, String> {
    match (v, ty) {
        (Value::Array(a), DynSolType::Array(t)) => Ok(DynSolValue::Array(a.iter().map(|x| coerce(x, t)).collect::<Result<_, _>>()?)),
        (Value::Array(a), DynSolType::FixedArray(t, n)) if a.len() == *n => Ok(DynSolValue::FixedArray(a.iter().map(|x| coerce(x, t)).collect::<Result<_, _>>()?)),
        (Value::Array(a), DynSolType::Tuple(ts)) if a.len() == ts.len() => Ok(DynSolValue::Tuple(a.iter().zip(ts).map(|(x, t)| coerce(x, t)).collect::<Result<_, _>>()?)),
        (Value::Array(_), _) => Err(format!("expected {ty}, found array (or its length doesn't match)")),
        (Value::Object(_), _) => Err(format!("expected {ty}, found JSON object")),
        (Value::String(s), _) => match ty {
            DynSolType::String => Ok(DynSolValue::String(s.clone())),
            _ => parse_value(s, ty),
        },
        _ => parse_value(&v.to_string().replace('"', ""), ty),
    }
}

/// forge's encoding of what `vm.parseJson` selected: nothing, the one value, or an array of them, ABI-encoded.
pub fn encode(values: Vec<DynSolValue>) -> Vec<u8> {
    match values.len() {
        0 => vec![],
        1 => values.into_iter().next().unwrap().abi_encode(),
        _ => DynSolValue::Array(values).abi_encode(),
    }
}

/// A value as `vm.serialize*` writes it: numbers that fit 64 bits as numbers, larger ones as decimal strings, bytes
/// and addresses (checksummed) as hex strings, strings holding a JSON object as that object.
pub fn to_json(v: DynSolValue) -> Value {
    match v {
        DynSolValue::Bool(b) => Value::Bool(b),
        DynSolValue::String(s) => match serde_json::from_str::<Map<String, Value>>(&s) {
            Ok(m) => Value::Object(m),
            Err(_) => Value::String(s),
        },
        DynSolValue::Bytes(b) => Value::String(format!("0x{}", hex::encode(b))),
        DynSolValue::FixedBytes(b, n) => Value::String(format!("0x{}", hex::encode(&b[..n]))),
        DynSolValue::Int(i, _) => i64::try_from(i).map_or_else(|_| Value::String(i.to_string()), |n| Value::Number(n.into())),
        DynSolValue::Uint(u, _) => u64::try_from(u).map_or_else(|_| Value::String(u.to_string()), |n| Value::Number(n.into())),
        DynSolValue::Address(a) => Value::String(a.to_checksum(None)),
        DynSolValue::Array(a) | DynSolValue::FixedArray(a) | DynSolValue::Tuple(a) => Value::Array(a.into_iter().map(to_json).collect()),
        DynSolValue::Function(f) => Value::String(f.to_string()),
    }
}

/// `vm.writeJson(value, path, key)`: puts `value` (JSON, else a string) at the dot-separated `key` of `data`,
/// creating the objects on the way.
pub fn upsert(data: &mut Value, value: &str, key: &str) -> Result<(), String> {
    let key = key.strip_prefix("$.").or_else(|| key.strip_prefix('.')).unwrap_or(key);
    let parts: Vec<&str> = key.split('.').filter(|s| !s.is_empty()).collect();
    let Some((last, path)) = parts.split_last() else { return Err("'valueKey' cannot be empty or just '$'".into()) };
    let mut cur = data;
    for seg in path {
        cur = cur.as_object_mut().ok_or_else(|| format!("path segment '{seg}' does not resolve to an object."))?.entry(seg.to_string()).or_insert_with(|| Value::Object(Map::new()));
    }
    let obj = cur.as_object_mut().ok_or("final destination is not an object, cannot insert key.")?;
    obj.insert(last.to_string(), serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string())));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        let v = json!({"a": {"b": [10, 20, 30], "c d": "x"}, "z": [{"k": 1}, {"k": 2}]});
        let s = |p: &str| select(&v, p).unwrap().into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(s("$"), [v.clone()]);
        assert_eq!(s("."), [v.clone()]);
        assert_eq!(s(".a.b[1]"), [json!(20)]);
        assert_eq!(s("$.a.b[-1]"), [json!(30)]);
        assert_eq!(s("$['a']['c d']"), [json!("x")]);
        assert_eq!(s(".a.b[0,2]"), [json!(10), json!(30)]);
        assert_eq!(s(".a.b[1:]"), [json!(20), json!(30)]);
        assert_eq!(s(".z[*].k"), [json!(1), json!(2)]);
        assert_eq!(s("$..k"), [json!(1), json!(2)]);
        assert_eq!(s(".missing"), Vec::<Value>::new());
        assert!(select(&v, "$.z[?(@.k == 1)]").is_err());
    }

    #[test]
    fn decoding() {
        let defs = struct_defs(["contract T { struct Rule { uint256 value; uint256 attacker; } }"].into_iter());
        assert_eq!(defs["T.Rule"], ["value", "attacker"]);
        // object values in the order of the struct with the same members, else by key
        let t = |v: Value| token(&v, &defs).unwrap();
        assert_eq!(t(json!({"attacker": 1, "value": 2})), DynSolValue::Tuple(vec![DynSolValue::Uint(U256::from(2), 256), DynSolValue::Uint(U256::from(1), 256)]));
        assert_eq!(t(json!({"b": 1, "a": "x"})), DynSolValue::Tuple(vec![DynSolValue::String("x".into()), DynSolValue::Uint(U256::from(1), 256)]));
        assert_eq!(t(json!("0x0000000000000000000000000000000000000001")), DynSolValue::Address(Address::with_last_byte(1)));
        assert_eq!(t(json!("123")), DynSolValue::String("123".into()));
        // past 64 bits: forge tries int256 first, so a big positive string decodes as an int256 too
        assert_eq!(t(json!("123456789012345678901234567890")), DynSolValue::Int("123456789012345678901234567890".parse().unwrap(), 256));
        assert!(token(&json!("0x000000000000000000000000000000000000001"), &defs).is_err(), "39 hex digits");
        // typed: strings and numbers coerced
        assert_eq!(coerce(&json!("0x10"), &DynSolType::Uint(256)).unwrap(), DynSolValue::Uint(U256::from(16), 256));
        assert_eq!(coerce(&json!(7), &DynSolType::Uint(256)).unwrap(), DynSolValue::Uint(U256::from(7), 256));
        assert_eq!(coerce(&json!(["1", 2]), &DynSolType::Array(Box::new(DynSolType::Uint(256)))).unwrap(), DynSolValue::Array(vec![DynSolValue::Uint(U256::from(1), 256), DynSolValue::Uint(U256::from(2), 256)]));
        assert_eq!(coerce(&json!(true), &DynSolType::Bool).unwrap(), DynSolValue::Bool(true));
        assert!(coerce(&json!("ff"), &DynSolType::Uint(256)).unwrap_err().contains("missing hex prefix"));
        assert!(coerce(&json!({"a": 1}), &DynSolType::Uint(256)).is_err());
        // serialized: big numbers as strings, addresses checksummed, JSON strings as objects, keys in their order
        assert_eq!(to_json(DynSolValue::Uint(U256::MAX, 256)), json!(U256::MAX.to_string()));
        assert_eq!(to_json(DynSolValue::Address(Address::with_last_byte(0xab))), json!("0x00000000000000000000000000000000000000AB"));
        assert_eq!(serde_json::to_string(&to_json(DynSolValue::String(r#"{"z":1,"a":2}"#.into()))).unwrap(), r#"{"z":1,"a":2}"#);
        let mut data = json!({"keep": 1});
        upsert(&mut data, "5", ".a.b").unwrap();
        upsert(&mut data, "text", "c").unwrap();
        assert_eq!(serde_json::to_string(&data).unwrap(), r#"{"keep":1,"a":{"b":5},"c":"text"}"#);
    }
}
