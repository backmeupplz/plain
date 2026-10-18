//! Source verification: Sourcify (no key), Etherscan's v2 API (one ETHERSCAN_API_KEY covers 60+ chains), and
//! the chain's Blockscout and Routescan explorers from chainlist.org. Every contract goes to every explorer at
//! once; what takes time is the explorers' own compiles, so those run in parallel too.
use crate::project::{Graph, Project};
use crate::rpc;
use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A deployed contract and what an explorer needs to verify it. Saved in .plain/deploys/<chain id>.json.
#[derive(Serialize, Deserialize, Clone)]
pub struct Record {
    pub name: String,
    /// "path/File.sol:Name"
    pub contract: String,
    /// constructor arguments, hex
    pub args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<B256>,
    /// linked libraries, "path/File.sol:Name" -> address
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub libs: BTreeMap<String, Address>,
    /// "0.8.28+commit.7893614a"
    pub compiler: String,
    /// sha256 of the standard JSON input, saved in .plain/deploys/inputs/<hash>.json
    pub input: String,
    #[serde(default)]
    pub verified: Vec<String>,
    /// an ERC-1967 proxy plain deployed: Etherscan is told to read it through its implementation
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proxy: bool,
}

/// Standard JSON input for `file`: its import closure as compiled, the project's settings. solc's metadata (and
/// so the bytecode) covers exactly this closure, whatever else the build compiled alongside it.
pub fn input(p: &Project, file: &str) -> Value {
    let g = Graph::build(p, &[file.to_string()]);
    let sources: serde_json::Map<String, Value> = g.closure(file).into_iter().map(|u| (u.clone(), json!({"content": g.files[&u].content}))).collect();
    let mut settings = crate::solc::settings(p);
    settings["outputSelection"] = json!({"*": {"*": ["abi", "evm.bytecode.object", "evm.deployedBytecode.object", "metadata"]}});
    json!({"language": "Solidity", "sources": sources, "settings": settings})
}

fn dir(p: &Project) -> std::path::PathBuf {
    p.root.join(".plain/deploys")
}

pub fn input_hash(input: &Value) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(serde_json::to_vec(input).unwrap()))
}

/// Saves an input once per content hash; returns the hash.
pub fn save_input(p: &Project, input: &Value) -> String {
    let h = input_hash(input);
    let path = dir(p).join("inputs").join(format!("{h}.json"));
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, serde_json::to_vec(input).unwrap()).ok();
    }
    h
}

/// A saved input with the record's libraries in its settings, the way solc links them at compile time
/// (what explorers' Etherscan-style APIs and their upload forms take).
fn with_libs(input: &str, r: &Record) -> Value {
    let mut input: Value = serde_json::from_str(input).unwrap_or_default();
    for (id, a) in &r.libs {
        if let Some((f, n)) = id.rsplit_once(':') {
            input["settings"]["libraries"][f][n] = json!(a.to_checksum(None));
        }
    }
    input
}

pub fn load(p: &Project, chain: u64) -> BTreeMap<Address, Record> {
    std::fs::read(dir(p).join(format!("{chain}.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(p: &Project, chain: u64, recs: &BTreeMap<Address, Record>) {
    std::fs::create_dir_all(dir(p)).ok();
    std::fs::write(dir(p).join(format!("{chain}.json")), serde_json::to_string_pretty(recs).unwrap() + "\n").ok();
}

/// Chains with saved deploys.
pub fn chains(p: &Project) -> Vec<u64> {
    std::fs::read_dir(dir(p)).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".json")?.parse().ok()).collect()
}

struct Explorer {
    name: &'static str,
    /// Etherscan-style API URL; empty for Sourcify
    api: String,
    key: String,
    /// minimum time between requests (free Etherscan keys allow 3 per second)
    gap: Duration,
    next: Mutex<Instant>,
}

impl Explorer {
    fn new(name: &'static str, api: String, key: String, gap_ms: u64) -> Explorer {
        Explorer { name, api, key, gap: Duration::from_millis(gap_ms), next: Mutex::new(Instant::now()) }
    }
    fn pace(&self) {
        let mut n = self.next.lock().unwrap();
        let now = Instant::now();
        if *n > now {
            std::thread::sleep(*n - now);
        }
        *n = Instant::now() + self.gap;
    }
}

/// $ETHERSCAN_API_KEY (or the project's .env), foundry.toml's [etherscan], or the one the menu saved.
pub fn etherscan_key(p: &Project) -> Option<String> {
    let saved = || crate::project::user_config().get("etherscan_api_key").and_then(|k| k.as_str()).map(String::from);
    std::env::var("ETHERSCAN_API_KEY").ok().or_else(|| p.etherscan_key.clone()).or_else(saved).filter(|k| !k.is_empty())
}

fn explorers(p: &Project, chain: u64, skipped: &mut Vec<String>) -> Vec<Explorer> {
    let mut out = vec![Explorer::new("sourcify", String::new(), String::new(), 100)];
    match etherscan_key(p) {
        Some(k) => out.push(Explorer::new("etherscan", format!("https://api.etherscan.io/v2/api?chainid={chain}"), k, 350)),
        None => skipped.push("etherscan (no API key: set ETHERSCAN_API_KEY, or run plain and open Settings)".into()),
    }
    if let Some(c) = rpc::chain_info(chain) {
        for e in &c.explorers {
            if e.contains("blockscout") && !out.iter().any(|x| x.name == "blockscout") {
                out.push(Explorer::new("blockscout", format!("{e}/api"), String::new(), 200));
            } else if e.contains("routescan.io") && !out.iter().any(|x| x.name == "routescan") {
                let net = if c.testnet { "testnet" } else { "mainnet" };
                out.push(Explorer::new("routescan", format!("https://api.routescan.io/v2/network/{net}/evm/{chain}/etherscan/api"), "verifyContract".into(), 200));
            }
        }
    }
    out
}

enum Outcome {
    Verified,
    Skipped(String),
    Failed(String),
}

/// Verifies `recs` (address -> record) on every explorer for `chain`; marks successes in the records.
/// Returns false if any contract failed to verify somewhere.
pub fn run(p: &Project, chain: u64, recs: &mut BTreeMap<Address, Record>) -> bool {
    let mut skipped = vec![];
    let exps = explorers(p, chain, &mut skipped);
    let todo: Vec<(&Explorer, Address)> = exps.iter().flat_map(|e| recs.iter().filter(|(_, r)| !r.verified.iter().any(|v| v == e.name)).map(move |(a, _)| (e, *a))).collect();
    if todo.is_empty() {
        println!("verified already on {}", exps.iter().map(|e| e.name).collect::<Vec<_>>().join(", "));
        return true;
    }
    let t = Instant::now();
    println!("verifying {} contract(s) on {}", recs.len(), exps.iter().map(|e| e.name).collect::<Vec<_>>().join(", "));
    let inputs: BTreeMap<String, String> = recs
        .values()
        .map(|r| (r.input.clone(), std::fs::read_to_string(dir(p).join("inputs").join(format!("{}.json", r.input))).unwrap_or_default()))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(300);
    let results: Vec<(&str, Address, Outcome)> = std::thread::scope(|s| {
        let hs: Vec<_> = todo
            .iter()
            .map(|(e, a)| {
                let r = &recs[a];
                let input = &inputs[&r.input];
                s.spawn(move || (e.name, *a, if input.is_empty() { Outcome::Failed("standard JSON input missing from .plain/deploys/inputs".into()) } else { one(e, chain, *a, r, input, deadline) }))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut ok = true;
    let mut lines = vec![];
    for e in &exps {
        let mine: Vec<_> = results.iter().filter(|r| r.0 == e.name).collect();
        if mine.is_empty() {
            continue;
        }
        let done = mine.iter().filter(|r| matches!(r.2, Outcome::Verified)).count();
        if let Some(Outcome::Skipped(why)) = mine.iter().map(|r| &r.2).find(|o| matches!(o, Outcome::Skipped(_))) {
            lines.push(format!("{}: skipped ({why})", e.name));
            continue;
        }
        lines.push(format!("{}: {done}/{} verified", e.name, mine.len()));
        for (_, a, o) in mine {
            match o {
                Outcome::Verified => recs.get_mut(a).unwrap().verified.push(e.name.to_string()),
                Outcome::Failed(why) => {
                    ok = false;
                    lines.push(format!("  {} {a}: {why}", recs[a].name));
                }
                Outcome::Skipped(_) => {}
            }
        }
    }
    lines.extend(skipped.into_iter().map(|s| format!("skipped {s}")));
    println!("verification ({:.1?}):\n  {}", t.elapsed(), lines.join("\n  "));
    ok
}

/// One contract on one explorer: submit (retrying while the explorer hasn't indexed it yet), then poll.
fn one(e: &Explorer, chain: u64, addr: Address, r: &Record, input: &str, deadline: Instant) -> Outcome {
    let mut wait = Duration::from_secs(2);
    let mut ticket: Option<String> = None;
    // explorers index a new contract within seconds; their compile queues can take minutes
    let indexed_by = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && (ticket.is_some() || Instant::now() < indexed_by) {
        e.pace();
        let step = match &ticket {
            None => submit(e, chain, addr, r, input),
            Some(t) => check(e, t),
        };
        match step {
            Step::Ticket(t) => {
                ticket = Some(t);
                wait = Duration::from_secs(2);
            }
            Step::Done(o) => return o,
            Step::Retry => {}
            Step::Again => ticket = None,
        }
        std::thread::sleep(wait);
        wait = (wait * 3 / 2).min(Duration::from_secs(10));
    }
    Outcome::Failed(format!("timed out{}", ticket.map(|t| format!(" (ticket {t})")).unwrap_or_default()))
}

enum Step {
    Ticket(String),
    Done(Outcome),
    Retry,
    /// submit again (the explorer's node didn't have the contract yet)
    Again,
}

fn submit(e: &Explorer, chain: u64, addr: Address, r: &Record, input: &str) -> Step {
    if e.api.is_empty() {
        let mut body = json!({"stdJsonInput": serde_json::from_str::<Value>(input).unwrap_or_default(), "compilerVersion": r.compiler, "contractIdentifier": r.contract});
        if let Some(tx) = r.tx {
            body["creationTransactionHash"] = json!(tx);
        }
        let res = rpc::agent().post(&format!("https://sourcify.dev/server/v2/verify/{chain}/{addr}")).send_json(&body);
        let Ok(mut res) = res else { return Step::Retry };
        let status = res.status().as_u16();
        let v: Value = res.body_mut().with_config().limit(16 << 20).read_json().unwrap_or_default();
        debug(e, &format!("submit {} {status} {v}", r.name));
        let code = v["customCode"].as_str().unwrap_or_default();
        return match (status, code) {
            (202, _) => v["verificationId"].as_str().map_or(Step::Retry, |id| Step::Ticket(id.into())),
            (409, _) => Step::Done(Outcome::Verified),
            (_, "unsupported_chain") => Step::Done(Outcome::Skipped("chain not supported".into())),
            (429, _) | (500.., _) | (_, "duplicate_verification_request") => Step::Retry,
            _ => Step::Done(Outcome::Failed(v["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {status}")))),
        };
    }
    // Etherscan-style APIs
    let input = with_libs(input, r);
    let args = r.args.trim_start_matches("0x");
    let form = [
        ("apikey", e.key.as_str()),
        ("module", "contract"),
        ("action", "verifysourcecode"),
        ("contractaddress", &addr.to_checksum(None)),
        ("sourceCode", &input.to_string()),
        ("codeformat", "solidity-standard-json-input"),
        ("contractname", &r.contract),
        ("compilerversion", &format!("v{}", r.compiler)),
        ("constructorArguements", args),
        ("constructorArguments", args),
    ];
    let Ok(mut res) = rpc::agent().post(&e.api).send_form(form) else { return Step::Retry };
    let v: Value = res.body_mut().read_json().unwrap_or_default();
    debug(e, &format!("submit {} {v}", r.name));
    let msg = v["result"].as_str().unwrap_or_default();
    let low = msg.to_lowercase();
    if v["status"] == "1" && !msg.is_empty() {
        Step::Ticket(msg.into())
    } else if low.contains("already verified") {
        Step::Done(Outcome::Verified)
    } else if low.contains("unable to locate") || low.contains("rate limit") || low.contains("not a smart-contract") || low.contains("not a contract") || low.contains("not found") || v.is_null() {
        // not indexed yet
        Step::Retry
    } else if low.contains("chainid") || low.contains("chain not supported") || v["message"].as_str().is_some_and(|m| m.contains("chain not supported")) {
        Step::Done(Outcome::Skipped("chain not supported".into()))
    } else {
        Step::Done(Outcome::Failed(if msg.is_empty() { v.to_string() } else { msg.to_string() }))
    }
}

fn check(e: &Explorer, ticket: &str) -> Step {
    if e.api.is_empty() {
        let Ok(mut res) = rpc::agent().get(&format!("https://sourcify.dev/server/v2/verify/{ticket}")).call() else { return Step::Retry };
        let v: Value = res.body_mut().read_json().unwrap_or_default();
        debug(e, &format!("check {v}"));
        if v["isJobCompleted"] != true {
            return Step::Retry;
        }
        let code = v["error"]["customCode"].as_str().unwrap_or_default();
        return match v["contract"]["match"].as_str() {
            Some(_) => Step::Done(Outcome::Verified),
            // its node may not have the block yet
            None if code.contains("not_deployed") || code.contains("cannot_fetch") => Step::Again,
            None => Step::Done(Outcome::Failed(v["error"]["message"].as_str().unwrap_or("verification failed").chars().take(300).collect())),
        };
    }
    let sep = if e.api.contains('?') { '&' } else { '?' };
    let url = format!("{}{sep}module=contract&action=checkverifystatus&guid={ticket}&apikey={}", e.api, e.key);
    let Ok(mut res) = rpc::agent().get(&url).call() else { return Step::Retry };
    let v: Value = res.body_mut().read_json().unwrap_or_default();
    debug(e, &format!("check {v}"));
    let msg = v["result"].as_str().unwrap_or_default();
    let low = msg.to_lowercase();
    if low.contains("pending") || low.contains("in progress") || low.contains("rate limit") || v.is_null() {
        Step::Retry
    } else if low.starts_with("pass") || low.contains("already verified") {
        Step::Done(Outcome::Verified)
    } else {
        Step::Done(Outcome::Failed(msg.into()))
    }
}

fn debug(e: &Explorer, msg: &str) {
    if std::env::var_os("PLAIN_DEBUG").is_some() {
        eprintln!("{}: {}", e.name, msg.chars().take(400).collect::<String>());
    }
}

/// Marks `proxy` as a proxy on Etherscan, so its page reads and writes through its implementation's ABI (what
/// "Is this a proxy?" does there). Best effort: a failure only prints a line.
pub fn link_proxy(p: &Project, chain: u64, proxy: Address) {
    let Some(key) = etherscan_key(p) else { return };
    let api = format!("https://api.etherscan.io/v2/api?chainid={chain}");
    let addr = proxy.to_checksum(None);
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut guid: Option<String> = None;
    let mut msg = String::new();
    while Instant::now() < deadline {
        let res = match &guid {
            None => rpc::agent().post(&api).send_form([("apikey", key.as_str()), ("module", "contract"), ("action", "verifyproxycontract"), ("address", &addr)]),
            Some(g) => rpc::agent().get(&format!("{api}&module=contract&action=checkproxyverification&guid={g}&apikey={key}")).call(),
        };
        let v: Value = res.ok().and_then(|mut r| r.body_mut().read_json().ok()).unwrap_or_default();
        msg = v["result"].as_str().unwrap_or_default().to_string();
        let low = msg.to_lowercase();
        match (&guid, v["status"] == "1") {
            (None, true) => guid = Some(msg.clone()),
            (Some(_), true) => {
                println!("etherscan: {addr} now reads as a proxy");
                return;
            }
            // not indexed or verified yet, still in the queue, rate limited
            _ if ["pending", "queue", "unable to locate", "not found", "rate limit", "in progress"].iter().any(|t| low.contains(t)) || v.is_null() => {}
            _ => break,
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    println!("etherscan: {addr} isn't marked as a proxy yet ({}); \"Is this a proxy?\" on its page does it", if msg.is_empty() { "timed out" } else { &msg });
}

/// Files to verify by hand on any explorer's "verify and publish" page: for each contract, the standard JSON input
/// to upload (libraries linked in) and a note with every value the form asks for. Returns the folder.
pub fn export(p: &Project, chain: u64, net: &str, recs: &BTreeMap<Address, Record>) -> std::path::PathBuf {
    let out = p.root.join("verify").join(net);
    std::fs::create_dir_all(&out).unwrap_or_else(|e| crate::die(&format!("{}: {e}", out.display())));
    // (chainlist files 31337 under another chain; a dev chain has no explorer)
    let explorers = if matches!(chain, 31337 | 1337) { vec![] } else { rpc::chain_info(chain).map(|c| c.explorers).unwrap_or_default() };
    for (addr, r) in recs {
        let stem = match recs.values().filter(|x| x.name == r.name).count() {
            1 => r.name.clone(),
            _ => format!("{}-{}", r.name, &addr.to_string()[..8]),
        };
        let Ok(raw) = std::fs::read_to_string(dir(p).join("inputs").join(format!("{}.json", r.input))) else {
            eprintln!("{} {addr}: its standard JSON input is missing from .plain/deploys/inputs", r.name);
            continue;
        };
        let input = with_libs(&raw, r);
        let file = r.contract.rsplit_once(':').map_or("", |x| x.0);
        let license = input["sources"][file]["content"].as_str().and_then(|s| s.split("SPDX-License-Identifier:").nth(1)).and_then(|s| s.split_whitespace().next()).unwrap_or("none");
        let args = r.args.trim_start_matches("0x");
        // Etherscan-family explorers take the address in their verify page's URL
        let pages: Vec<String> = explorers
            .iter()
            .map(|e| match e.contains("etherscan") || (e.contains("scan.") && !e.contains("routescan")) {
                true => format!("{e}/verifyContract?a={addr}"),
                false => format!("{e}/address/{addr}"),
            })
            .collect();
        let mut note = format!("{} at {addr} (chain {chain})\n\n", r.name);
        if !pages.is_empty() {
            note += &format!("Verify page:\n  {}\n\n", pages.join("\n  "));
        }
        note += &format!("Compiler type:     Solidity (Standard-Json-Input)\nCompiler version:  v{}\nLicense:           {license}\nFile to upload:    {stem}.json\nContract name:     {}\nConstructor args:  {}\n", r.compiler, r.contract, if args.is_empty() { "none" } else { args });
        for (lib, a) in &r.libs {
            note += &format!("Library:           {lib} at {a}\n");
        }
        std::fs::write(out.join(format!("{stem}.json")), serde_json::to_vec_pretty(&input).unwrap()).ok();
        std::fs::write(out.join(format!("{stem}.txt")), &note).ok();
        println!("{}", note.trim_end());
        println!();
    }
    out
}
