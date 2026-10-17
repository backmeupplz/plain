//! JSON-RPC client and networks: built-in names, plain.toml/foundry.toml, `$<NAME>_RPC_URL`, and any
//! chain on chainlist.org by name or chain id. Public RPCs are raced and the first to answer is used.
use crate::project::{Project, expand_env};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// name, chain id, default RPC ("" = take RPCs from chainlist)
const NETWORKS: &[(&str, u64, &str)] = &[
    ("mainnet", 1, "https://ethereum-rpc.publicnode.com"),
    ("sepolia", 11155111, "https://ethereum-sepolia-rpc.publicnode.com"),
    ("hoodi", 560048, ""),
    ("base", 8453, "https://mainnet.base.org"),
    ("base-sepolia", 84532, "https://sepolia.base.org"),
    ("optimism", 10, "https://mainnet.optimism.io"),
    ("op", 10, "https://mainnet.optimism.io"),
    ("optimism-sepolia", 11155420, "https://sepolia.optimism.io"),
    ("arbitrum", 42161, "https://arb1.arbitrum.io/rpc"),
    ("arbitrum-sepolia", 421614, "https://sepolia-rollup.arbitrum.io/rpc"),
    ("polygon", 137, "https://polygon-rpc.com"),
    ("polygon-amoy", 80002, "https://rpc-amoy.polygon.technology"),
    ("bsc", 56, "https://bsc-dataseed.bnbchain.org"),
    ("avalanche", 43114, "https://api.avax.network/ext/bc/C/rpc"),
    ("gnosis", 100, "https://rpc.gnosischain.com"),
];

pub struct Net {
    pub url: String,
    /// chain id, when the network was resolved by name or id (and the RPC confirmed it)
    pub chain: Option<u64>,
}

/// Resolve a network name, chain id or URL to an RPC: `$<NAME>_RPC_URL`, plain.toml/foundry.toml, built-ins,
/// then chainlist.org. With several candidate RPCs, the first to answer with the right chain id wins.
pub fn network(p: &Project, name: &str) -> Result<Net, String> {
    if name.contains("://") {
        return Ok(Net { url: name.into(), chain: None });
    }
    let var = format!("{}_RPC_URL", name.to_uppercase().replace('-', "_"));
    if let Some(u) = std::env::var(&var).ok().or_else(|| p.networks.get(name).map(|u| expand_env(u))).filter(|u| !u.is_empty()) {
        return Ok(Net { url: u, chain: None });
    }
    if name == "local" {
        return Ok(Net { url: "http://127.0.0.1:8545".into(), chain: None });
    }
    let builtin = NETWORKS.iter().find(|n| n.0 == name);
    // built-ins don't wait for a chainlist download; a cached one only adds race candidates
    let list = chainlist(builtin.is_none_or(|b| b.2.is_empty()));
    let id = match builtin {
        Some(b) => b.1,
        None => find(&list, name)?.id,
    };
    // a built-in RPC when it answers, else the fastest of chainlist's
    if let Some(b) = builtin.filter(|b| !b.2.is_empty() && probe(b.2) == Ok(b.1)) {
        return Ok(Net { url: b.2.into(), chain: Some(id) });
    }
    let list = if list.is_empty() { chainlist(true) } else { list };
    let urls: Vec<String> = list.iter().filter(|c| c.id == id).flat_map(|c| c.rpcs.iter().take(8).cloned()).collect();
    let url = race(&urls, id).ok_or_else(|| format!("no public RPC for {name} (chain {id}) answered; set {var}"))?;
    Ok(Net { url, chain: Some(id) })
}

pub fn network_url(p: &Project, name: &str) -> Option<String> {
    network(p, name).map(|n| n.url).ok()
}

/// The first URL to answer eth_chainId with `id`.
fn race(urls: &[String], id: u64) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    for u in urls {
        let (tx, u) = (tx.clone(), u.clone());
        std::thread::spawn(move || {
            let r = probe(&u).ok();
            tx.send((u, r)).ok();
        });
    }
    drop(tx);
    rx.iter().find(|(_, r)| *r == Some(id)).map(|(u, _)| u)
}

fn probe(url: &str) -> Result<u64, String> {
    let mut r = agent().post(url).config().timeout_global(Some(Duration::from_secs(5))).build().send_json(json!({"jsonrpc": "2.0", "id": 0, "method": "eth_chainId", "params": []})).map_err(|e| e.to_string())?;
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    v["result"].as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).ok_or("no chain id".into())
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Chain {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    short: String,
    /// public RPCs that don't track users
    #[serde(default)]
    rpcs: Vec<String>,
    #[serde(default)]
    pub explorers: Vec<String>,
    #[serde(default)]
    pub testnet: bool,
}

/// chainlist.org's chain list, cached for a week in ~/.plain/chainlist.json. Without `download`, only the cache.
fn chainlist(download: bool) -> Vec<Chain> {
    let path = crate::solc::home().join("chainlist.json");
    let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    if download && age.is_none_or(|a| a.as_secs() > 7 * 86400) {
        let fetched = agent().get("https://chainlist.org/rpcs.json").call().map_err(|e| e.to_string()).and_then(|mut r| r.body_mut().with_config().limit(64 << 20).read_json::<Value>().map_err(|e| e.to_string()));
        match fetched {
            Ok(v) => {
                let s = |c: &Value, k: &str| c[k].as_str().unwrap_or_default().to_string();
                let list: Vec<Chain> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        let rpcs = c["rpc"].as_array().into_iter().flatten().filter(|r| r["tracking"] == "none").filter_map(|r| r["url"].as_str()).filter(|u| u.starts_with("https://") && !u.contains('$')).map(String::from).collect();
                        let explorers = c["explorers"].as_array().into_iter().flatten().filter_map(|e| e["url"].as_str()).map(|u| u.trim_end_matches('/').to_string()).collect();
                        Some(Chain { id: c["chainId"].as_u64()?, name: s(c, "name"), slug: s(c, "chainSlug"), short: s(c, "shortName"), rpcs, explorers, testnet: c["isTestnet"] == true })
                    })
                    .collect();
                std::fs::create_dir_all(path.parent().unwrap()).ok();
                std::fs::write(&path, serde_json::to_vec(&list).unwrap()).ok();
                return list;
            }
            Err(e) if age.is_none() => eprintln!("warning: chainlist.org: {e}"),
            Err(_) => {}
        }
    }
    std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// A chain's chainlist entry (explorers, name).
pub fn chain_info(id: u64) -> Option<Chain> {
    chainlist(true).into_iter().find(|c| c.id == id)
}

/// Chain by id, slug/short name, or name without "mainnet"/"testnet"/"network" ("base-sepolia" = "Base Sepolia Testnet").
fn find<'a>(list: &'a [Chain], q: &str) -> Result<&'a Chain, String> {
    let words = |s: &str| -> Vec<String> { s.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect() };
    let core = |s: &str| words(s).into_iter().filter(|w| !matches!(w.as_str(), "mainnet" | "testnet" | "network")).collect::<String>();
    let nq = words(q).concat();
    let hits: Vec<&Chain> = match q.parse::<u64>() {
        Ok(id) => list.iter().filter(|c| c.id == id).collect(),
        Err(_) => {
            let exact: Vec<&Chain> = list.iter().filter(|c| c.slug.eq_ignore_ascii_case(q) || words(&c.short).concat() == nq).collect();
            if exact.is_empty() { list.iter().filter(|c| core(&c.name) == core(q) || words(&c.name).concat() == nq).collect() } else { exact }
        }
    };
    // ambiguous names: chainlist's canonical entries carry a slug
    let mut hits = hits;
    hits.sort_by_key(|c| c.slug.is_empty());
    match hits.as_slice() {
        [] if list.is_empty() => Err(format!("unknown network {q:?} (chainlist.org unreachable)")),
        [] => Err(format!("unknown network {q:?}; use a name or chain id from chainlist.org, or a URL")),
        [one, ..] => Ok(*one),
    }
}

/// One shared agent so connections (and TLS sessions) are reused. Non-2xx responses are returned, not errors.
pub fn agent() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).http_status_as_error(false).build().into())
}

pub fn call(url: &str, method: &str, params: Value) -> Result<Value, String> {
    Ok(batch(url, &[(method, params)])?.remove(0))
}

/// JSON-RPC batch; results in request order, failing if any call fails.
pub fn batch(url: &str, calls: &[(&str, Value)]) -> Result<Vec<Value>, String> {
    batch_each(url, calls)?.into_iter().collect()
}

/// JSON-RPC batch with a result per call. Retries what load-balanced public RPCs fail on now and then
/// (a node behind on blocks, rate limits); falls back to one request per call if the server rejects batches.
pub fn batch_each(url: &str, calls: &[(&str, Value)]) -> Result<Vec<Result<Value, String>>, String> {
    let transient = |e: &str| ["429", "rate limit", "too many requests", "timed out", "timeout"].iter().any(|t| e.to_lowercase().contains(t));
    let mut tries = 0;
    loop {
        let r = batch_once(url, calls);
        let retry = match &r {
            Err(e) => transient(e),
            Ok(items) => items.iter().any(|x| x.as_ref().is_err_and(|e| transient(e))),
        };
        if !retry || tries == 4 {
            return r;
        }
        tries += 1;
        std::thread::sleep(Duration::from_millis(300 << tries));
    }
}

fn batch_once(url: &str, calls: &[(&str, Value)]) -> Result<Vec<Result<Value, String>>, String> {
    let body: Vec<Value> = calls.iter().enumerate().map(|(i, (m, p))| json!({"jsonrpc": "2.0", "id": i, "method": m, "params": p})).collect();
    let name = calls.iter().map(|c| c.0).collect::<Vec<_>>().join(",");
    let t = std::time::Instant::now();
    let _log = Defer(|| {
        if std::env::var_os("PLAIN_DEBUG").is_some() {
            eprintln!("rpc {name} {:.0?}", t.elapsed());
        }
    });
    let mut r = agent().post(url).send_json(if body.len() == 1 { body[0].clone() } else { Value::Array(body) }).map_err(|e| format!("{name}: {e}"))?;
    let status = r.status().as_u16();
    let text = r.body_mut().with_config().limit(256 << 20).read_to_string().map_err(|e| format!("{name}: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|_| format!("{name}: HTTP {status}: {}", text.chars().take(200).collect::<String>()))?;
    let mut items = match v {
        // every call failing over the batch itself (e.g. free plans capping batch size): one call at a time
        Value::Array(a) if a.len() == calls.len() && !(a.len() > 1 && a.iter().all(|x| x["error"]["message"].as_str().is_some_and(|m| m.to_lowercase().contains("batch")))) => a,
        // one error object back for a batch: the server doesn't take batches
        _ if calls.len() > 1 => return Ok(calls.iter().map(|(m, p)| call(url, m, p.clone())).collect()),
        one => vec![one],
    };
    items.sort_by_key(|x| x["id"].as_u64());
    Ok(items
        .into_iter()
        .zip(calls)
        .map(|(x, (m, _))| match x.get("error") {
            Some(e) => Err(format!("{m}: {}", e["message"].as_str().unwrap_or(&e.to_string()))),
            None => Ok(x["result"].clone()),
        })
        .collect())
}

struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        let c = |id, name: &str, slug: &str, short: &str| super::Chain { id, name: name.into(), slug: slug.into(), short: short.into(), rpcs: vec![], explorers: vec![], testnet: false };
        let list = vec![c(8453, "Base", "base", "base"), c(84532, "Base Sepolia Testnet", "", "basesep"), c(238, "Blast Mainnet", "", "blast"), c(81457, "Blast", "blast", "blastmainnet"), c(42161, "Arbitrum One", "arbitrum", "arb1")];
        let f = |q: &str| super::find(&list, q).map(|c| c.id).unwrap_or(0);
        assert_eq!(f("base"), 8453);
        assert_eq!(f("base-sepolia"), 84532);
        assert_eq!(f("Base Sepolia"), 84532);
        assert_eq!(f("blast"), 81457);
        assert_eq!(f("arb1"), 42161);
        assert_eq!(f("84532"), 84532);
        assert_eq!(f("nope"), 0);
    }
}
