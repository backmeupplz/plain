//! Upgradeable contracts behind OpenZeppelin proxies: `plain deploy X --proxy` puts one up, and `plain upgrade`
//! points it at a new implementation once that implementation is shown to keep the storage the proxy holds.
use crate::deploy::Conn;
use crate::project::Project;
use crate::solc::Artifact;
use crate::verify::Record;
use crate::{die, rpc};
use alloy_primitives::{Address, B256, U256, b256};
use std::collections::HashMap;
use serde_json::{Value, json};

/// ERC-1967 storage slots: the implementation, and a transparent proxy's admin (its ProxyAdmin contract)
pub const IMPLEMENTATION: B256 = b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
const ADMIN: B256 = b256!("b53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103");

/// OpenZeppelin's ERC1967Proxy (for UUPS implementations) or TransparentUpgradeableProxy with the ProxyAdmin it
/// creates, compiled from the project's own @openzeppelin/contracts with the build's solc and settings: the proxy,
/// and everything that compile made (to name and verify what the proxy creates).
pub fn proxy_artifacts(p: &Project, version: &str, uups: bool) -> (Artifact, Vec<Artifact>) {
    let path = match uups {
        true => "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol",
        false => "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol",
    };
    let unit = p.resolve_import("", path);
    if p.locate(&unit).is_none() {
        die(&format!("--proxy deploys OpenZeppelin's {path}, which isn't installed: npm i @openzeppelin/contracts (or forge install OpenZeppelin/openzeppelin-contracts)"));
    }
    let out = crate::solc::standard_json(version, &crate::verify::input(p, &unit), &p.root).unwrap_or_else(|e| die(&format!("compiling {path}:\n{e}")));
    let mut arts = vec![];
    for (file, cs) in out["contracts"].as_object().into_iter().flatten() {
        for (name, c) in cs.as_object().into_iter().flatten() {
            let code = |k: &str| c["evm"][k]["object"].as_str().unwrap_or_default().to_string();
            arts.push(Artifact { name: name.clone(), file: file.clone(), abi: c["abi"].clone(), bytecode: code("bytecode"), deployed: code("deployedBytecode"), links: Value::Null, deployed_links: Value::Null, immutables: Value::Null, shard: 0, tests: vec![], deploys: vec![], rewritten: false, profile: 0, methods: Value::Null });
        }
    }
    let name = if uups { "ERC1967Proxy" } else { "TransparentUpgradeableProxy" };
    let proxy = arts.iter().find(|a| a.file == unit && a.name == name).cloned().unwrap_or_else(|| die(&format!("{path} has no {name}")));
    // OpenZeppelin 4's transparent proxy takes an admin and makes that account unable to use the contract; 5 takes
    // the owner of a ProxyAdmin it creates
    let ctor = proxy.abi.as_array().into_iter().flatten().find(|x| x["type"] == "constructor").cloned().unwrap_or_default();
    if !uups && ctor["inputs"][1]["name"] != "initialOwner" {
        die("this @openzeppelin/contracts is older than 5.0, whose TransparentUpgradeableProxy makes the deploying account its admin (and unable to use the contract); upgrade OpenZeppelin, or make the contract UUPSUpgradeable");
    }
    (proxy, arts)
}

pub struct Plan {
    pub proxy: Address,
    /// its name in deployments/; the new implementation is saved there as <name>_Implementation
    pub name: String,
    /// a transparent proxy's ProxyAdmin; none for UUPS, where the proxy upgrades itself
    pub admin: Option<Address>,
    /// the contract to upgrade to
    pub target: String,
    /// a function of the new implementation to call through the proxy as part of the upgrade
    pub call: Option<String>,
    current: Address,
    /// what plain recorded when it deployed the current implementation
    old: Option<Record>,
}

/// Finds the proxy (an address, or a name in deployments/<network>.json) and what it runs now. Without
/// `target`, the upgrade goes to the contract the current implementation was built from, as it is now.
pub fn plan(p: &Project, conn: &Conn, proxy: &str, target: Option<String>, call: Option<String>) -> Plan {
    let book = crate::deploy::addresses(p, conn);
    let (addr, name) = match proxy.parse::<Address>() {
        Ok(a) => (a, book.iter().find(|(_, v)| v.parse() == Ok(a)).map_or(a.to_string(), |(k, _)| k.clone())),
        Err(_) => match book.get(proxy).and_then(|a| a.parse().ok()) {
            Some(a) => (a, proxy.to_string()),
            None => die(&format!("no {proxy:?} in {}; pass the proxy's address instead", crate::deploy::addresses_path(p, conn).display())),
        },
    };
    let r = rpc::batch(&conn.url, &[("eth_getStorageAt", json!([addr, IMPLEMENTATION, "latest"])), ("eth_getStorageAt", json!([addr, ADMIN, "latest"]))]).unwrap_or_else(|e| die(&e));
    let word = |v: &Value| Some(Address::from_word(rpc::quantity(v).unwrap_or_else(|e| die(&format!("{name}'s ERC-1967 slots: {e}"))).into())).filter(|a| !a.is_zero());
    let current = word(&r[0]).unwrap_or_else(|| die(&format!("{name} ({addr}) isn't an ERC-1967 proxy: its implementation slot is empty")));
    let old = crate::verify::load(p, conn.chain).remove(&current);
    let target = target.or_else(|| old.as_ref().map(|r| r.contract.clone()));
    let target = target.unwrap_or_else(|| die(&format!("{name} runs {current}, which plain didn't deploy from this project; name the contract to upgrade to")));
    let admin = word(&r[1]);
    Plan { proxy: addr, name, admin, target, call, current, old }
}

/// Stops the upgrade unless `new` keeps every variable the proxy's storage already holds, in the same slot with
/// a compatible type. It may rename them, add variables after them, or take new ones out of a `__gap`. The same goes
/// for each ERC-7201 namespace (`@custom:storage-location erc7201:<id>` structs, OpenZeppelin 5's own state too).
pub fn check(p: &Project, plan: &Plan, new: &Artifact, version: &str) {
    let Some(old) = &plan.old else {
        die(&format!("{} wasn't deployed by plain from this project, so there's no storage layout to compare with; --unsafe-skip-storage-check upgrades anyway", plan.current));
    };
    let input = crate::verify::input(p, &new.file);
    if old.contract == format!("{}:{}", new.file, new.name) && crate::verify::input_hash(&input) == old.input {
        die(&format!("{} hasn't changed since {} was deployed, so there's nothing to upgrade to", new.name, plan.current));
    }
    let saved = std::fs::read(p.root.join(".plain/deploys/inputs").join(format!("{}.json", old.input))).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let saved = saved.unwrap_or_else(|| die(&format!("the sources {} was built from are missing from .plain/deploys/inputs; --unsafe-skip-storage-check upgrades anyway", plan.current)));
    let (of, on) = old.contract.rsplit_once(':').unwrap_or_default();
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| layout(p, &old.compiler, saved, of, on));
        (a.join().unwrap(), layout(p, version, input, &new.file, &new.name))
    });
    let (mut notes, mut errs) = match compare(&a, &b) {
        Ok(n) => (n, vec![]),
        Err(e) => (vec![], e),
    };
    let spaces = a["namespaces"].as_object().cloned().unwrap_or_default();
    for (id, old_ns) in &spaces {
        match b["namespaces"].get(id) {
            None => errs.push(format!("ERC-7201 namespace {id} is gone (its data stays in storage, unread)")),
            Some(new_ns) => match compare(old_ns, new_ns) {
                Ok(n) => notes.extend(n.into_iter().map(|n| format!("{id}: {n}"))),
                Err(e) => errs.extend(e.into_iter().map(|e| format!("{id}: {e}"))),
            },
        }
    }
    if !spaces.is_empty() {
        notes.push(format!("{} ERC-7201 namespace(s) checked", spaces.len()));
    }
    match errs.is_empty() {
        true => println!("storage layout: {} can take over {}'s storage{}", new.name, old.name, notes.iter().map(|n| format!("; {n}")).collect::<String>()),
        false => die(&format!("{} can't take over {}'s storage:\n  {}\nfix the order of the variables, or --unsafe-skip-storage-check if this is intended", new.name, old.name, errs.join("\n  "))),
    }
}

/// Every node of a solc AST.
fn walk(node: &Value, f: &mut dyn FnMut(&Value)) {
    match node {
        Value::Object(o) => {
            f(node);
            o.values().for_each(|v| walk(v, f));
        }
        Value::Array(a) => a.iter().for_each(|v| walk(v, f)),
        _ => {}
    }
}

/// What the layout's type ids don't say, from the AST: the type a user-defined value type wraps, and enum members.
fn annotate(layout: &mut Value, out: &Value) {
    let (mut udvt, mut enums) = (HashMap::new(), HashMap::new());
    for src in out["sources"].as_object().into_iter().flatten().map(|(_, s)| &s["ast"]) {
        walk(src, &mut |n| match n["nodeType"].as_str() {
            Some("UserDefinedValueTypeDefinition") => {
                udvt.insert(n["id"].to_string(), n["underlyingType"]["typeDescriptions"]["typeString"].clone());
            }
            Some("EnumDefinition") => {
                enums.insert(n["id"].to_string(), json!(n["members"].as_array().into_iter().flatten().map(|m| m["name"].clone()).collect::<Vec<_>>()));
            }
            _ => {}
        });
    }
    // type ids end in the defining node's id: "t_enum(Status)12", "t_userDefinedValueType(Price)4"
    let ids: Vec<String> = layout["types"].as_object().into_iter().flatten().map(|(k, _)| k.clone()).collect();
    for t in ids {
        let node = t.rsplit(')').next().unwrap_or_default().to_string();
        if let Some(u) = udvt.get(&node) {
            layout["udvt"][&t] = u.clone();
        }
        if let Some(m) = enums.get(&node) {
            layout["enums"][&t] = m.clone();
        }
    }
}

/// A contract's storage layout (with what `annotate` adds), and under "namespaces" the layout of each ERC-7201
/// namespace in its sources, keyed by its id.
fn layout(p: &Project, compiler: &str, mut input: Value, file: &str, name: &str) -> Value {
    // nothing but the layout and the AST: solc stops after analysis
    input["settings"]["outputSelection"] = json!({"*": {"": ["ast"]}});
    input["settings"]["outputSelection"][file][name] = json!(["storageLayout"]);
    let out = crate::solc::standard_json(compiler, &input, &p.root).unwrap_or_else(|e| die(&e));
    let mut l = out["contracts"][file][name]["storageLayout"].clone();
    annotate(&mut l, &out);
    // namespaced structs aren't variables, so solc lays them out only for a contract that declares one: a helper
    // contract declares each, and its layout gives the struct's members
    let mut spaces: Vec<(String, String, String)> = vec![];
    for (unit, src) in out["sources"].as_object().into_iter().flatten() {
        walk(&src["ast"], &mut |n| {
            let doc = n["documentation"].as_str().or(n["documentation"]["text"].as_str()).unwrap_or_default();
            if let (Some("StructDefinition"), Some(id)) = (n["nodeType"].as_str(), doc.split("erc7201:").nth(1).and_then(|r| r.split_whitespace().next())) {
                spaces.push((id.to_string(), unit.clone(), n["canonicalName"].as_str().unwrap_or_default().to_string()));
            }
        });
    }
    if spaces.is_empty() {
        return l;
    }
    let mut helper = String::from("// SPDX-License-Identifier: UNLICENSED\n");
    for (i, (_, unit, _)) in spaces.iter().enumerate() {
        // relative, so no remapping applies: "__plain/../<unit>" is the unit itself
        helper += &format!("import \"../{unit}\" as __N{i};\n");
    }
    helper += "contract __PlainNamespaces {\n";
    for (i, (_, _, struct_)) in spaces.iter().enumerate() {
        helper += &format!("    __N{i}.{struct_} _{i};\n");
    }
    helper += "}\n";
    input["sources"]["__plain/namespaces.sol"] = json!({"content": helper});
    input["settings"]["outputSelection"] = json!({"*": {"": ["ast"]}, "__plain/namespaces.sol": {"__PlainNamespaces": ["storageLayout"]}});
    let out = crate::solc::standard_json(compiler, &input, &p.root).unwrap_or_else(|e| die(&format!("laying out the ERC-7201 namespaces failed ({e}); --unsafe-skip-storage-check upgrades without that check")));
    let mut held = out["contracts"]["__plain/namespaces.sol"]["__PlainNamespaces"]["storageLayout"].clone();
    annotate(&mut held, &out);
    for (i, (id, _, _)) in spaces.iter().enumerate() {
        let Some(var) = held["storage"].as_array().and_then(|v| v.iter().find(|x| x["label"] == format!("_{i}"))) else { continue };
        let members = held["types"][var["type"].as_str().unwrap_or_default()]["members"].clone();
        // the namespace as if its members were the variables of a contract of their own
        let mut ns = json!({"storage": members, "types": held["types"].clone()});
        for k in ["udvt", "enums"] {
            ns[k] = held[k].clone();
        }
        l["namespaces"][id] = ns;
    }
    l
}

/// What changed from storage layout `old` to `new`: notes when the data stays readable, else what breaks.
fn compare(old: &Value, new: &Value) -> Result<Vec<String>, Vec<String>> {
    let vars = |l: &Value| l["storage"].as_array().cloned().unwrap_or_default();
    let (ov, nv) = (vars(old), vars(new));
    let label = |v: &Value| v["label"].as_str().unwrap_or("?").to_string();
    // the same variable: the same name, and when that name is declared more than once (private variables in
    // different bases may share one), the same declaring contract too
    let twice = |l: &[Value], name: &str| l.iter().filter(|v| label(v) == name).count() > 1;
    let same_var = |a: &Value, b: &Value| label(a) == label(b) && (a["contract"] == b["contract"] || !(twice(&ov, &label(a)) || twice(&nv, &label(b))));
    let gap = |v: &Value| label(v).starts_with("__gap");
    let pos = |v: &Value| (v["slot"].as_str().and_then(|s| s.parse::<U256>().ok()).unwrap_or_default(), v["offset"].as_u64().unwrap_or(0));
    // a type as the error shows it: with what a user-defined value type wraps, and an enum's members
    let ty = |l: &Value, v: &Value| {
        let id = v["type"].as_str().unwrap_or_default();
        let label = l["types"][id]["label"].as_str().unwrap_or("?").to_string();
        match (l["udvt"][id].as_str(), l["enums"][id].as_array()) {
            (Some(u), _) => format!("{label} ({u})"),
            (_, Some(m)) => format!("{label} {{{}}}", m.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")),
            _ => label,
        }
    };
    let last = ov.iter().map(pos).max();
    let (mut notes, mut errs) = (vec![], vec![]);
    for o in ov.iter().filter(|o| !gap(o)) {
        let (name, (slot, _)) = (label(o), pos(o));
        if let Some(m) = nv.iter().find(|n| same_var(n, o) && pos(n) != pos(o)) {
            errs.push(format!("`{name}` moved from slot {slot} to slot {} (was something added before it?)", pos(m).0));
            continue;
        }
        match nv.iter().find(|n| pos(n) == pos(o)) {
            None => errs.push(format!("`{name}` (slot {slot}) is gone")),
            Some(n) if !same(old, o["type"].as_str().unwrap_or_default(), new, n["type"].as_str().unwrap_or_default(), Some(pos(o)) == last) => {
                errs.push(format!("`{name}` (slot {slot}) changed type from {} to {}", ty(old, o), ty(new, n)))
            }
            Some(n) if label(n) != name => notes.push(format!("`{name}` is now called `{}`", label(n))),
            Some(_) => {}
        }
    }
    let added: Vec<String> = nv.iter().filter(|n| !gap(n) && !ov.iter().any(|o| !gap(o) && (same_var(o, n) || pos(o) == pos(n)))).map(|n| format!("`{}`", label(n))).collect();
    if !added.is_empty() {
        notes.push(format!("adds {}", added.join(", ")));
    }
    if errs.is_empty() { Ok(notes) } else { Err(errs) }
}

/// Whether data stored as type `o` (in layout `ol`) reads the same as type `n` (in `nl`). `grow`: the value may
/// get longer at its end (the last variable, mapping values, dynamic array elements).
fn same(ol: &Value, o: &str, nl: &Value, n: &str, grow: bool) -> bool {
    let (a, b) = (&ol["types"][o], &nl["types"][n]);
    let s = |t: &Value, k: &str| t[k].as_str().unwrap_or_default().to_string();
    let size = |t: &Value| s(t, "numberOfBytes").parse::<u64>().unwrap_or(0);
    // contracts and interfaces are addresses and enums small integers, whatever they're called
    // a user-defined value type is what it wraps, under any name
    let kind = |l: &Value, id: &str, t: &Value| match (l["udvt"][id].as_str(), s(t, "label")) {
        (Some(u), _) => u.to_string(),
        (_, l) if l.starts_with("contract ") || l.starts_with("address") => "address".to_string(),
        (_, l) if l.starts_with("enum ") => "enum".to_string(),
        (_, l) => l,
    };
    // an enum's stored number means the same member only while members are added at the end
    let members = |l: &Value, id: &str| l["enums"][id].as_array().cloned();
    let enum_kept = match (members(ol, o), members(nl, n)) {
        (Some(x), Some(y)) => x.len() <= y.len() && x.iter().zip(&y).all(|(a, b)| a == b),
        _ => true,
    };
    if s(a, "encoding") != s(b, "encoding") {
        return false;
    }
    match s(a, "encoding").as_str() {
        "mapping" => same(ol, &s(a, "key"), nl, &s(b, "key"), false) && same(ol, &s(a, "value"), nl, &s(b, "value"), true),
        // array elements sit one after another: one that grows moves every element after the first
        "dynamic_array" => same(ol, &s(a, "base"), nl, &s(b, "base"), false),
        "bytes" => kind(ol, o, a) == kind(nl, n, b),
        _ if !(size(a) == size(b) || grow && size(b) > size(a)) => false,
        // a struct may get members at its end; a member found under its name elsewhere has moved
        _ if a["members"].is_array() => {
            let (am, bm) = (a["members"].as_array().unwrap(), b["members"].as_array().cloned().unwrap_or_default());
            let at = |m: &Value| (m["slot"].clone(), m["offset"].clone());
            am.len() <= bm.len()
                && am.iter().zip(&bm).all(|(x, y)| at(x) == at(y) && same(ol, &s(x, "type"), nl, &s(y, "type"), false))
                && am.iter().all(|x| bm.iter().filter(|y| y["label"] == x["label"]).all(|y| at(y) == at(x)))
        }
        _ if a["base"].is_string() => same(ol, &s(a, "base"), nl, &s(b, "base"), false),
        _ => kind(ol, o, a) == kind(nl, n, b) && enum_kept,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    fn layout(vars: &[(&str, u64, &str)]) -> serde_json::Value {
        let types = json!({
            "t_uint256": {"encoding": "inplace", "label": "uint256", "numberOfBytes": "32"},
            "t_address": {"encoding": "inplace", "label": "address", "numberOfBytes": "20"},
            "t_contract(IERC20)12": {"encoding": "inplace", "label": "contract IERC20", "numberOfBytes": "20"},
            "t_array(t_uint256)49_storage": {"encoding": "inplace", "label": "uint256[49]", "numberOfBytes": "1568", "base": "t_uint256"},
            "t_array(t_uint256)48_storage": {"encoding": "inplace", "label": "uint256[48]", "numberOfBytes": "1536", "base": "t_uint256"},
            "t_mapping(t_address,t_uint256)": {"encoding": "mapping", "label": "mapping(address => uint256)", "numberOfBytes": "32", "key": "t_address", "value": "t_uint256"},
        });
        let storage: Vec<_> = vars.iter().map(|(l, slot, t)| json!({"label": l, "slot": slot.to_string(), "offset": 0, "type": t})).collect();
        json!({"storage": storage, "types": types})
    }

    #[test]
    fn layouts() {
        let v1 = layout(&[("owner", 0, "t_address"), ("balances", 1, "t_mapping(t_address,t_uint256)"), ("__gap", 2, "t_array(t_uint256)49_storage"), ("total", 51, "t_uint256")]);
        let ok = |l| super::compare(&v1, &l).is_ok();
        assert!(ok(v1.clone()));
        // appended, renamed, out of the gap, an address that's now an interface
        assert!(ok(layout(&[("admin", 0, "t_contract(IERC20)12"), ("balances", 1, "t_mapping(t_address,t_uint256)"), ("fee", 2, "t_uint256"), ("__gap", 3, "t_array(t_uint256)48_storage"), ("total", 51, "t_uint256"), ("extra", 52, "t_uint256")])));
        // inserted before, retyped, dropped
        assert!(!ok(layout(&[("fee", 0, "t_uint256"), ("owner", 1, "t_address"), ("__gap", 2, "t_array(t_uint256)49_storage"), ("total", 51, "t_uint256")])));
        assert!(!ok(layout(&[("owner", 0, "t_uint256"), ("balances", 1, "t_mapping(t_address,t_uint256)"), ("__gap", 2, "t_array(t_uint256)49_storage"), ("total", 51, "t_uint256")])));
        assert!(!ok(layout(&[("owner", 0, "t_address"), ("balances", 1, "t_mapping(t_address,t_uint256)")])));
    }

    fn with_contract(mut l: serde_json::Value, contracts: &[&str]) -> serde_json::Value {
        for (v, c) in l["storage"].as_array_mut().unwrap().iter_mut().zip(contracts) {
            v["contract"] = json!(c);
        }
        l
    }

    #[test]
    fn renamed_contract_and_shared_names() {
        // Box -> BoxV2 with a variable inserted before `value`: a move, not a rename
        let old = with_contract(layout(&[("value", 0, "t_uint256")]), &["src/Box.sol:Box"]);
        let new = with_contract(layout(&[("bonus", 0, "t_uint256"), ("value", 1, "t_uint256")]), &["src/BoxV2.sol:BoxV2", "src/BoxV2.sol:BoxV2"]);
        assert!(super::compare(&old, &new).is_err());
        // two bases each with a private `x`: unchanged layout is fine
        let two = with_contract(layout(&[("x", 0, "t_uint256"), ("x", 1, "t_uint256")]), &["src/A.sol:A", "src/B.sol:B"]);
        assert!(super::compare(&two, &two).is_ok());
    }

    #[test]
    fn struct_members() {
        let s = |names: [&str; 2]| {
            let mut l = layout(&[("pos", 0, "t_struct(P)1_storage")]);
            l["types"]["t_struct(P)1_storage"] = json!({"encoding": "inplace", "label": "struct P", "numberOfBytes": "64", "members": [
                {"label": names[0], "slot": "0", "offset": 0, "type": "t_uint256"}, {"label": names[1], "slot": "1", "offset": 0, "type": "t_uint256"}]});
            l
        };
        assert!(super::compare(&s(["a", "b"]), &s(["a", "renamed"])).is_ok());
        assert!(super::compare(&s(["a", "b"]), &s(["b", "a"])).is_err());
    }
}
