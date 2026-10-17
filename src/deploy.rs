//! `plain deploy`: simulate (forked when a network is given), then optionally sign + send.
use crate::evm::{self, BroadcastTx, Cheats, Env, SENDER};
use crate::project::Project;
use crate::solc::Artifact;
use crate::{decode, die, rpc};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_dyn_abi::{DynSolType, JsonAbiExt, Specifier};
use alloy_eips::eip2718::Encodable2718;
use alloy_json_abi::JsonAbi;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub struct Opts {
    pub target: String,
    pub args: Vec<String>,
    pub network: Option<String>,
    pub broadcast: bool,
}

fn key_addr(k: B256) -> Address {
    evm::pk_to_addr(k.into()).unwrap_or_else(|e| die(&e))
}

fn hexnum(v: &Value) -> u128 {
    u128::from_str_radix(v.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0)
}

fn find(arts: &[Artifact], target: &str) -> Artifact {
    let hits: Vec<&Artifact> = arts
        .iter()
        .filter(|a| !a.bytecode.is_empty())
        .filter(|a| if target.ends_with(".sol") { a.file == target || a.file.ends_with(&format!("/{target}")) } else { a.name == target || format!("{}:{}", a.file, a.name) == target })
        .collect();
    match hits.as_slice() {
        [a] => (*a).clone(),
        [] => die(&format!("no deployable contract matches {target:?}")),
        many => {
            // a script file may hold helpers; prefer the one with run()
            let runs: Vec<_> = many.iter().filter(|a| has_run(a)).collect();
            if runs.len() == 1 {
                return (**runs[0]).clone();
            }
            die(&format!("{target:?} is ambiguous: {}", many.iter().map(|a| format!("{}:{}", a.file, a.name)).collect::<Vec<_>>().join(", ")))
        }
    }
}

fn has_run(a: &Artifact) -> bool {
    a.abi.as_array().is_some_and(|f| f.iter().any(|x| x["type"] == "function" && x["name"] == "run" && x["inputs"].as_array().is_some_and(|i| i.is_empty())))
}

fn describe(tx: &BroadcastTx, names: &HashMap<Address, String>) -> String {
    let created = || tx.created.and_then(|a| names.get(&a).cloned()).unwrap_or("<unknown contract>".into());
    match tx.to {
        None => format!("create {}", created()),
        Some(evm::CREATE2_DEPLOYER) => format!("create2 {}", created()),
        Some(to) => {
            let f = (tx.data.len() >= 4).then(|| decode::tables().and_then(|t| t.functions.get(&<[u8; 4]>::try_from(&tx.data[..4]).unwrap()))).flatten();
            let call = match f {
                Some(f) => match f.abi_decode_input(&tx.data[4..]) {
                    Ok(v) => format!("{}({})", f.name, v.iter().map(decode::display).collect::<Vec<_>>().join(", ")),
                    Err(_) => f.name.clone(),
                },
                None if tx.data.is_empty() => "transfer".into(),
                None => format!("0x{}", hex::encode(&tx.data[..4.min(tx.data.len())])),
            };
            format!("call {to}.{call}")
        }
    }
}

pub fn run(p: &Project, arts: Vec<Artifact>, o: Opts) {
    decode::init(&arts);
    let key = std::env::var("PRIVATE_KEY").ok().map(|k| k.trim().parse::<B256>().unwrap_or_else(|_| die("PRIVATE_KEY must be 32 bytes hex")));
    let me = key.map(key_addr);
    let url = o.network.as_ref().map(|n| rpc::network_url(p, n).unwrap_or_else(|| die(&format!("unknown network {n:?}; known: {}", rpc::network_names(p).join(", ")))));
    if o.broadcast && url.is_none() {
        die("--broadcast needs --network <name|url>");
    }
    if o.broadcast && key.is_none() {
        die("--broadcast needs PRIVATE_KEY in the environment");
    }

    let arts = Arc::new(arts);
    let remote = evm::Remote(url.as_ref().map(|u| Arc::new(evm::Fork::new(u, None))), None);
    let mut env = Env::new(31337);
    if let Some(u) = &url {
        env.chain_id = hexnum(&rpc::call(u, "eth_chainId", json!([])).unwrap_or_else(|e| die(&e))) as u64;
        if let Ok(b) = rpc::call(u, "eth_getBlockByNumber", json!(["latest", false])) {
            env.block.number = U256::from(hexnum(&b["number"]));
            env.block.timestamp = U256::from(hexnum(&b["timestamp"]));
        }
    }
    let art = find(&arts, &o.target);
    let mut cheats = Cheats::new(arts.clone(), p.root.clone());
    cheats.default_broadcaster = Some(me.unwrap_or(SENDER));
    if let Some(k) = key {
        cheats.keys.insert(me.unwrap(), k);
    }
    let db = evm::base_db(remote);
    cheats.track_creates = true;
    let is_script = art.file.ends_with(".s.sol") || (p.dirs.get(2).is_some_and(|d| art.file.starts_with(&format!("{d}/"))) && has_run(&art));

    let txs: Vec<BroadcastTx> = if is_script {
        let mut libs = HashMap::new();
        let (db, r) = crate::test::deploy(db, &mut env, &mut cheats, &arts, &art, &[], &mut libs);
        let addr = r.address.filter(|_| r.ok).unwrap_or_else(|| die(&format!("script constructor reverted: {}", decode::reason(&r.output))));
        let (_, r) = evm::transact(db, &mut env, &mut cheats, SENDER, TxKind::Call(addr), Bytes::from_static(&[0xc0, 0x40, 0x62, 0x26]), U256::ZERO, true);
        for l in &cheats.console {
            println!("log: {l}");
        }
        if !r.ok {
            die(&format!("{}.run() reverted: {}", art.name, decode::reason(&r.output)));
        }
        std::mem::take(&mut cheats.broadcasts)
    } else {
        if art.links.is_object() {
            die("contracts that need library linking must be deployed from a script");
        }
        let abi: JsonAbi = serde_json::from_value(art.abi.clone()).unwrap();
        let inputs = abi.constructor().map(|c| c.inputs.clone()).unwrap_or_default();
        if inputs.len() != o.args.len() {
            die(&format!("{} constructor takes {} args ({}), got {}", art.name, inputs.len(), inputs.iter().map(|i| i.ty.clone()).collect::<Vec<_>>().join(", "), o.args.len()));
        }
        let vals: Vec<_> = inputs
            .iter()
            .zip(&o.args)
            .map(|(i, a)| {
                let ty: DynSolType = i.resolve().unwrap();
                ty.coerce_str(a).unwrap_or_else(|e| die(&format!("arg {:?} as {}: {e}", a, i.ty)))
            })
            .collect();
        let mut data = hex::decode(&art.bytecode).unwrap();
        data.extend(alloy_dyn_abi::DynSolValue::Tuple(vals).abi_encode_params());
        let from = me.unwrap_or(SENDER);
        let (_, r) = evm::transact(db, &mut env, &mut cheats, from, TxKind::Create, data.clone().into(), U256::ZERO, false);
        if let Some(a) = r.address {
            cheats.names.insert(a, art.name.clone());
        }
        if !r.ok {
            die(&format!("{} constructor reverted: {}", art.name, decode::reason(&r.output)));
        }
        vec![BroadcastTx { from, to: None, value: U256::ZERO, data: data.into(), created: r.address }]
    };

    if txs.is_empty() {
        println!("nothing to broadcast (did the script call vm.startBroadcast?)");
        return;
    }
    for (i, tx) in txs.iter().enumerate() {
        println!("{}. {} (from {})", i + 1, describe(tx, &cheats.names), tx.from);
    }
    if !o.broadcast {
        println!("simulated {} transaction(s){}; add --broadcast to send", txs.len(), o.network.as_ref().map(|n| format!(" against {n}")).unwrap_or_default());
        return;
    }
    send(p, &cheats.names, url.as_ref().unwrap(), o.network.as_ref().unwrap(), &txs, &cheats.keys);
}

fn send(p: &Project, names: &HashMap<Address, String>, url: &str, network: &str, txs: &[BroadcastTx], keys: &HashMap<Address, B256>) {
    let q = |m: &str, params: Value| rpc::call(url, m, params).unwrap_or_else(|e| die(&e));
    let chain_id = hexnum(&q("eth_chainId", json!([]))) as u64;
    let mut nonces: HashMap<Address, u64> = HashMap::new();
    let mut deployed = BTreeMap::new();
    for (i, tx) in txs.iter().enumerate() {
        let key = keys.get(&tx.from).unwrap_or_else(|| die(&format!("no private key for broadcaster {}", tx.from)));
        let nonce = *nonces.entry(tx.from).or_insert_with(|| hexnum(&q("eth_getTransactionCount", json!([tx.from, "pending"]))) as u64);
        let mut call = json!({"from": tx.from, "value": format!("0x{:x}", tx.value), "data": format!("0x{}", hex::encode(&tx.data))});
        if let Some(to) = tx.to {
            call["to"] = json!(to);
        }
        let gas = hexnum(&q("eth_estimateGas", json!([call]))) as u64;
        let base = hexnum(&q("eth_getBlockByNumber", json!(["latest", false]))["baseFeePerGas"]);
        let prio = rpc::call(url, "eth_maxPriorityFeePerGas", json!([])).map(|v| hexnum(&v)).unwrap_or(1_000_000);
        let t = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: gas * 13 / 10,
            max_fee_per_gas: base * 2 + prio,
            max_priority_fee_per_gas: prio,
            to: tx.to.map(TxKind::Call).unwrap_or(TxKind::Create),
            value: tx.value,
            access_list: Default::default(),
            input: tx.data.clone(),
        };
        let sk = k256::ecdsa::SigningKey::from_slice(key.as_slice()).unwrap();
        let (sig, rec) = sk.sign_prehash_recoverable(t.signature_hash().as_slice()).unwrap();
        let signed = t.into_signed(Signature::from_signature_and_parity(sig, rec.is_y_odd()));
        let raw = TxEnvelope::from(signed).encoded_2718();
        let hash = q("eth_sendRawTransaction", json!([format!("0x{}", hex::encode(raw))]));
        let hash = hash.as_str().unwrap_or_default().to_string();
        nonces.insert(tx.from, nonce + 1);
        let receipt = wait(url, &hash);
        let ok = receipt["status"] == "0x1";
        let mut line = format!("{}. {} {hash} gas {}", i + 1, if ok { "ok" } else { "REVERTED" }, hexnum(&receipt["gasUsed"]));
        let addr = match tx.to {
            None => receipt["contractAddress"].as_str().and_then(|a| a.parse::<Address>().ok()),
            Some(evm::CREATE2_DEPLOYER) if tx.data.len() > 32 => {
                let salt = B256::from_slice(&tx.data[..32]);
                Some(evm::CREATE2_DEPLOYER.create2(salt, alloy_primitives::keccak256(&tx.data[32..])))
            }
            _ => None,
        };
        if let Some(addr) = addr {
            if tx.created.is_some_and(|c| c != addr) {
                eprintln!("warning: {addr} differs from simulated address {} (nonce changed?)", tx.created.unwrap());
            }
            let name = tx.created.and_then(|c| names.get(&c).cloned()).unwrap_or("contract".into());
            line += &format!(" -> {name} at {addr}");
            // repeated names (e.g. proxies) keep the latest under name, earlier ones under name#n
            let key = if deployed.contains_key(&name) { format!("{name}#{}", i + 1) } else { name };
            deployed.insert(key, addr.to_checksum(None));
        }
        println!("{line}");
        if !ok {
            die("transaction reverted; stopping");
        }
    }
    if !deployed.is_empty() {
        let file = if network.contains("://") { chain_id.to_string() } else { network.to_string() };
        let path = p.root.join("deployments").join(format!("{file}.json"));
        let mut all: BTreeMap<String, String> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        all.extend(deployed);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap() + "\n").unwrap();
        println!("addresses saved to {}", path.strip_prefix(&p.root).unwrap_or(&path).display());
    }
}

fn wait(url: &str, hash: &str) -> Value {
    for _ in 0..600 {
        if let Ok(r) = rpc::call(url, "eth_getTransactionReceipt", json!([hash])) {
            if !r.is_null() {
                return r;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    die(&format!("timed out waiting for {hash}"))
}
