//! `plain deploy`: simulate (forked when a network is given), replay each transaction on its own for exact gas,
//! then sign everything, send without waiting in between, and verify the created contracts on explorers.
use crate::evm::{self, BroadcastTx, Cheats, Env, SENDER};
use crate::project::Project;
use crate::solc::Artifact;
use crate::verify::Record;
use crate::{decode, die, rpc};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, TxLegacy};
use alloy_dyn_abi::{DynSolType, JsonAbiExt, Specifier};
use alloy_eips::eip2718::Encodable2718;
use alloy_json_abi::JsonAbi;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Opts {
    pub target: String,
    pub args: Vec<String>,
    pub broadcast: bool,
    pub verify: bool,
}

/// The network as of now, fetched in one round trip while solc compiles.
pub struct Conn {
    pub name: String,
    pub url: String,
    pub chain: u64,
    block: u64,
    timestamp: u64,
    gas_limit: u64,
    base_fee: Option<u128>,
    prio: Option<u128>,
    gas_price: u128,
}

pub fn connect(p: &Project, name: &str) -> Conn {
    let net = rpc::network(p, name).unwrap_or_else(|e| die(&e));
    let calls = [("eth_chainId", json!([])), ("eth_getBlockByNumber", json!(["latest", false])), ("eth_maxPriorityFeePerGas", json!([])), ("eth_gasPrice", json!([]))];
    let r = rpc::batch_each(&net.url, &calls).unwrap_or_else(|e| die(&format!("{name}: {e}")));
    let chain = r[0].clone().unwrap_or_else(|e| die(&format!("{name}: {e}")));
    let b = r[1].clone().unwrap_or_else(|e| die(&format!("{name}: {e}")));
    if std::env::var_os("PLAIN_DEBUG").is_some() {
        eprintln!("rpc for {name}: {}", net.url);
    }
    Conn {
        name: name.into(),
        chain: hexnum(&chain) as u64,
        block: hexnum(&b["number"]) as u64,
        timestamp: hexnum(&b["timestamp"]) as u64,
        gas_limit: hexnum(&b["gasLimit"]) as u64,
        base_fee: b["baseFeePerGas"].is_string().then(|| hexnum(&b["baseFeePerGas"])),
        prio: r[2].as_ref().ok().map(hexnum),
        gas_price: r[3].as_ref().map(hexnum).unwrap_or(0),
        url: net.url,
    }
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

/// A transaction as the chain will run it: on its own, after the ones before it.
struct Sim {
    gas: u64,
    /// contracts it creates (directly, through the CREATE2 deployer, or nested), with their init code
    creates: Vec<(Address, Bytes)>,
}

pub fn run(p: &Project, out: crate::solc::Output, conn: Option<Conn>, o: Opts) {
    let arts = out.artifacts;
    decode::init(&arts);
    let key = std::env::var("PRIVATE_KEY").ok().map(|k| k.trim().parse::<B256>().unwrap_or_else(|_| die("PRIVATE_KEY must be 32 bytes hex")));
    let me = key.map(key_addr);
    if o.broadcast && conn.is_none() {
        die("--broadcast needs --network <name|chain id|url>");
    }
    if o.broadcast && key.is_none() {
        die("--broadcast needs PRIVATE_KEY in the environment");
    }

    let arts = Arc::new(arts);
    let fork = conn.as_ref().map(|c| Arc::new(evm::Fork::new(&c.url, Some(c.block))));
    let mut env = Env::new(conn.as_ref().map_or(31337, |c| c.chain));
    if let Some(c) = &conn {
        env.block.number = U256::from(c.block);
        env.block.timestamp = U256::from(c.timestamp);
    }
    let fresh_env = env.clone();
    let art = find(&arts, &o.target);
    let mut cheats = Cheats::new(arts.clone(), p.root.clone());
    cheats.default_broadcaster = Some(me.unwrap_or(SENDER));
    if let Some(k) = key {
        cheats.keys.insert(me.unwrap(), k);
    }
    let db = evm::base_db(evm::Remote(fork.clone(), None));
    cheats.track_creates = true;
    cheats.presume_creates = true;
    let is_script = art.file.ends_with(".s.sol") || (p.dirs.get(2).is_some_and(|d| art.file.starts_with(&format!("{d}/"))) && has_run(&art));

    // libraries go on chain first, deployed by the broadcaster (like forge script)
    let from = me.unwrap_or(SENDER);
    let mut libs = HashMap::new();
    let mut txs: Vec<BroadcastTx> = vec![];
    let db = crate::test::deploy_libs(db, &mut env, &mut cheats, &arts, &art, &mut libs, from, &mut Some(&mut txs));
    cheats.libs = libs.clone();
    if is_script {
        // the script contract itself only runs locally
        let (db, r) = crate::test::deploy(db, &mut env, &mut cheats, &arts, &art, &[], &mut libs);
        let addr = r.address.filter(|_| r.ok).unwrap_or_else(|| die(&format!("script constructor reverted: {}", decode::reason(&r.output))));
        let (_, r) = evm::transact(db, &mut env, &mut cheats, SENDER, TxKind::Call(addr), Bytes::from_static(&[0xc0, 0x40, 0x62, 0x26]), U256::ZERO, true);
        for l in &cheats.console {
            println!("log: {l}");
        }
        if !r.ok {
            die(&format!("{}.run() reverted: {}", art.name, decode::reason(&r.output)));
        }
        txs.append(&mut cheats.broadcasts);
    } else {
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
        let mut data = evm::link_code(&art.bytecode, &art.links, &libs).unwrap_or_else(|e| die(&format!("{}: {e}", art.name)));
        data.extend(alloy_dyn_abi::DynSolValue::Tuple(vals).abi_encode_params());
        let (_, r) = evm::transact(db, &mut env, &mut cheats, from, TxKind::Create, data.clone().into(), U256::ZERO, false);
        if let Some(a) = r.address {
            cheats.names.insert(a, art.name.clone());
        }
        if !r.ok {
            die(&format!("{} constructor reverted: {}", art.name, decode::reason(&r.output)));
        }
        txs.push(BroadcastTx { from, to: None, value: U256::ZERO, data: data.into(), created: r.address });
    }

    if txs.is_empty() {
        println!("nothing to broadcast (did the script call vm.startBroadcast?)");
        return;
    }
    // where contracts land was taken as empty (`Fork::presume`): one round trip checks it all, and the replay
    // runs again on the chain's own state when something was there after all (a CREATE2 salt used before)
    let settle = || fork.as_ref().is_some_and(|f| f.settle().unwrap_or_else(|e| die(&e.to_string())));
    settle();
    let mut sims = replay(fork.clone(), &fresh_env, &txs, &arts, p);
    while settle() {
        sims = replay(fork.clone(), &fresh_env, &txs, &arts, p);
    }
    for (i, (tx, s)) in txs.iter().zip(&sims).enumerate() {
        println!("{}. {} (from {}, gas {})", i + 1, describe(tx, &cheats.names), tx.from, s.gas);
        if std::env::var_os("PLAIN_DEBUG").is_some() {
            println!("   to {:?} value {} data {} bytes keccak {}", tx.to, tx.value, tx.data.len(), alloy_primitives::keccak256(&tx.data));
        }
    }
    let conn = match conn {
        Some(c) if o.broadcast => c,
        c => {
            println!("simulated {} transaction(s){}; add --broadcast to send", txs.len(), c.map(|c| format!(" against {}", c.name)).unwrap_or_default());
            return;
        }
    };
    let hashes = send(p, &conn, &txs, &sims, &cheats.keys, &cheats.names);

    // what explorers need to verify each created contract, kept in .plain/deploys for `plain verify`
    let compiler = crate::solc::long_version(&out.solc);
    let codes: Vec<(&Artifact, Vec<u8>)> = arts.iter().filter(|a| !a.bytecode.is_empty() && !a.rewritten && a.shard == 0).filter_map(|a| Some((a, evm::link_code(&a.bytecode, &a.links, &libs).ok()?))).collect();
    let mut inputs: HashMap<String, String> = HashMap::new();
    let mut recs = BTreeMap::new();
    for ((s, hash), tx) in sims.iter().zip(&hashes).zip(&txs) {
        for (addr, init) in &s.creates {
            let Some((a, code)) = codes.iter().filter(|(_, c)| init.starts_with(c)).max_by_key(|(_, c)| c.len()) else {
                eprintln!("warning: {addr} (created by {}) matches no artifact; not verifying it", describe(tx, &cheats.names));
                continue;
            };
            let input = inputs.entry(a.file.clone()).or_insert_with(|| crate::verify::save_input(p, &crate::verify::input(p, &a.file))).clone();
            let used: BTreeMap<String, Address> = a.links.as_object().into_iter().flatten().flat_map(|(f, ns)| ns.as_object().into_iter().flatten().map(move |(n, _)| format!("{f}:{n}"))).filter_map(|id| Some((id.clone(), *libs.get(&id)?))).collect();
            recs.insert(*addr, Record { name: a.name.clone(), contract: format!("{}:{}", a.file, a.name), args: format!("0x{}", hex::encode(&init[code.len()..])), tx: Some(*hash), libs: used, compiler: compiler.clone(), input, verified: vec![] });
        }
    }
    let mut all = crate::verify::load(p, conn.chain);
    all.extend(recs.clone());
    crate::verify::save(p, conn.chain, &all);
    if recs.is_empty() {
        return;
    }
    if !o.verify || matches!(conn.chain, 31337 | 1337) {
        match o.verify {
            true => println!("not verifying on a dev chain"),
            false => println!("not verifying; later: plain verify --network {}", conn.name),
        }
        return;
    }
    let ok = crate::verify::run(p, conn.chain, &mut recs);
    all.extend(recs);
    crate::verify::save(p, conn.chain, &all);
    if !ok {
        println!("retry the failed ones with: plain verify --network {}", conn.name);
    }
}

/// Runs each transaction on its own, in order, on the state as the chain has it: the gas each needs, what it
/// creates, and an early stop if one would fail on chain (a script can depend on things a transaction can't).
fn replay(fork: Option<Arc<evm::Fork>>, env: &Env, txs: &[BroadcastTx], arts: &Arc<Vec<Artifact>>, p: &Project) -> Vec<Sim> {
    let mut db = match &fork {
        Some(_) => evm::Db::new(evm::Remote(fork.clone(), None)),
        None => evm::base_db(evm::Remote(None, None)),
    };
    if fork.is_none() {
        for tx in txs {
            db.insert_account_info(tx.from, revm::state::AccountInfo::from_balance(U256::from(u128::MAX)));
        }
    }
    let mut cheats = Cheats::new(arts.clone(), p.root.clone());
    cheats.track_creates = true;
    cheats.presume_creates = true;
    let mut vm = evm::Vm::new(db, env, &mut cheats);
    let mut out = vec![];
    for (i, tx) in txs.iter().enumerate() {
        let n = vm.cheats().creates.len();
        let r = vm.call(tx.from, tx.to.map(TxKind::Call).unwrap_or(TxKind::Create), tx.data.clone(), tx.value, true);
        if !r.ok {
            die(&format!("transaction {} would revert on chain: {}", i + 1, decode::reason(&r.output)));
        }
        let creates = vm.cheats().creates[n..].to_vec();
        if tx.to == Some(evm::CREATE2_DEPLOYER) && creates.is_empty() {
            die(&format!("transaction {} goes through the CREATE2 deployer {}, which this chain doesn't have", i + 1, evm::CREATE2_DEPLOYER));
        }
        out.push(Sim { gas: r.gas, creates });
    }
    out
}

/// Signs every transaction up front, sends them back to back (nonces keep them in order), then waits for all
/// receipts at once. Returns the transaction hashes.
fn send(p: &Project, conn: &Conn, txs: &[BroadcastTx], sims: &[Sim], keys: &HashMap<Address, B256>, names: &HashMap<Address, String>) -> Vec<B256> {
    let url = &conn.url;
    let mut senders: Vec<Address> = txs.iter().map(|t| t.from).collect();
    senders.sort();
    senders.dedup();
    let calls: Vec<(&str, Value)> = senders.iter().flat_map(|a| [("eth_getTransactionCount", json!([a, "pending"])), ("eth_getBalance", json!([a, "latest"]))]).collect();
    let r = rpc::batch(url, &calls).unwrap_or_else(|e| die(&e));
    let mut nonces: HashMap<Address, u64> = senders.iter().enumerate().map(|(i, a)| (*a, hexnum(&r[2 * i]) as u64)).collect();
    let balances: HashMap<Address, u128> = senders.iter().enumerate().map(|(i, a)| (*a, hexnum(&r[2 * i + 1]))).collect();
    // fees: EIP-1559 when the chain has a base fee, with room for it to double
    let (max_fee, prio) = match conn.base_fee {
        Some(base) => {
            let prio = conn.prio.unwrap_or_else(|| conn.gas_price.saturating_sub(base).max(1_000_000));
            (base * 2 + prio, prio)
        }
        None => (conn.gas_price + conn.gas_price / 10, 0),
    };
    let mut cost: HashMap<Address, u128> = HashMap::new();
    let mut signed = vec![];
    for (tx, s) in txs.iter().zip(sims) {
        let key = keys.get(&tx.from).unwrap_or_else(|| die(&format!("no private key for broadcaster {}", tx.from)));
        let nonce = nonces.get_mut(&tx.from).unwrap();
        // 30% over the simulated gas, as forge does; within Osaka's per-transaction cap when that's enough
        let mut gas = s.gas * 13 / 10;
        if s.gas < 1 << 24 {
            gas = gas.min(1 << 24);
        }
        if conn.gas_limit > 0 {
            gas = gas.min(conn.gas_limit);
        }
        *cost.entry(tx.from).or_default() += gas as u128 * max_fee + tx.value.to::<u128>();
        let to = tx.to.map(TxKind::Call).unwrap_or(TxKind::Create);
        let sk = k256::ecdsa::SigningKey::from_slice(key.as_slice()).unwrap();
        let sign = |h: B256| {
            let (sig, rec) = sk.sign_prehash_recoverable(h.as_slice()).unwrap();
            Signature::from_signature_and_parity(sig, rec.is_y_odd())
        };
        let env: TxEnvelope = if conn.base_fee.is_some() {
            let t = TxEip1559 { chain_id: conn.chain, nonce: *nonce, gas_limit: gas, max_fee_per_gas: max_fee, max_priority_fee_per_gas: prio, to, value: tx.value, access_list: Default::default(), input: tx.data.clone() };
            let sig = sign(t.signature_hash());
            t.into_signed(sig).into()
        } else {
            let t = TxLegacy { chain_id: Some(conn.chain), nonce: *nonce, gas_price: max_fee, gas_limit: gas, to, value: tx.value, input: tx.data.clone() };
            let sig = sign(t.signature_hash());
            t.into_signed(sig).into()
        };
        *nonce += 1;
        signed.push((*env.tx_hash(), env.encoded_2718()));
    }
    for (a, need) in &cost {
        let have = balances[a];
        if have < *need {
            die(&format!("{a} has {} but these transactions can cost up to {} (in the chain's native token, at a {} gwei max fee)", eth(have), eth(*need), max_fee as f64 / 1e9));
        }
    }
    let t = Instant::now();
    let raw = |i: usize| ("eth_sendRawTransaction", json!([format!("0x{}", hex::encode(&signed[i].1))]));
    // the first on its own: a fee, balance or chain problem stops the deploy before anything else is in the pool.
    // The rest in one batch, in nonce order (a round trip each would cost seconds on a remote RPC)
    let mut results = vec![rpc::call(url, raw(0).0, raw(0).1)];
    if results[0].is_ok() && signed.len() > 1 {
        let calls: Vec<(&str, Value)> = (1..signed.len()).map(raw).collect();
        results.extend(rpc::batch_each(url, &calls).unwrap_or_else(|e| vec![Err(e); calls.len()]));
    }
    let mut sent = vec![];
    for (i, r) in results.into_iter().enumerate() {
        let r = match r {
            // a node that ran the batch out of order: again, now that the nonces before it are in
            Err(e) if e.contains("nonce too high") || e.contains("gap") => rpc::call(url, raw(i).0, raw(i).1),
            r => r,
        };
        match r {
            // (nonce too low past the first: ours, mined while a rate-limited batch was retried)
            Err(e) if !["already known", "known transaction", "already imported"].iter().any(|k| e.to_lowercase().contains(k)) && !(i > 0 && e.contains("nonce too low")) => {
                eprintln!("transaction {} rejected: {e}", i + 1);
                if i + 1 < signed.len() && i > 0 {
                    eprintln!("the {} after it went out in the same batch and wait behind its nonce", signed.len() - i - 1);
                }
                break;
            }
            _ => sent.push(signed[i].0),
        }
    }
    println!("sent {}/{} transactions to {} in {:.1?}; waiting for receipts", sent.len(), signed.len(), conn.name, t.elapsed());

    let mut receipts: Vec<Option<Value>> = vec![None; sent.len()];
    let mut next = 0;
    let mut deployed = BTreeMap::new();
    let mut failed = false;
    let mut last = Instant::now();
    // block number of the last receipt (or resend): a block passing without any of ours means the node dropped
    // them (anvil does when one doesn't fit in a block; public mempools evict): send what's pending again,
    // nodes answer "already known" otherwise
    let mut seen = 0;
    while next < sent.len() {
        let pending: Vec<usize> = (0..sent.len()).filter(|i| receipts[*i].is_none()).collect();
        // a sender's transactions are mined in nonce order: ask for each sender's first pending receipt, and for
        // the rest once one of those is in (public RPCs rate-limit per call, batched or not)
        let heads: Vec<usize> = pending.iter().copied().filter(|&i| !pending.iter().any(|&j| j < i && txs[j].from == txs[i].from)).collect();
        let (mut block, mut progress, mut polled) = (0, false, false);
        for ask in [heads, pending.clone()] {
            let ask: Vec<usize> = ask.into_iter().filter(|i| receipts[*i].is_none()).collect();
            if ask.is_empty() || (polled && !progress) {
                break;
            }
            let mut calls: Vec<(&str, Value)> = ask.iter().map(|i| ("eth_getTransactionReceipt", json!([sent[*i]]))).collect();
            calls.push(("eth_blockNumber", json!([])));
            let Ok(mut rs) = rpc::batch_each(url, &calls) else { break };
            polled = true;
            block = rs.pop().and_then(|b| b.ok()).map(|b| hexnum(&b)).unwrap_or(0);
            for (i, r) in ask.iter().zip(rs) {
                if let Ok(r) = r.and_then(|r| if r.is_null() { Err(String::new()) } else { Ok(r) }) {
                    receipts[*i] = Some(r);
                    last = Instant::now();
                    progress = true;
                }
            }
        }
        if polled {
            if progress || seen == 0 {
                seen = block;
            } else if block > seen {
                rpc::batch_each(url, &pending.iter().take(16).map(|&i| raw(i)).collect::<Vec<_>>()).ok();
                seen = block;
            }
        }
        while let Some(Some(receipt)) = receipts.get(next) {
            let (i, tx) = (next, &txs[next]);
            let ok = receipt["status"] == "0x1";
            failed |= !ok;
            let mut line = format!("{}. {} {} gas {}", i + 1, if ok { "ok" } else { "REVERTED" }, sent[i], hexnum(&receipt["gasUsed"]));
            let addr = match tx.to {
                None => receipt["contractAddress"].as_str().and_then(|a| a.parse::<Address>().ok()),
                Some(evm::CREATE2_DEPLOYER) => sims[i].creates.first().map(|c| c.0),
                _ => None,
            };
            if let Some(addr) = addr.filter(|_| ok) {
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
            next += 1;
        }
        if next < sent.len() {
            if last.elapsed() > Duration::from_secs(300) {
                die(&format!("no receipt for {} after 5 minutes", sent[next]));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    if !deployed.is_empty() {
        let file = if conn.name.contains("://") { conn.chain.to_string() } else { conn.name.clone() };
        let path = p.root.join("deployments").join(format!("{file}.json"));
        let mut all: BTreeMap<String, String> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        all.extend(deployed);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap() + "\n").unwrap();
        println!("addresses saved to {}", path.strip_prefix(&p.root).unwrap_or(&path).display());
    }
    if failed || sent.len() < signed.len() {
        die("not every transaction went through (see above)");
    }
    sent
}

fn eth(wei: u128) -> String {
    format!("{:.6}", wei as f64 / 1e18)
}
