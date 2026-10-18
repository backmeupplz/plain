//! Upgradeable contracts behind OpenZeppelin proxies: `plain deploy X --proxy` puts one up, and `plain upgrade`
//! points it at a new implementation once that implementation is shown to keep the storage the proxy holds.
use crate::deploy::Conn;
use crate::project::Project;
use crate::solc::Artifact;
use crate::verify::Record;
use crate::{die, rpc};
use alloy_primitives::{Address, B256, U256, b256};
use serde_json::{Value, json};

/// ERC-1967 storage slots: the implementation, and a transparent proxy's admin (its ProxyAdmin contract)
const IMPLEMENTATION: B256 = b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
const ADMIN: B256 = b256!("b53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103");

/// OpenZeppelin's ERC1967Proxy (for UUPS implementations) or TransparentUpgradeableProxy with the ProxyAdmin it
/// creates, compiled from the project's own @openzeppelin/contracts with the build's solc and settings.
pub fn proxy_artifacts(p: &Project, version: &str, uups: bool) -> Vec<Artifact> {
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
            arts.push(Artifact { name: name.clone(), file: file.clone(), abi: c["abi"].clone(), bytecode: code("bytecode"), deployed: code("deployedBytecode"), links: Value::Null, shard: 0, tests: vec![], deploys: vec![], rewritten: false });
        }
    }
    arts
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
    let word = |v: &Value| U256::from_str_radix(v.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).ok().map(|w| Address::from_word(w.into())).filter(|a| !a.is_zero());
    let current = word(&r[0]).unwrap_or_else(|| die(&format!("{name} ({addr}) isn't an ERC-1967 proxy: its implementation slot is empty")));
    let old = crate::verify::load(p, conn.chain).remove(&current);
    let target = target.or_else(|| old.as_ref().map(|r| r.contract.clone()));
    let target = target.unwrap_or_else(|| die(&format!("{name} runs {current}, which plain didn't deploy from this project; name the contract to upgrade to")));
    Plan { proxy: addr, name, admin: word(&r[1]), target, call, current, old }
}

/// Stops the upgrade unless `new` keeps every variable the proxy's storage already holds, in the same slot with
/// a compatible type. It may rename them, add variables after them, or take new ones out of a `__gap`.
// ponytail: compares solc's storageLayout, which leaves out ERC-7201 namespaced structs (OpenZeppelin 5's own
// state); comparing those needs the AST, as OpenZeppelin's upgrades-core does
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
    match compare(&a, &b) {
        Ok(notes) => println!("storage layout: {} can take over {}'s storage{}", new.name, old.name, notes.iter().map(|n| format!("; {n}")).collect::<String>()),
        Err(errs) => die(&format!("{} can't take over {}'s storage:\n  {}\nfix the order of the variables, or --unsafe-skip-storage-check if this is intended", new.name, old.name, errs.join("\n  "))),
    }
}

fn layout(p: &Project, compiler: &str, mut input: Value, file: &str, name: &str) -> Value {
    // nothing but the layout: solc stops after analysis
    input["settings"]["outputSelection"] = json!({});
    input["settings"]["outputSelection"][file][name] = json!(["storageLayout"]);
    let out = crate::solc::standard_json(compiler, &input, &p.root).unwrap_or_else(|e| die(&e));
    out["contracts"][file][name]["storageLayout"].clone()
}

/// What changed from storage layout `old` to `new`: notes when the data stays readable, else what breaks.
fn compare(old: &Value, new: &Value) -> Result<Vec<String>, Vec<String>> {
    let vars = |l: &Value| l["storage"].as_array().cloned().unwrap_or_default();
    let (ov, nv) = (vars(old), vars(new));
    let label = |v: &Value| v["label"].as_str().unwrap_or("?").to_string();
    let gap = |v: &Value| label(v).starts_with("__gap");
    let pos = |v: &Value| (v["slot"].as_str().and_then(|s| s.parse::<U256>().ok()).unwrap_or_default(), v["offset"].as_u64().unwrap_or(0));
    let ty = |l: &Value, v: &Value| l["types"][v["type"].as_str().unwrap_or_default()]["label"].as_str().unwrap_or("?").to_string();
    let last = ov.iter().map(pos).max();
    let (mut notes, mut errs) = (vec![], vec![]);
    for o in ov.iter().filter(|o| !gap(o)) {
        let (name, (slot, _)) = (label(o), pos(o));
        if let Some(m) = nv.iter().find(|n| label(n) == name && pos(n) != pos(o)) {
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
    let added: Vec<String> = nv.iter().filter(|n| !gap(n) && !ov.iter().any(|o| !gap(o) && (label(o) == label(n) || pos(o) == pos(n)))).map(|n| format!("`{}`", label(n))).collect();
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
    let kind = |t: &Value| match s(t, "label") {
        l if l.starts_with("contract ") || l.starts_with("address") => "address".to_string(),
        l if l.starts_with("enum ") => "enum".to_string(),
        l => l,
    };
    if s(a, "encoding") != s(b, "encoding") {
        return false;
    }
    match s(a, "encoding").as_str() {
        "mapping" => same(ol, &s(a, "key"), nl, &s(b, "key"), false) && same(ol, &s(a, "value"), nl, &s(b, "value"), true),
        "dynamic_array" => same(ol, &s(a, "base"), nl, &s(b, "base"), true),
        "bytes" => kind(a) == kind(b),
        _ if !(size(a) == size(b) || grow && size(b) > size(a)) => false,
        // a struct may get members at its end
        _ if a["members"].is_array() => {
            let (am, bm) = (a["members"].as_array().unwrap(), b["members"].as_array().cloned().unwrap_or_default());
            am.len() <= bm.len() && am.iter().zip(&bm).all(|(x, y)| x["slot"] == y["slot"] && x["offset"] == y["offset"] && same(ol, &s(x, "type"), nl, &s(y, "type"), false))
        }
        _ if a["base"].is_string() => same(ol, &s(a, "base"), nl, &s(b, "base"), false),
        _ => kind(a) == kind(b),
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
}
