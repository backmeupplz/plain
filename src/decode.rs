//! Human-readable decoding: revert reasons, custom errors, console.log, assertion failures.
use crate::solc::Artifact;
use alloy_dyn_abi::{DynSolType, DynSolValue, JsonAbiExt};
use alloy_json_abi::JsonAbi;
use alloy_primitives::{Bytes, U256, keccak256};
use std::collections::HashMap;
use std::sync::OnceLock;

pub struct Tables {
    /// (name, sorted selectors, creation code) for every contract with runtime code
    pub contracts: Vec<(String, Vec<[u8; 4]>, Vec<u8>)>,
    pub errors: HashMap<[u8; 4], alloy_json_abi::Error>,
    pub functions: HashMap<[u8; 4], alloy_json_abi::Function>,
    pub events: HashMap<alloy_primitives::B256, alloy_json_abi::Event>,
}

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn init(arts: &[Artifact]) {
    TABLES.get_or_init(|| {
        let mut t = Tables { contracts: vec![], errors: HashMap::new(), functions: HashMap::new(), events: HashMap::new() };
        for a in arts {
            let Ok(abi) = serde_json::from_value::<JsonAbi>(a.abi.clone()) else { continue };
            if !a.deployed.is_empty() {
                let mut sels: Vec<[u8; 4]> = abi.functions().map(|f| f.selector().0).collect();
                sels.sort();
                t.contracts.push((a.name.clone(), sels, hex::decode(&a.bytecode).unwrap_or_default()));
            }
            for e in abi.errors() {
                t.errors.insert(e.selector().0, e.clone());
            }
            for f in abi.functions() {
                t.functions.insert(f.selector().0, f.clone());
            }
            for e in abi.events() {
                t.events.insert(e.selector(), e.clone());
            }
        }
        t
    });
}

pub fn tables() -> Option<&'static Tables> {
    TABLES.get()
}

/// Name a deployed contract by matching its dispatcher's PUSH4 selectors against known ABIs.
/// (Exact bytecode comparison fails under via-IR, whose output depends on the compilation unit.)
pub fn contract_name(init: &[u8], runtime: &[u8]) -> Option<String> {
    let t = tables()?;
    if let Some((n, _, _)) = t.contracts.iter().filter(|(_, _, c)| !c.is_empty() && init.starts_with(c)).max_by_key(|(_, _, c)| c.len()) {
        return Some(n.clone());
    }
    let mut pushed = std::collections::HashSet::new();
    let mut i = 0;
    while i < runtime.len() {
        let op = runtime[i];
        if (0x60..=0x63).contains(&op) {
            // selectors with leading zero bytes are pushed with PUSH1..PUSH3
            let n = (op - 0x5f) as usize;
            if let Some(b) = runtime.get(i + 1..i + 1 + n) {
                let mut w = [0u8; 4];
                w[4 - n..].copy_from_slice(b);
                pushed.insert(w);
            }
        }
        i += if (0x60..=0x7f).contains(&op) { (op - 0x5f) as usize + 1 } else { 1 };
    }
    let hits: Vec<_> = t.contracts.iter().filter(|(_, sels, _)| !sels.is_empty() && sels.iter().all(|s| pushed.contains(s))).collect();
    let best = hits.iter().map(|(_, s, _)| s.len()).max()?;
    let top: Vec<_> = hits.iter().filter(|(_, s, _)| s.len() == best).collect();
    // identical ABIs are ambiguous: no name beats a wrong one
    (top.len() == 1).then(|| top[0].0.clone())
}

pub fn revert_msg(msg: &str) -> Bytes {
    let mut v = vec![0x08, 0xc3, 0x79, 0xa0];
    v.extend(DynSolValue::Tuple(vec![DynSolValue::String(msg.into())]).abi_encode_params());
    v.into()
}

pub fn error_string(out: &[u8]) -> Option<String> {
    if out.len() >= 4 && out[..4] == [0x08, 0xc3, 0x79, 0xa0] {
        if let Ok(DynSolValue::Tuple(v)) = DynSolType::Tuple(vec![DynSolType::String]).abi_decode_params(&out[4..]) {
            return v[0].as_str().map(String::from);
        }
    }
    None
}

fn panic_reason(code: U256) -> &'static str {
    match code.to::<u64>() {
        0x01 => "assertion failed",
        0x11 => "arithmetic underflow or overflow",
        0x12 => "division or modulo by zero",
        0x21 => "enum conversion out of range",
        0x22 => "invalid storage byte array encoding",
        0x31 => "pop on empty array",
        0x32 => "array out-of-bounds access",
        0x41 => "out of memory",
        0x51 => "call to zero-initialized function",
        _ => "unknown panic",
    }
}

/// Best-effort decode of revert data.
pub fn reason(out: &[u8]) -> String {
    if out.is_empty() {
        return "reverted without reason".into();
    }
    if let Some(s) = error_string(out) {
        return s;
    }
    if out.len() == 36 && out[..4] == [0x4e, 0x48, 0x7b, 0x71] {
        let code = U256::from_be_slice(&out[4..]);
        return format!("panic: {} (0x{code:x})", panic_reason(code));
    }
    if let Ok(s) = std::str::from_utf8(out) {
        if s.chars().all(|c| !c.is_control()) {
            return s.to_string();
        }
    }
    if out.len() >= 4 {
        if let Some(e) = tables().and_then(|t| t.errors.get(&<[u8; 4]>::try_from(&out[..4]).unwrap())) {
            if let Ok(vals) = e.abi_decode_input(&out[4..]) {
                return format!("{}({})", e.name, vals.iter().map(display).collect::<Vec<_>>().join(", "));
            }
            return e.name.clone();
        }
    }
    format!("custom error 0x{}", hex::encode(out))
}

/// Same as `reason`, used inside expectRevert mismatch messages.
pub fn reason_plain(out: &[u8]) -> String {
    reason(out)
}

pub fn describe_bytes(d: &[u8]) -> String {
    if d.len() == 4 {
        if let Some(e) = tables().and_then(|t| t.errors.get(&<[u8; 4]>::try_from(d).unwrap())) {
            return e.signature();
        }
    }
    reason(d)
}

pub fn display(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Address(a) => a.to_checksum(None),
        DynSolValue::Uint(u, _) => u.to_string(),
        DynSolValue::Int(i, _) => i.to_string(),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::String(s) => s.clone(),
        DynSolValue::FixedBytes(b, n) => format!("0x{}", hex::encode(&b[..*n])),
        DynSolValue::Bytes(b) => format!("0x{}", hex::encode(b)),
        DynSolValue::Array(a) | DynSolValue::FixedArray(a) => format!("[{}]", a.iter().map(display).collect::<Vec<_>>().join(", ")),
        DynSolValue::Tuple(a) => format!("({})", a.iter().map(display).collect::<Vec<_>>().join(", ")),
        other => format!("{other:?}"),
    }
}

fn num(v: &DynSolValue) -> Option<(bool, U256)> {
    // (negative, magnitude)
    match v {
        DynSolValue::Uint(u, _) => Some((false, *u)),
        DynSolValue::Int(i, _) => Some((i.is_negative(), i.unsigned_abs())),
        _ => None,
    }
}

fn cmp(a: &DynSolValue, b: &DynSolValue) -> Option<std::cmp::Ordering> {
    let (na, ma) = num(a)?;
    let (nb, mb) = num(b)?;
    Some(match (na, nb) {
        (false, false) => ma.cmp(&mb),
        (true, true) => mb.cmp(&ma),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
    })
}

fn delta(a: &DynSolValue, b: &DynSolValue) -> Option<U256> {
    let (na, ma) = num(a)?;
    let (nb, mb) = num(b)?;
    Some(if na == nb { if ma > mb { ma - mb } else { mb - ma } } else { ma.saturating_add(mb) })
}

/// Evaluate a vm.assertX cheatcode; Err carries the forge-style failure message.
pub fn assertion(name: &str, ty: &DynSolType, data: &[u8]) -> Result<(), String> {
    use std::cmp::Ordering::*;
    let Ok(DynSolValue::Tuple(v)) = ty.abi_decode_params(data) else { return Err("assertion failed".into()) };
    let base = name.trim_end_matches("Decimal");
    let ok = match base {
        "assertTrue" => v[0].as_bool() == Some(true),
        "assertFalse" => v[0].as_bool() == Some(false),
        "assertEq" => v[0] == v[1],
        "assertNotEq" => v[0] != v[1],
        "assertLt" => cmp(&v[0], &v[1]) == Some(Less),
        "assertGt" => cmp(&v[0], &v[1]) == Some(Greater),
        "assertLe" => cmp(&v[0], &v[1]) != Some(Greater),
        "assertGe" => cmp(&v[0], &v[1]) != Some(Less),
        "assertApproxEqAbs" => delta(&v[0], &v[1]).zip(num(&v[2])).is_some_and(|(d, (_, max))| d <= max),
        "assertApproxEqRel" => match (delta(&v[0], &v[1]), num(&v[1]), num(&v[2])) {
            (Some(d), Some((_, b)), Some((_, max))) => {
                if b.is_zero() { d.is_zero() } else { d.saturating_mul(U256::from(10).pow(U256::from(18))) / b <= max }
            }
            _ => false,
        },
        _ => false,
    };
    if ok {
        return Ok(());
    }
    let msg = match v.last() {
        Some(DynSolValue::String(s)) if v.len() > 1 || base == "assertTrue" || base == "assertFalse" => {
            // the last string is the message unless it is the right-hand operand of a 2-arg string compare
            if v.len() == 2 && matches!(v[0], DynSolValue::String(_)) { None } else { Some(s.clone()) }
        }
        _ => None,
    };
    let op = match base {
        "assertEq" => "!=",
        "assertNotEq" => "==",
        "assertLt" => ">=",
        "assertGt" => "<=",
        "assertLe" => ">",
        "assertGe" => "<",
        "assertApproxEqAbs" | "assertApproxEqRel" => "!~",
        _ => "",
    };
    let detail = if op.is_empty() || v.len() < 2 { String::new() } else { format!("{} {op} {}", display(&v[0]), display(&v[1])) };
    Err(match (msg, detail.is_empty()) {
        (Some(m), true) => m,
        (Some(m), false) => format!("{m}: {detail}"),
        (None, true) => "assertion failed".into(),
        (None, false) => format!("assertion failed: {detail}"),
    })
}

fn console_sigs() -> &'static HashMap<[u8; 4], DynSolType> {
    static T: OnceLock<HashMap<[u8; 4], DynSolType>> = OnceLock::new();
    T.get_or_init(|| {
        let base = ["uint256", "int256", "string", "bool", "address"];
        let mut sigs: Vec<Vec<String>> = vec![vec![]];
        let mut level: Vec<Vec<String>> = vec![vec![]];
        for _ in 0..4 {
            level = level.iter().flat_map(|p| base.iter().map(move |t| [p.clone(), vec![t.to_string()]].concat())).collect();
            sigs.extend(level.clone());
        }
        sigs.push(vec!["bytes".into()]);
        for n in 1..=32 {
            sigs.push(vec![format!("bytes{n}")]);
        }
        let mut out = HashMap::new();
        for s in sigs {
            let ty = DynSolType::parse(&format!("({})", s.join(","))).unwrap();
            let canon = format!("log({})", s.join(","));
            let short = canon.replace("uint256", "uint").replace("int256", "int");
            for sig in [canon, short] {
                out.insert(keccak256(sig.as_bytes())[..4].try_into().unwrap(), ty.clone());
            }
        }
        for (name, t) in [("logUint", "uint256"), ("logInt", "int256"), ("logString", "string"), ("logBool", "bool"), ("logAddress", "address"), ("logBytes", "bytes")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .chain((1..=32).map(|n| (format!("logBytes{n}"), format!("bytes{n}"))))
        {
            out.insert(keccak256(format!("{name}({t})").as_bytes())[..4].try_into().unwrap(), DynSolType::parse(&format!("({t})")).unwrap());
        }
        out
    })
}

pub fn console(data: &[u8]) -> String {
    if data.len() < 4 {
        return String::new();
    }
    let sel: [u8; 4] = data[..4].try_into().unwrap();
    let Some(ty) = console_sigs().get(&sel) else { return format!("<console 0x{}>", hex::encode(data)) };
    let Ok(DynSolValue::Tuple(vals)) = ty.abi_decode_params(&data[4..]) else { return "<bad console.log>".into() };
    let parts: Vec<String> = vals.iter().map(display).collect();
    // printf-style: first string arg with %s/%d/%i/%x substitutes the rest
    if let (Some(DynSolValue::String(fmt)), true) = (vals.first(), parts.len() > 1) {
        if fmt.contains('%') {
            let mut out = String::new();
            let mut rest = parts[1..].iter();
            let mut chars = fmt.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '%' && matches!(chars.peek(), Some('s' | 'd' | 'i' | 'x' | 'o' | 'e')) {
                    chars.next();
                    out.push_str(rest.next().map(String::as_str).unwrap_or(""));
                } else {
                    out.push(c);
                }
            }
            for r in rest {
                out.push(' ');
                out.push_str(r);
            }
            return out;
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn console_and_reasons() {
        let mut d = keccak256(b"log(string,uint256)")[..4].to_vec();
        d.extend(DynSolValue::Tuple(vec![DynSolValue::String("x = %s".into()), DynSolValue::Uint(U256::from(5), 256)]).abi_encode_params());
        assert_eq!(console(&d), "x = 5");
        assert_eq!(reason(&revert_msg("boom")), "boom");
        let mut p = vec![0x4e, 0x48, 0x7b, 0x71];
        p.extend(U256::from(0x11).to_be_bytes::<32>());
        assert!(reason(&p).contains("overflow"));
        let ty = DynSolType::parse("(uint256,uint256,string)").unwrap();
        let data = DynSolValue::Tuple(vec![DynSolValue::Uint(U256::from(1), 256), DynSolValue::Uint(U256::from(2), 256), DynSolValue::String("bal".into())]).abi_encode_params();
        assert_eq!(assertion("assertEq", &ty, &data), Err("bal: 1 != 2".into()));
        let ty = DynSolType::parse("(string,string)").unwrap();
        let data = DynSolValue::Tuple(vec![DynSolValue::String("a".into()), DynSolValue::String("a".into())]).abi_encode_params();
        assert_eq!(assertion("assertEq", &ty, &data), Ok(()));
    }
}
