//! JSON-RPC client and networks: built-in names, plain.toml/foundry.toml, `$<NAME>_RPC_URL`, and any
//! chain on chainlist.org by name or chain id. Public RPCs are raced and the first to answer is used.
use crate::project::{Project, expand_env};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// name, chain id, default RPC ("" = take RPCs from chainlist)
pub const NETWORKS: &[(&str, u64, &str)] = &[
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

/// A built-in network's name for a chain id ("base-sepolia" for 84532).
pub fn name_of(chain: u64) -> Option<&'static str> {
    NETWORKS.iter().find(|n| n.1 == chain).map(|n| n.0)
}

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
    if let Some(u) = crate::project::var(&var).or_else(|| p.networks.get(name).map(|u| expand_env(u))).filter(|u| !u.is_empty()) {
        return Ok(Net { url: u, chain: None });
    }
    if name == "local" {
        return Ok(Net { url: "http://127.0.0.1:8545".into(), chain: None });
    }
    // by name, or by chain id ("8453" uses Base's own RPC too)
    let builtin = NETWORKS.iter().find(|n| (n.0 == name || name.parse() == Ok(n.1)) && !n.2.is_empty()).or_else(|| NETWORKS.iter().find(|n| n.0 == name));
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

/// An RPC URL as plain shows it: scheme and host, since the rest often holds an API key
/// ("https://base-mainnet.g.alchemy.com/v2/<key>" shows as "https://base-mainnet.g.alchemy.com/…").
pub fn shown(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or(host);
    let cut = if rest.trim_end_matches('/').len() > host.len() { "/…" } else { "" };
    if scheme.is_empty() { format!("{host}{cut}") } else { format!("{scheme}://{host}{cut}") }
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
    /// the native token ("ETH", "POL")
    #[serde(default)]
    pub symbol: String,
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
                        Some(Chain { id: c["chainId"].as_u64()?, name: s(c, "name"), slug: s(c, "chainSlug"), short: s(c, "shortName"), rpcs, explorers, testnet: c["isTestnet"] == true, symbol: c["nativeCurrency"]["symbol"].as_str().unwrap_or_default().into() })
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

/// The chain's native token from the cached chain list, without downloading it; ETH when unknown.
pub fn symbol(id: u64) -> String {
    chainlist(false).into_iter().find(|c| c.id == id).map(|c| c.symbol).filter(|s| !s.is_empty()).unwrap_or("ETH".into())
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

/// Failures worth asking again: rate limits as public RPCs word them ("25/second request limit reached", "capacity
/// exceeded"), timeouts, gateways and server errors, the network itself (DNS, refused or dropped connections), and
/// answers that aren't proper JSON-RPC responses, which a node behind a load balancer gives now and then.
fn transient(e: &str) -> bool {
    let e = e.to_lowercase();
    ["429", "rate limit", "rate-limit", "ratelimit", "too many requests", "request limit", "limit reached", "capacity exceeded", "timed out", "timeout", "bad gateway", "service unavailable", "gateway", "http 5", "network error", "not a json-rpc", "malformed"]
        .iter()
        .any(|t| e.contains(t))
}

/// JSON-RPC batch with a result per call. Retries what load-balanced public RPCs fail on now and then
/// (a node behind on blocks, rate limits, a dropped connection); falls back to one request per call if the server
/// rejects batches. A result is always the answer to its own call: an answer that isn't one is an error.
pub fn batch_each(url: &str, calls: &[(&str, Value)]) -> Result<Vec<Result<Value, String>>, String> {
    let mut tries = 0;
    loop {
        let r = batch_once(url, calls);
        let retry = match &r {
            Err(e) => transient(e),
            Ok(items) => items.iter().any(|x| x.as_ref().is_err_and(|e| transient(e))),
        };
        if !retry || tries == 8 {
            return r;
        }
        if std::env::var_os("PLAIN_DEBUG").is_some() {
            eprintln!("rpc retry: {}", match &r { Err(e) => e.clone(), Ok(items) => items.iter().filter_map(|x| x.as_ref().err()).cloned().collect::<Vec<_>>().join("; ") });
        }
        tries += 1;
        backoff(tries);
    }
}

/// The wait before asking again: doubling from 0.6 s up to 5 s (2 ms in plain's own tests); 8 tries are about 30 s,
/// what a rate-limited public RPC needs to let a burst of parallel tests through.
pub fn backoff(tries: u32) {
    let ms = if cfg!(test) { 1 << tries.min(4) } else { (300u64 << tries.min(5)).min(5000) };
    std::thread::sleep(Duration::from_millis(ms));
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
    // (an error can quote the URL, which can hold a key)
    let mut r = agent().post(url).send_json(if body.len() == 1 { body[0].clone() } else { Value::Array(body) }).map_err(|e| format!("{name}: network error: {}", e.to_string().replace(url, &shown(url))))?;
    let status = r.status().as_u16();
    let text = r.body_mut().with_config().limit(256 << 20).read_to_string().map_err(|e| format!("{name}: network error: {e}"))?;
    let snippet = || text.chars().take(200).collect::<String>();
    let v: Value = serde_json::from_str(&text).map_err(|_| format!("{name}: HTTP {status}: {}", snippet()))?;
    let items = match v {
        // every call failing over the batch itself (e.g. free plans capping batch size): one call at a time
        Value::Array(a) if a.len() > 1 && a.iter().all(|x| x["error"]["message"].as_str().is_some_and(|m| m.to_lowercase().contains("batch"))) => return Ok(calls.iter().map(|(m, p)| call(url, m, p.clone())).collect()),
        Value::Array(a) if a.len() == calls.len() => a,
        // one error object, or a different number of answers, back for a batch: the server doesn't take batches
        _ if calls.len() > 1 => return Ok(calls.iter().map(|(m, p)| call(url, m, p.clone())).collect()),
        one => vec![one],
    };
    // answers may come in any order; each must carry the id of a call, once (some servers echo ids as strings)
    let mut answers: Vec<Option<Value>> = vec![None; calls.len()];
    for x in items {
        let id = match &x["id"] {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        };
        // an error about the request itself may come without an id
        let id = id.or((calls.len() == 1 && x.get("error").is_some()).then_some(0));
        match id.map(|i| i as usize).filter(|&i| i < calls.len() && answers[i].is_none()) {
            Some(i) => answers[i] = Some(x),
            // e.g. mainnet.base.org's rate limit: HTTP 429 {"code":-32007,"message":"25/second request limit reached"}
            None if !(200..300).contains(&status) => return Err(format!("{name}: HTTP {status}: {}", snippet())),
            None => return Err(format!("{name}: not a JSON-RPC answer to this request (ids don't match): {}", snippet())),
        }
    }
    let ok = (200..300).contains(&status);
    Ok(answers
        .into_iter()
        .zip(calls)
        .map(|(x, (m, _))| {
            let x = x.unwrap_or_default();
            match (x.get("error"), x.get("result")) {
                (Some(e), _) => Err(format!("{m}: {}", e["message"].as_str().map_or_else(|| e.to_string(), String::from))),
                (None, _) if !ok => Err(format!("{m}: HTTP {status}: {}", snippet())),
                (None, Some(r)) => Ok(r.clone()),
                (None, None) => Err(format!("{m}: malformed answer, with neither a result nor an error: {}", snippet())),
            }
        })
        .collect())
}

/// A JSON-RPC quantity (a balance, a nonce, a block number) or storage word: "0x" and 1 to 64 hex digits. Anything
/// else (null, "0x", a number, garbage) is an error, so that a node that answers oddly can never pass for an empty
/// account or a zero slot.
pub fn quantity(v: &Value) -> Result<alloy_primitives::U256, String> {
    let s = v.as_str().ok_or_else(|| format!("expected a hex number, got {}", short(v)))?;
    s.strip_prefix("0x")
        .filter(|h| !h.is_empty() && h.len() <= 64)
        .and_then(|h| alloy_primitives::U256::from_str_radix(h, 16).ok())
        .ok_or_else(|| format!("expected a hex number, got {}", short(v)))
}

/// JSON-RPC data (code, a hash): "0x" and an even number of hex digits; "0x" alone is empty data.
pub fn data(v: &Value) -> Result<Vec<u8>, String> {
    v.as_str().and_then(|s| s.strip_prefix("0x")).and_then(|h| hex::decode(h).ok()).ok_or_else(|| format!("expected hex data, got {}", short(v)))
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 80 { format!("{}…", s.chars().take(80).collect::<String>()) } else { s }
}

struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

/// A JSON-RPC server on localhost for tests: `answer(request, n)` gives the n-th request's status and body.
#[cfg(test)]
pub mod mock {
    use std::io::{BufRead, Read, Write};

    pub fn serve(answer: impl Fn(&serde_json::Value, usize) -> (u16, String) + Send + 'static) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for (n, s) in l.incoming().enumerate() {
                let Ok(s) = s else { continue };
                let mut r = std::io::BufReader::new(s);
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
                        len = v.trim().parse().unwrap_or_else(|_| panic!("{k}: {v}"));
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let (status, out) = answer(&serde_json::from_slice(&body).unwrap(), n);
                let mut s = r.into_inner();
                write!(s, "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len()).unwrap();
            }
        });
        url
    }

    /// Answers each call of a request (a batch or one call) with `result(method, params, n)`, in request order.
    pub fn results(result: impl Fn(&str, &serde_json::Value, usize) -> serde_json::Value + Send + 'static) -> String {
        serve(move |req, n| {
            let one = |c: &serde_json::Value| serde_json::json!({"jsonrpc": "2.0", "id": c["id"], "result": result(c["method"].as_str().unwrap(), &c["params"], n)});
            (200, match req.as_array() {
                Some(calls) => serde_json::Value::Array(calls.iter().map(one).collect()),
                None => one(req),
            }.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    #[test]
    fn urls_show_without_keys() {
        assert_eq!(super::shown("https://base-mainnet.g.alchemy.com/v2/secret"), "https://base-mainnet.g.alchemy.com/…");
        assert_eq!(super::shown("https://user:secret@node.example:8545/?key=secret"), "https://node.example:8545/…");
        assert_eq!(super::shown("http://127.0.0.1:8545"), "http://127.0.0.1:8545");
        assert_eq!(super::shown("https://mainnet.base.org/"), "https://mainnet.base.org");
    }

    #[test]
    fn answers_go_to_their_calls() {
        // answers in reverse order, ids echoed as strings: each result still belongs to its own call
        let url = super::mock::serve(|req, _| {
            let a: Vec<Value> = req.as_array().unwrap().iter().rev().map(|c| json!({"jsonrpc": "2.0", "id": c["id"].to_string(), "result": c["method"]})).collect();
            (200, Value::Array(a).to_string())
        });
        assert_eq!(super::batch(&url, &[("a", json!([])), ("b", json!([])), ("c", json!([]))]).unwrap(), ["a", "b", "c"]);
        // a flaky first answer is asked again
        let url = super::mock::serve(|req, n| match n {
            0 => (503, r#"{"message":"no healthy upstream"}"#.into()),
            _ => (200, json!({"jsonrpc": "2.0", "id": req["id"], "result": "0x5"}).to_string()),
        });
        assert_eq!(super::call(&url, "eth_chainId", json!([])).unwrap(), "0x5");
    }

    #[test]
    fn broken_answers_are_errors() {
        // ids that don't match the calls: never results, which could be some other call's
        for body in [
            r#"[{"jsonrpc":"2.0","id":0,"result":"0x1"},{"jsonrpc":"2.0","id":0,"result":"0x2"}]"#,
            r#"[{"jsonrpc":"2.0","id":0,"result":"0x1"},{"jsonrpc":"2.0","id":7,"result":"0x2"}]"#,
            r#"[{"jsonrpc":"2.0","result":"0x1"},{"jsonrpc":"2.0","result":"0x2"}]"#,
        ] {
            let url = super::mock::serve(move |_, _| (200, body.into()));
            assert!(super::batch(&url, &[("a", json!([])), ("b", json!([]))]).is_err(), "{body}");
        }
        // a body that isn't a JSON-RPC answer, one with neither result nor error, a result under an error status
        // mainnet.base.org's rate limit came back as `result: null`, so as a zero slot (plain 0.3.2)
        let base_429 = r#"{"code":-32007,"message":"25/second request limit reached"}"#;
        for (status, body) in [(429, base_429), (503, r#"{"message":"no healthy upstream"}"#), (200, r#"{"jsonrpc":"2.0","id":0}"#), (429, r#"{"jsonrpc":"2.0","id":0,"result":"0x5"}"#), (200, "<html>")] {
            let url = super::mock::serve(move |_, _| (status, body.into()));
            assert!(super::call(&url, "a", json!([])).is_err(), "{status} {body}");
        }
        // a null result is a result (a receipt not in yet); the typed reads reject what isn't a number or data
        let url = super::mock::results(|_, _, _| Value::Null);
        assert_eq!(super::call(&url, "eth_getTransactionReceipt", json!([])).unwrap(), Value::Null);
        for bad in [json!(null), json!("0x"), json!(5), json!("0xzz"), json!(format!("0x{}", "1".repeat(65)))] {
            assert!(super::quantity(&bad).is_err(), "{bad}");
        }
        assert_eq!(super::quantity(&json!("0x0")).unwrap(), alloy_primitives::U256::ZERO);
        assert_eq!(super::data(&json!("0x")).unwrap(), Vec::<u8>::new());
        assert!(super::data(&json!(null)).is_err() && super::data(&json!("0x123")).is_err());
    }

    #[test]
    fn names() {
        let c = |id, name: &str, slug: &str, short: &str| super::Chain { id, name: name.into(), slug: slug.into(), short: short.into(), rpcs: vec![], explorers: vec![], testnet: false, symbol: String::new() };
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
