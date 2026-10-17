//! JSON-RPC client + network table.
use crate::project::{Project, expand_env};
use serde_json::{Value, json};

const NETWORKS: &[(&str, &str)] = &[
    ("mainnet", "https://ethereum-rpc.publicnode.com"),
    ("sepolia", "https://ethereum-sepolia-rpc.publicnode.com"),
    ("base", "https://mainnet.base.org"),
    ("base-sepolia", "https://sepolia.base.org"),
    ("optimism", "https://mainnet.optimism.io"),
    ("optimism-sepolia", "https://sepolia.optimism.io"),
    ("arbitrum", "https://arb1.arbitrum.io/rpc"),
    ("arbitrum-sepolia", "https://sepolia-rollup.arbitrum.io/rpc"),
    ("polygon", "https://polygon-rpc.com"),
    ("polygon-amoy", "https://rpc-amoy.polygon.technology"),
    ("bsc", "https://bsc-dataseed.bnbchain.org"),
    ("avalanche", "https://api.avax.network/ext/bc/C/rpc"),
    ("gnosis", "https://rpc.gnosischain.com"),
    ("local", "http://127.0.0.1:8545"),
];

/// URL for a network name: $<NAME>_RPC_URL, then plain.toml/foundry.toml, then built-ins. URLs pass through.
pub fn network_url(p: &Project, name: &str) -> Option<String> {
    if name.contains("://") {
        return Some(name.to_string());
    }
    let key = format!("{}_RPC_URL", name.to_uppercase().replace('-', "_"));
    if let Ok(u) = std::env::var(key) {
        return Some(u);
    }
    if let Some(u) = p.networks.get(name).map(|u| expand_env(u)).filter(|u| !u.is_empty()) {
        return Some(u);
    }
    NETWORKS.iter().find(|(n, _)| *n == name).map(|(_, u)| u.to_string())
}

pub fn network_names(p: &Project) -> Vec<String> {
    let mut v: Vec<String> = NETWORKS.iter().map(|(n, _)| n.to_string()).chain(p.networks.keys().cloned()).collect();
    v.sort();
    v.dedup();
    v
}

/// One shared agent so connections (and TLS sessions) are reused.
fn agent() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(60))).build().into())
}

pub fn call(url: &str, method: &str, params: Value) -> Result<Value, String> {
    Ok(batch(url, &[(method, params)])?.remove(0))
}

/// JSON-RPC batch; results in request order.
pub fn batch(url: &str, calls: &[(&str, Value)]) -> Result<Vec<Value>, String> {
    let body: Vec<Value> = calls.iter().enumerate().map(|(i, (m, p))| json!({"jsonrpc": "2.0", "id": i, "method": m, "params": p})).collect();
    let name = calls.iter().map(|c| c.0).collect::<Vec<_>>().join(",");
    let t = std::time::Instant::now();
    let _log = Defer(|| {
        if std::env::var_os("PLAIN_DEBUG").is_some() {
            eprintln!("rpc {name} {:.0?}", t.elapsed());
        }
    });
    let mut r = agent().post(url).send_json(if body.len() == 1 { body[0].clone() } else { Value::Array(body) }).map_err(|e| format!("{name}: {e}"))?;
    let v: Value = r.body_mut().read_json().map_err(|e| format!("{name}: {e}"))?;
    let mut items = match v {
        Value::Array(a) => a,
        one => vec![one],
    };
    items.sort_by_key(|x| x["id"].as_u64());
    items
        .into_iter()
        .zip(calls)
        .map(|(x, (m, _))| match x.get("error") {
            Some(e) => Err(format!("{m}: {}", e["message"].as_str().unwrap_or(&e.to_string()))),
            None => Ok(x["result"].clone()),
        })
        .collect()
}

struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
