//! `plain deploy`: simulate (forked when a network is given), replay each transaction on its own for exact gas,
//! then sign everything, send without waiting in between, and verify the created contracts on explorers.
use crate::evm::{self, BroadcastTx, Cheats, Env, SENDER};
use crate::project::Project;
use crate::solc::Artifact;
use crate::verify::Record;
use crate::{decode, die, rpc};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, TxLegacy};
use alloy_dyn_abi::{DynSolType, DynSolValue, JsonAbiExt, Specifier};
use alloy_eips::eip2718::Encodable2718;
use alloy_json_abi::{Function, JsonAbi, Param};
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
    /// a keystore (see account.rs) to sign with, instead of $PRIVATE_KEY
    pub account: Option<String>,
    /// deploy behind an OpenZeppelin proxy, `args` going to initialize()
    pub proxy: bool,
    /// point this proxy at `target`, `args` going to the plan's call
    pub upgrade: Option<crate::upgrade::Plan>,
    /// with --broadcast: show what will be sent and what it can cost, and send only on a yes
    pub confirm: bool,
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
    // a URL given as the network is shown without what may be a key
    let name = &if name.contains("://") { rpc::shown(name) } else { name.to_string() };
    let calls = [("eth_chainId", json!([])), ("eth_getBlockByNumber", json!(["latest", false])), ("eth_maxPriorityFeePerGas", json!([])), ("eth_gasPrice", json!([]))];
    let r = rpc::batch_each(&net.url, &calls).unwrap_or_else(|e| die(&format!("{name}: {e}")));
    let chain = r[0].clone().unwrap_or_else(|e| die(&format!("{name}: {e}")));
    let b = r[1].clone().unwrap_or_else(|e| die(&format!("{name}: {e}")));
    if !b.is_object() {
        die(&format!("{name}: the RPC answered {b} for the latest block"));
    }
    if std::env::var_os("PLAIN_DEBUG").is_some() {
        eprintln!("rpc for {name}: {}", rpc::shown(&net.url));
    }
    let fee = |r: &Result<Value, String>| r.as_ref().ok().and_then(|v| rpc::quantity(v).ok()).map(|v| v.saturating_to::<u128>());
    Conn {
        name: name.into(),
        chain: hexnum(&chain, &format!("{name}: eth_chainId")) as u64,
        block: hexnum(&b["number"], &format!("{name}: the latest block's number")) as u64,
        timestamp: hexnum(&b["timestamp"], &format!("{name}: the latest block's timestamp")) as u64,
        gas_limit: hexnum(&b["gasLimit"], &format!("{name}: the latest block's gas limit")) as u64,
        // blocks before London have no base fee
        base_fee: (!b["baseFeePerGas"].is_null()).then(|| hexnum(&b["baseFeePerGas"], &format!("{name}: the latest block's base fee"))),
        prio: fee(&r[2]),
        gas_price: fee(&r[3]).unwrap_or(0),
        url: net.url,
    }
}

/// deployments/<network>.json, where deploys save addresses by name.
pub fn addresses_path(p: &Project, conn: &Conn) -> std::path::PathBuf {
    let file = if conn.name.contains("://") { conn.chain.to_string() } else { conn.name.clone() };
    p.root.join("deployments").join(format!("{file}.json"))
}

pub fn addresses(p: &Project, conn: &Conn) -> BTreeMap<String, String> {
    std::fs::read(addresses_path(p, conn)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Command-line arguments as `params`, ABI-encoded.
fn encode(params: &[Param], args: &[String], what: &str) -> Vec<u8> {
    if params.len() != args.len() {
        die(&format!("{what} takes {} args ({}), got {}", params.len(), params.iter().map(|i| i.ty.clone()).collect::<Vec<_>>().join(", "), args.len()));
    }
    let vals: Vec<DynSolValue> = params
        .iter()
        .zip(args)
        .map(|(i, a)| {
            let ty: DynSolType = i.resolve().unwrap();
            // an address in mixed case carries an EIP-55 checksum: one that doesn't match is a typo, not an address
            let mixed = a.chars().any(|c| c.is_ascii_uppercase()) && a.chars().skip(2).any(|c| c.is_ascii_lowercase());
            if ty == DynSolType::Address && mixed && Address::parse_checksummed(a.trim(), None).is_err() {
                die(&format!("{a} has a bad checksum (a typo?); write it in lower case to skip the check"));
            }
            ty.coerce_str(a).unwrap_or_else(|e| die(&format!("arg {:?} as {}: {e}", a, i.ty)))
        })
        .collect();
    DynSolValue::Tuple(vals).abi_encode_params()
}

/// Calldata for `name(args)` on `abi`. A signature ("initialize(uint256)") picks the overload; a bare name picks
/// the one these arguments fit, and two that both fit are an error rather than a guess.
fn calldata(abi: &JsonAbi, contract: &str, name: &str, args: &[String]) -> Vec<u8> {
    let fs: Vec<&Function> = match name.contains('(') {
        true => abi.functions().filter(|f| f.signature() == name.replace(' ', "")).collect(),
        false => abi.function(name).map(|v| v.iter().collect()).unwrap_or_default(),
    };
    if fs.is_empty() {
        die(&format!("{contract} has no function {name}"));
    }
    let fits: Vec<&Function> = fs.iter().copied().filter(|f| f.inputs.len() == args.len() && f.inputs.iter().zip(args).all(|(p, a)| p.resolve().is_ok_and(|t: DynSolType| t.coerce_str(a).is_ok()))).collect();
    let f = match fits.as_slice() {
        [f] => *f,
        [] => fs[0],
        many => die(&format!("{contract}.{name} is overloaded and these arguments fit {}; name one, as in --call '{}'", many.iter().map(|f| f.signature()).collect::<Vec<_>>().join(" and "), many[0].signature())),
    };
    [f.selector().to_vec(), encode(&f.inputs, args, &format!("{contract}.{name}"))].concat()
}

fn call_sig(sig: &str, args: &[DynSolValue]) -> Bytes {
    Function::parse(sig).unwrap().abi_encode_input(args).unwrap().into()
}

fn key_addr(k: B256) -> Address {
    evm::pk_to_addr(k.into()).unwrap_or_else(|e| die(&e))
}

/// A number the network answered, or the deploy stops: a guessed 0 would be a wrong chain id, nonce or balance.
fn hexnum(v: &Value, what: &str) -> u128 {
    rpc::quantity(v).and_then(|n| u128::try_from(n).map_err(|_| format!("{n} is over 128 bits"))).unwrap_or_else(|e| die(&format!("{what}: {e}")))
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

/// Why a simulated call failed. Not reading the chain's state (a rate limit, a flaky RPC) ends the deploy as what
/// it is, rather than passing for a revert.
fn why(out: &[u8]) -> String {
    let r = decode::reason(out);
    if let Some(e) = r.strip_prefix("EvmError: database error: ") {
        die(&format!("couldn't read the chain's state from the RPC ({e}); try again, or use your own RPC with <NAME>_RPC_URL"));
    }
    r
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
            match names.get(&to) {
                Some(n) => format!("call {n} ({to}).{call}"),
                None => format!("call {to}.{call}"),
            }
        }
    }
}

/// A transaction as the chain will run it: on its own, after the ones before it.
struct Sim {
    /// gas used, as the receipt will say
    gas: u64,
    /// gas spent before refunds: what the transaction has to be given
    spent: u64,
    /// contracts it creates (directly, through the CREATE2 deployer, or nested), with their init code
    creates: Vec<(Address, Bytes)>,
}

/// What a simulation of the deploy produced.
struct Planned {
    txs: Vec<BroadcastTx>,
    cheats: Cheats,
    libs: HashMap<String, Address>,
    /// proxies this deploys or upgrades, to point Etherscan at their implementation
    proxies: Vec<Address>,
}

/// Whether an RPC is a node on this machine (a local chain, or a fork of a live one): nothing it deploys is verified.
fn local(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, port)| if port.bytes().all(|b| b.is_ascii_digit()) { h } else { host });
    matches!(host.trim_matches(['[', ']']), "127.0.0.1" | "localhost" | "::1" | "0.0.0.0") || host.ends_with(".localhost")
}

pub fn run(p: &Project, out: crate::solc::Output, conn: Option<Conn>, o: Opts) {
    let mut arts = out.artifacts;
    let target = find(&arts, &o.target);
    // the proxy --proxy deploys, from the project's OpenZeppelin: exactly that artifact, never one that shares its name
    let proxy = o.proxy.then(|| {
        let uups = target.abi.as_array().is_some_and(|f| f.iter().any(|x| x["name"] == "proxiableUUID"));
        let (proxy, compiled) = crate::upgrade::proxy_artifacts(p, &out.solc, uups);
        arts.extend(compiled);
        proxy
    });
    if o.upgrade.as_ref().is_some_and(|u| u.admin.is_some()) {
        // ProxyAdmin's ABI (OpenZeppelin 5's and 4's), so the upgrade shows as a call rather than a selector
        let abi = json!([{"type": "function", "name": "upgradeAndCall", "stateMutability": "payable", "outputs": [],
            "inputs": [{"name": "proxy", "type": "address"}, {"name": "implementation", "type": "address"}, {"name": "data", "type": "bytes"}]},
            {"type": "function", "name": "upgrade", "stateMutability": "nonpayable", "outputs": [],
            "inputs": [{"name": "proxy", "type": "address"}, {"name": "implementation", "type": "address"}]}]);
        arts.push(Artifact { name: "ProxyAdmin".into(), file: String::new(), abi, bytecode: String::new(), deployed: String::new(), links: Value::Null, deployed_links: Value::Null, immutables: Value::Null, shard: 0, tests: vec![], deploys: vec![], rewritten: false, profile: 0 });
    }
    decode::init(&arts);
    if o.broadcast && conn.is_none() {
        die("--broadcast needs --network <name|chain id|url>");
    }
    let recorded = o.account.as_deref().and_then(crate::account::address);
    // with --confirm and a keystore that records its address, the password comes after the plan is shown
    let late = o.broadcast && o.confirm && recorded.is_some();
    let key = match &o.account {
        Some(a) if (o.broadcast && !late) || recorded.is_none() => Some(crate::account::unlock(a)),
        Some(_) => None,
        None => crate::project::var("PRIVATE_KEY").filter(|k| !k.is_empty()).map(|k| k.trim().parse::<B256>().unwrap_or_else(|_| die("PRIVATE_KEY must be 32 bytes hex"))),
    };
    // a dev chain (plain node, anvil, hardhat) deploys from its first funded account unless told otherwise
    let dev = key.is_none() && o.account.is_none() && conn.as_ref().is_some_and(|c| c.chain == 31337);
    let key = key.or_else(|| dev.then(|| crate::node::KEYS[0].parse().unwrap()));
    let me = key.map(key_addr).or(recorded);
    if o.broadcast && key.is_none() && !late {
        die("--broadcast needs a deployer: --account <name> (`plain account new <name>` makes one), or PRIVATE_KEY in the environment");
    }
    if o.broadcast && dev {
        println!("sending from the dev account {}", me.unwrap());
    }

    let arts = Arc::new(arts);
    let fork = conn.as_ref().map(|c| Arc::new(evm::Fork::new(&c.url, Some(c.block))));
    let mut fresh_env = Env::new(conn.as_ref().map_or(31337, |c| c.chain));
    if let Some(c) = &conn {
        fresh_env.block.number = U256::from(c.block);
        fresh_env.block.timestamp = U256::from(c.timestamp);
    }
    // the rules the code was compiled for, and a chain's size limits on contracts (EIP-170, EIP-3860)
    fresh_env.spec = evm::spec(&out.evm);
    fresh_env.strict = true;
    let art = find(&arts, &o.target);
    let is_script = art.file.ends_with(".s.sol") || (p.dirs.get(2).is_some_and(|d| art.file.starts_with(&format!("{d}/"))) && has_run(&art));
    if is_script && o.proxy {
        die("--proxy is for a contract; a script deploys its own proxies (new ERC1967Proxy(...))");
    }
    if is_script && !o.args.is_empty() {
        die(&format!("a script takes no arguments: plain runs {}.run()", art.name));
    }
    let from = me.unwrap_or(SENDER);

    let simulate = || -> Planned {
        let mut env = fresh_env.clone();
        let mut cheats = Cheats::new(arts.clone(), p.root.clone(), Arc::new(p.access.clone()));
        cheats.default_broadcaster = Some(from);
        if let Some(k) = key {
            cheats.keys.insert(key_addr(k), k);
        }
        let db = evm::base_db(evm::Remote(fork.clone(), None));
        cheats.track_creates = true;
        cheats.presume_creates = true;
        // libraries go on chain first, deployed by the broadcaster (like forge script)
        let mut libs = HashMap::new();
        let mut txs: Vec<BroadcastTx> = vec![];
        let db = crate::test::deploy_libs(db, &mut env, &mut cheats, &arts, &art, &mut libs, from, &mut Some(&mut txs));
        let mut proxies: Vec<Address> = vec![];
        cheats.libs = libs.clone();
        if is_script {
            // the script contract itself only runs locally
            let (db, r) = crate::test::deploy(db, &mut env, &mut cheats, &arts, &art, &[], &mut libs);
            let addr = r.address.filter(|_| r.ok).unwrap_or_else(|| die(&format!("script constructor reverted: {}", why(&r.output))));
            // run() sees the deployer as msg.sender and tx.origin, as with forge, so `new X(msg.sender)` and
            // transferOwnership(msg.sender) name the deployer
            cheats.act_as(from);
            let (_, r) = evm::transact(db, &mut env, &mut cheats, SENDER, TxKind::Call(addr), Bytes::from_static(&[0xc0, 0x40, 0x62, 0x26]), U256::ZERO, true);
            for l in &cheats.console {
                println!("log: {l}");
            }
            if !r.ok {
                // a bare revert often bubbles up from a frame that hit a limit: say which
                let inner = match cheats.last_halt {
                    Some(revm::interpreter::InstructionResult::CreateContractSizeLimit) => " (a contract it creates is over 24 KB, the EIP-170 limit chains enforce)".into(),
                    Some(revm::interpreter::InstructionResult::CreateInitCodeSizeLimit) => " (an init code over 48 KB, the EIP-3860 limit)".into(),
                    Some(h) => format!(" (inside: {h:?})"),
                    None => String::new(),
                };
                die(&format!("{}.run() reverted: {}{inner}", art.name, why(&r.output)));
            }
            txs.append(&mut cheats.broadcasts);
            return Planned { txs, cheats, libs, proxies };
        }
        let abi: JsonAbi = serde_json::from_value(art.abi.clone()).unwrap();
        let inputs = abi.constructor().map(|c| c.inputs.clone()).unwrap_or_default();
        let code = evm::link_code(&art.bytecode, &art.links, &libs).unwrap_or_else(|e| die(&format!("{}: {e}", art.name)));
        if (o.proxy || o.upgrade.is_some()) && !inputs.is_empty() {
            die(&format!("{}'s constructor takes arguments, which a proxy never passes; an upgradeable contract takes them in initialize()", art.name));
        }
        // the contract itself; behind a proxy, as the implementation
        let what = if o.proxy || o.upgrade.is_some() { "implementation" } else { "constructor" };
        let data: Bytes = match o.proxy || o.upgrade.is_some() {
            true => code.into(),
            false => [code, encode(&inputs, &o.args, &format!("{} constructor", art.name))].concat().into(),
        };
        let (db, r) = evm::transact(db, &mut env, &mut cheats, from, TxKind::Create, data.clone(), U256::ZERO, true);
        let addr = r.address.filter(|_| r.ok).unwrap_or_else(|| die(&format!("{} {what} reverted: {}", art.name, why(&r.output))));
        let name = o.upgrade.as_ref().map_or(art.name.clone(), |u| u.name.clone());
        cheats.names.insert(addr, if what == "implementation" { format!("{name}_Implementation") } else { name });
        txs.push(BroadcastTx { from, to: None, value: U256::ZERO, data, created: Some(addr) });
        if let Some(proxy) = &proxy {
            // UUPS implementations upgrade themselves and go behind a plain ERC1967Proxy; anything else behind a
            // TransparentUpgradeableProxy, whose ProxyAdmin the deployer owns
            let init = match abi.function("initialize") {
                Some(_) => calldata(&abi, &art.name, "initialize", &o.args),
                None if o.args.is_empty() => vec![],
                None => die(&format!("{} has no initialize() to take those arguments", art.name)),
            };
            let args = match proxy.name == "ERC1967Proxy" {
                true => vec![DynSolValue::Address(addr), DynSolValue::Bytes(init)],
                false => vec![DynSolValue::Address(addr), DynSolValue::Address(from), DynSolValue::Bytes(init)],
            };
            let data: Bytes = [hex::decode(&proxy.bytecode).unwrap(), DynSolValue::Tuple(args).abi_encode_params()].concat().into();
            let (mut db, r) = evm::transact(db, &mut env, &mut cheats, from, TxKind::Create, data.clone(), U256::ZERO, true);
            let px = r.address.filter(|_| r.ok).unwrap_or_else(|| die(&format!("{} reverted while setting up the proxy (in initialize?): {}", art.name, why(&r.output))));
            let now = revm::Database::storage(&mut db, px, crate::upgrade::IMPLEMENTATION.into()).map(|w| Address::from_word(w.into())).unwrap_or_else(|e| die(&e.to_string()));
            if now != addr {
                die(&format!("the proxy points at {now}, not at the implementation {addr}"));
            }
            cheats.names.insert(px, art.name.clone());
            txs.push(BroadcastTx { from, to: None, value: U256::ZERO, data, created: Some(px) });
            proxies.push(px);
        } else if let Some(u) = &o.upgrade {
            if u.admin.is_none() && !(abi.function("upgradeToAndCall").is_some() && abi.function("proxiableUUID").is_some()) {
                die(&format!("{} is a UUPS proxy, and {} has no upgradeToAndCall/proxiableUUID: after this upgrade nothing could upgrade it again", u.name, art.name));
            }
            let call = match &u.call {
                Some(f) => calldata(&abi, &art.name, f, &o.args),
                None if o.args.is_empty() => vec![],
                None => die("arguments after the contract go to the function --call names"),
            };
            let (proxy_, imp) = (DynSolValue::Address(u.proxy), DynSolValue::Address(addr));
            let tries: Vec<(Address, Bytes)> = match u.admin {
                Some(admin) => {
                    cheats.names.insert(admin, format!("{}'s ProxyAdmin", u.name));
                    let mut t = vec![(admin, call_sig("upgradeAndCall(address,address,bytes)", &[proxy_.clone(), imp.clone(), DynSolValue::Bytes(call.clone())]))];
                    // OpenZeppelin 4's ProxyAdmin calls the new implementation even with no data; its upgrade doesn't
                    if call.is_empty() {
                        t.push((admin, call_sig("upgrade(address,address)", &[proxy_, imp])));
                    }
                    t
                }
                // OpenZeppelin 4's UUPS calls the new implementation even with no data; its upgradeTo doesn't
                None if call.is_empty() && abi.function("upgradeTo").is_some() => vec![(u.proxy, call_sig("upgradeTo(address)", &[imp]))],
                None => vec![(u.proxy, call_sig("upgradeToAndCall(address,bytes)", &[imp, DynSolValue::Bytes(call)]))],
            };
            let mut why_not = String::new();
            for (to, data) in tries {
                let (mut db, r) = evm::transact(db.clone(), &mut env, &mut cheats, from, TxKind::Call(to), data.clone(), U256::ZERO, true);
                if !r.ok {
                    why_not = format!("upgrading {} reverted: {} (may {from} upgrade it?)", u.name, why(&r.output));
                    continue;
                }
                // a call that succeeds isn't an upgrade (an admin that's a plain account takes any call): the proxy
                // has to point at the new implementation afterwards
                let now = revm::Database::storage(&mut db, u.proxy, crate::upgrade::IMPLEMENTATION.into()).map(|w| Address::from_word(w.into())).unwrap_or_else(|e| die(&e.to_string()));
                if now != addr {
                    why_not = format!("the upgrade call succeeds, but {} would still run {now}: its admin {to} isn't a contract plain knows how to upgrade through", u.name);
                    continue;
                }
                cheats.names.insert(u.proxy, u.name.clone());
                txs.push(BroadcastTx { from, to: Some(to), value: U256::ZERO, data, created: None });
                proxies.push(u.proxy);
                return Planned { txs, cheats, libs, proxies };
            }
            die(&why_not);
        }
        Planned { txs, cheats, libs, proxies }
    };

    // where contracts land is taken as empty (`Fork::presume`): one round trip checks it all, and the simulation runs
    // again on the chain's own state when something was there after all (a CREATE2 salt used before)
    let settle = || fork.as_ref().is_some_and(|f| f.settle().unwrap_or_else(|e| die(&e.to_string())));
    let Planned { txs, cheats, libs, proxies } = loop {
        let plan = simulate();
        if !settle() {
            break plan;
        }
    };
    if txs.is_empty() {
        println!("nothing to broadcast (did the script call vm.startBroadcast?)");
        return;
    }
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
    // what sending will cost, before anyone is asked for a password
    if let (Some(c), Some(who)) = (conn.as_ref().filter(|c| c.chain != 31337 && (!o.broadcast || o.confirm)), me) {
        let (need, sym) = (cost(c, who, &txs, &sims), rpc::symbol(c.chain));
        if let Ok(have) = rpc::call(&c.url, "eth_getBalance", json!([who, "latest"])).and_then(|b| rpc::quantity(&b)).map(|b| b.saturating_to::<u128>()) {
            let short = if have < need { "; that's not enough, so fund it first" } else { "" };
            println!("{who} has {} {sym}; sending can cost up to {} {sym}{short}", eth(have), eth(need));
        }
    }
    let conn = match conn {
        Some(c) if o.broadcast => c,
        c => {
            println!("simulated {} transaction(s){}; add --broadcast to send", txs.len(), c.map(|c| format!(" against {}", c.name)).unwrap_or_default());
            return;
        }
    };
    if o.confirm && !crate::ui::confirm(&format!("Send {} transaction(s) to {}?", txs.len(), conn.name)) {
        println!("nothing sent");
        return;
    }
    let mut keys = cheats.keys.clone();
    if late {
        let k = crate::account::unlock(o.account.as_deref().unwrap());
        if Some(key_addr(k)) != me {
            die("that keystore's key doesn't match the address it records");
        }
        keys.insert(key_addr(k), k);
    }
    check_bytecode(p, &out.solc, &arts, &libs, &txs);

    // what explorers need to verify each contract the deploy creates, from the sources just checked against the
    // bytecode, saved before anything is sent (kept in .plain/deploys for `plain verify` and upgrade checks)
    let compiler = crate::solc::long_version(&out.solc);
    let codes: Vec<(&Artifact, Vec<u8>)> = arts.iter().filter(|a| !a.bytecode.is_empty() && !a.rewritten && a.shard == 0).filter_map(|a| Some((a, evm::link_code(&a.bytecode, &a.links, &libs).ok()?))).collect();
    let mut inputs: HashMap<String, String> = HashMap::new();
    let planned: Vec<Vec<(Address, Record)>> = sims
        .iter()
        .zip(&txs)
        .map(|(s, tx)| {
            s.creates
                .iter()
                .filter_map(|(addr, init)| {
                    let Some((a, code)) = codes.iter().filter(|(_, c)| init.starts_with(c)).max_by_key(|(_, c)| c.len()) else {
                        eprintln!("warning: {addr} (created by {}) matches no artifact; it won't be verified", describe(tx, &cheats.names));
                        return None;
                    };
                    let input = inputs.entry(a.file.clone()).or_insert_with(|| crate::verify::save_input(p, &crate::verify::input(p, &a.file))).clone();
                    let used: BTreeMap<String, Address> = a.links.as_object().into_iter().flatten().flat_map(|(f, ns)| ns.as_object().into_iter().flatten().map(move |(n, _)| format!("{f}:{n}"))).filter_map(|id| Some((id.clone(), *libs.get(&id)?))).collect();
                    Some((*addr, Record { name: a.name.clone(), contract: format!("{}:{}", a.file, a.name), args: format!("0x{}", hex::encode(&init[code.len()..])), tx: None, libs: used, compiler: compiler.clone(), input, verified: vec![], proxy: proxies.contains(addr) }))
                })
                .collect()
        })
        .collect();

    let landed = send(p, &conn, &txs, &sims, &keys, &cheats.names, fork.as_ref().map_or(conn.block, |f| f.block()));
    let all_ok = landed.iter().all(|l| l.is_some_and(|(_, ok)| ok));
    // records for what made it on chain, whatever happened to the rest
    let mut recs = BTreeMap::new();
    for (l, created) in landed.iter().zip(planned) {
        if let Some((hash, true)) = l {
            for (addr, mut r) in created {
                r.tx = Some(*hash);
                recs.insert(addr, r);
            }
        }
    }
    let mut all = crate::verify::load(p, conn.chain);
    all.extend(recs.clone());
    crate::verify::save(p, conn.chain, &all);
    if o.proxy && all_ok {
        println!("{} is upgradeable: plain upgrade {} --network {}", art.name, art.name, conn.name);
    }
    if !recs.is_empty() {
        if !o.verify {
            println!("not verifying; later: plain verify --network {}", conn.name);
        } else if matches!(conn.chain, 31337 | 1337) || local(&conn.url) {
            // (a fork on this machine keeps the forked chain's id: its contracts don't exist on that chain)
            println!("not verifying: {} is a node on this machine", conn.name);
        } else {
            let ok = crate::verify::run(p, conn.chain, &mut recs);
            all.extend(recs);
            crate::verify::save(p, conn.chain, &all);
            for px in proxies.iter().filter(|px| all.contains_key(*px) || o.upgrade.as_ref().is_some_and(|u| u.proxy == **px)) {
                crate::verify::link_proxy(p, conn.chain, *px);
            }
            if !ok {
                println!("retry the failed ones with: plain verify --network {}", conn.name);
            }
        }
    }
    if !all_ok {
        die("not every transaction went through (see above)");
    }
}

/// Stops the deploy unless each contract a transaction creates directly is solc's output for its sources: every one
/// is compiled again from the standard JSON input explorers get and compared byte for byte, so a stale or tampered
/// cache, a rewrite or a compiler quirk can't put other bytecode on chain under this source.
fn check_bytecode(p: &Project, version: &str, arts: &[Artifact], libs: &HashMap<String, Address>, txs: &[BroadcastTx]) {
    use rayon::prelude::*;
    let codes: Vec<(&Artifact, Vec<u8>)> = arts.iter().filter(|a| !a.bytecode.is_empty() && !a.rewritten && a.shard == 0).filter_map(|a| Some((a, evm::link_code(&a.bytecode, &a.links, libs).ok()?))).collect();
    let mut want: BTreeMap<&str, Vec<&Artifact>> = BTreeMap::new();
    for (i, tx) in txs.iter().enumerate() {
        let init: &[u8] = match tx.to {
            None => &tx.data,
            Some(evm::CREATE2_DEPLOYER) if tx.data.len() >= 32 => &tx.data[32..],
            _ => continue,
        };
        match codes.iter().filter(|(_, c)| init.starts_with(c)).max_by_key(|(_, c)| c.len()) {
            Some((a, _)) => want.entry(a.file.as_str()).or_default().push(a),
            None => eprintln!("warning: transaction {} creates a contract from bytecode that isn't one of this project's; there's no source to check it against", i + 1),
        }
    }
    let bad: Vec<String> = want
        .par_iter()
        .flat_map(|(f, arts)| match crate::solc::canonical(p, version, f) {
            Err(e) => vec![format!("{f} (compiling it again failed: {e})")],
            Ok(c) => arts.iter().filter(|a| c.get(&a.name).is_none_or(|(b, _)| *b != a.bytecode)).map(|a| format!("{}:{}", a.file, a.name)).collect(),
        })
        .collect();
    if !bad.is_empty() {
        die(&format!("not sending: the bytecode for {} isn't what solc compiles from the sources; `plain build --force` rebuilds it", bad.join(", ")));
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
    let mut cheats = Cheats::new(arts.clone(), p.root.clone(), Arc::new(p.access.clone()));
    cheats.track_creates = true;
    cheats.presume_creates = true;
    let mut vm = evm::Vm::new(db, env, &mut cheats);
    let mut out = vec![];
    for (i, tx) in txs.iter().enumerate() {
        let n = vm.cheats().creates.len();
        let r = vm.call(tx.from, tx.to.map(TxKind::Call).unwrap_or(TxKind::Create), tx.data.clone(), tx.value, true);
        if !r.ok {
            die(&format!("transaction {} would revert on chain: {}", i + 1, why(&r.output)));
        }
        let creates = vm.cheats().creates[n..].to_vec();
        if tx.to == Some(evm::CREATE2_DEPLOYER) && creates.is_empty() {
            die(&format!("transaction {} goes through the CREATE2 deployer {}, which this chain doesn't have", i + 1, evm::CREATE2_DEPLOYER));
        }
        out.push(Sim { gas: r.gas, spent: r.spent, creates });
    }
    out
}

/// Max fee and priority fee per gas: EIP-1559 when the chain has a base fee, with room for it to double.
fn fees(conn: &Conn) -> (u128, u128) {
    match conn.base_fee {
        Some(base) => {
            let prio = conn.prio.unwrap_or_else(|| conn.gas_price.saturating_sub(base).max(1_000_000));
            (base * 2 + prio, prio)
        }
        None => (conn.gas_price + conn.gas_price / 10, 0),
    }
}

/// The most sending `txs` from `who` can cost, gas at 30% over the simulation as sent and fees at their cap.
fn cost(conn: &Conn, who: Address, txs: &[BroadcastTx], sims: &[Sim]) -> u128 {
    txs.iter().zip(sims).filter(|(t, _)| t.from == who).map(|(t, s)| gas_limit(conn, s) as u128 * fees(conn).0 + t.value.to::<u128>()).sum()
}

/// Signs every transaction up front, sends them back to back (nonces keep them in order), then waits for all
/// receipts at once. Returns each transaction's hash and whether it went through, None for one that didn't get on
/// chain; saves the addresses of what did.
fn send(p: &Project, conn: &Conn, txs: &[BroadcastTx], sims: &[Sim], keys: &HashMap<Address, B256>, names: &HashMap<Address, String>, at: u64) -> Vec<Option<(B256, bool)>> {
    let url = &conn.url;
    let mut senders: Vec<Address> = txs.iter().map(|t| t.from).collect();
    senders.sort();
    senders.dedup();
    let block = format!("0x{at:x}");
    let calls: Vec<(&str, Value)> = senders.iter().flat_map(|a| [("eth_getTransactionCount", json!([a, "pending"])), ("eth_getBalance", json!([a, "latest"])), ("eth_getTransactionCount", json!([a, block]))]).collect();
    let r = rpc::batch(url, &calls).unwrap_or_else(|e| die(&e));
    let mut nonces: HashMap<Address, u64> = senders.iter().enumerate().map(|(i, a)| (*a, hexnum(&r[3 * i], &format!("{a}'s pending nonce")) as u64)).collect();
    let balances: HashMap<Address, u128> = senders.iter().enumerate().map(|(i, a)| (*a, hexnum(&r[3 * i + 1], &format!("{a}'s balance")))).collect();
    // contracts land at addresses that follow from the sender's nonce, and later transactions call them there: if
    // the next nonce isn't the one the simulation ran with, they'd call where nothing is
    for (i, a) in senders.iter().enumerate() {
        let (next, simulated) = (nonces[a], hexnum(&r[3 * i + 2], &format!("{a}'s nonce at block {at}")) as u64);
        if next != simulated {
            die(&format!("{a}'s next nonce is {next}, but the simulation ran at nonce {simulated}: a transaction of its own is pending or just landed. Once it's in, run this again"));
        }
    }
    let (max_fee, prio) = fees(conn);
    let mut cost: HashMap<Address, u128> = HashMap::new();
    let mut signed = vec![];
    for (tx, s) in txs.iter().zip(sims) {
        let key = keys.get(&tx.from).unwrap_or_else(|| die(&format!("no private key for broadcaster {}", tx.from)));
        let nonce = nonces.get_mut(&tx.from).unwrap();
        let gas = gas_limit(conn, s);
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
            let sym = rpc::symbol(conn.chain);
            die(&format!("{a} has {} {sym} but these transactions can cost up to {} {sym} (at a {} gwei max fee); fund it first", eth(have), eth(*need), max_fee as f64 / 1e9));
        }
    }
    let t = Instant::now();
    let raw = |i: usize| ("eth_sendRawTransaction", json!([format!("0x{}", hex::encode(&signed[i].1))]));
    // the first on its own: a fee, balance or chain problem stops the deploy before anything else is in the pool.
    // The rest in one batch, in nonce order (a round trip each would cost seconds on a remote RPC)
    let mut results = vec![rpc::call(url, raw(0).0, raw(0).1)];
    if results[0].is_ok() && signed.len() > 1 {
        let calls: Vec<(&str, Value)> = (1..signed.len()).map(raw).collect();
        // a batch that failed as a whole may still have gone through: its transactions are looked for below
        results.extend(rpc::batch_each(url, &calls).unwrap_or_else(|e| vec![Err(format!("unknown: {e}")); calls.len()]));
    }
    // the transactions that are (or may be) in the pool, in order; the first rejected one ends it
    let mut sent = 0;
    for (i, r) in results.into_iter().enumerate() {
        let r = match r {
            // a node that ran the batch out of order: again, now that the nonces before it are in
            Err(e) if e.contains("nonce too high") || e.contains("gap") => rpc::call(url, raw(i).0, raw(i).1),
            r => r,
        };
        let known = |e: &str| ["already known", "known transaction", "already imported"].iter().any(|k| e.to_lowercase().contains(k));
        // "nonce too low": ours, mined while a rate-limited request was retried, if the chain has it
        let mined = |e: &str| e.contains("nonce too low") && rpc::call(url, "eth_getTransactionByHash", json!([signed[i].0])).is_ok_and(|v| !v.is_null());
        match r {
            Err(e) if !known(&e) && !e.starts_with("unknown: ") && !mined(&e) => {
                eprintln!("transaction {} rejected: {e}", i + 1);
                if i + 1 < signed.len() && i > 0 {
                    eprintln!("the {} after it went out in the same batch and wait behind its nonce", signed.len() - i - 1);
                }
                break;
            }
            _ => sent = i + 1,
        }
    }
    println!("sent {sent}/{} transactions to {} in {:.1?}; waiting for receipts", signed.len(), conn.name, t.elapsed());

    let mut receipts: Vec<Option<Value>> = vec![None; sent];
    let mut next = 0;
    let mut deployed: BTreeMap<String, String> = BTreeMap::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut last = Instant::now();
    // block number of the last receipt (or resend): a block passing without any of ours means the node dropped
    // them (anvil does when one doesn't fit in a block; public mempools evict): send what's pending again,
    // nodes answer "already known" otherwise
    let mut seen = 0;
    while next < sent {
        let pending: Vec<usize> = (0..sent).filter(|i| receipts[*i].is_none()).collect();
        // a sender's transactions are mined in nonce order: ask for each sender's first pending receipt, and for
        // the rest once one of those is in (public RPCs rate-limit per call, batched or not)
        let heads: Vec<usize> = pending.iter().copied().filter(|&i| !pending.iter().any(|&j| j < i && txs[j].from == txs[i].from)).collect();
        let (mut block, mut progress, mut polled) = (0, false, false);
        for ask in [heads, pending.clone()] {
            let ask: Vec<usize> = ask.into_iter().filter(|i| receipts[*i].is_none()).collect();
            if ask.is_empty() || (polled && !progress) {
                break;
            }
            let mut calls: Vec<(&str, Value)> = ask.iter().map(|i| ("eth_getTransactionReceipt", json!([signed[*i].0]))).collect();
            calls.push(("eth_blockNumber", json!([])));
            let Ok(mut rs) = rpc::batch_each(url, &calls) else { break };
            polled = true;
            // 0 (unknown) only means no resend this round
            block = rs.pop().and_then(|b| b.ok()).and_then(|b| rpc::quantity(&b).ok()).map_or(0, |b| b.saturating_to::<u128>());
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
            let gas = rpc::quantity(&receipt["gasUsed"]).map_or("?".into(), |g| g.to_string());
            let mut line = format!("{}. {} {} gas {gas}", i + 1, if ok { "ok" } else { "REVERTED" }, signed[i].0);
            let addr = match tx.to {
                None => receipt["contractAddress"].as_str().and_then(|a| a.parse::<Address>().ok()),
                Some(evm::CREATE2_DEPLOYER) => sims[i].creates.first().map(|c| c.0),
                _ => None,
            };
            if let Some(addr) = addr.filter(|_| ok) {
                if tx.created.is_some_and(|c| c != addr) {
                    eprintln!("warning: {addr} differs from simulated address {} (nonce changed?)", tx.created.unwrap());
                }
                let name = tx.created.and_then(|c| names.get(&c).cloned()).unwrap_or_else(|| format!("contract-{i}"));
                line += &format!(" -> {name} at {addr}");
                // a name made more than once (proxies, say) keeps the latest under the name, earlier ones as name#n
                if let Some(old) = deployed.remove(&name) {
                    deployed.insert(format!("{name}#{}", index[&name] + 1), old);
                }
                index.insert(name.clone(), i);
                deployed.insert(name, addr.to_checksum(None));
            }
            println!("{line}");
            next += 1;
        }
        if next < sent {
            if last.elapsed() > Duration::from_secs(300) {
                eprintln!("no receipt for {} after 5 minutes; it may still land (plain verify can verify it later)", signed[next].0);
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    if !deployed.is_empty() {
        let path = addresses_path(p, conn);
        let mut all = addresses(p, conn);
        all.extend(deployed);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap() + "\n").unwrap();
        println!("addresses saved to {}", path.strip_prefix(&p.root).unwrap_or(&path).display());
    }
    (0..signed.len()).map(|i| receipts.get(i).and_then(|r| r.as_ref()).map(|r| (signed[i].0, r["status"] == "0x1"))).collect()
}

/// The gas a transaction is sent with: 30% over what the simulation spent before refunds (refunds come back only
/// after execution), as forge does; within Osaka's per-transaction cap when that's enough.
fn gas_limit(conn: &Conn, s: &Sim) -> u64 {
    let need = s.spent.max(s.gas);
    let mut gas = need * 13 / 10;
    if need < 1 << 24 {
        gas = gas.min(1 << 24);
    }
    if conn.gas_limit > 0 {
        gas = gas.min(conn.gas_limit);
    }
    gas
}

fn eth(wei: u128) -> String {
    // L2 deploys cost fractions of a cent: more digits below 0.001
    let v = wei as f64 / 1e18;
    match v {
        0.0 => "0".into(),
        v if v < 0.001 => format!("{v:.9}"),
        v => format!("{v:.6}"),
    }
}
