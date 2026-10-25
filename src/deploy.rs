//! `plain deploy`: simulate (forked when a network is given), replay each transaction on its own for exact gas,
//! then sign everything, send it a window at a time (what uses a new contract only once that's in), and verify the
//! created contracts on explorers.
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
    /// forge's --slow: send one transaction at a time, each after the last one's receipt
    pub slow: bool,
    /// `plain script`: a simulation against a chain also writes forge's dry-run journal (see `journal`)
    pub journal: bool,
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
#[derive(serde::Serialize, serde::Deserialize)]
struct Sim {
    /// gas used, as the receipt will say
    gas: u64,
    /// gas spent before refunds: what the transaction has to be given
    spent: u64,
    /// contracts it creates (directly, through the CREATE2 deployer, or nested), with their init code
    creates: Vec<(Address, Bytes)>,
    /// accounts it called or ran the code of
    touched: std::collections::HashSet<Address>,
}

/// What a simulation of the deploy produced.
struct Planned {
    txs: Vec<BroadcastTx>,
    cheats: Cheats,
    libs: HashMap<String, Address>,
    /// proxies this deploys or upgrades, to point Etherscan at their implementation
    proxies: Vec<Address>,
}

/// A transaction signed and ready to go, with what recording what it makes takes.
#[derive(serde::Serialize, serde::Deserialize)]
struct Left {
    /// its number in the deploy, from 1
    n: usize,
    tx: BroadcastTx,
    sim: Sim,
    nonce: u64,
    hash: B256,
    raw: Bytes,
    /// what explorers need to verify the contracts it creates
    records: Vec<(Address, Record)>,
}

/// Signed transactions to send, kept in .plain/deploys/<chain id>-resume.json from before the first goes out until
/// all are in: what didn't get on chain is there for `plain deploy --resume`, and no new deploy to the chain starts
/// meanwhile (its nonces are this one's).
#[derive(serde::Serialize, serde::Deserialize)]
struct Resume {
    names: HashMap<Address, String>,
    /// proxies to point Etherscan at their implementation
    proxies: Vec<Address>,
    left: Vec<Left>,
    /// why what's left can't be sent any more (a transaction before it failed), when some of it was sent and may
    /// still land
    #[serde(default)]
    void: Option<String>,
}

fn resume_path(p: &Project, conn: &Conn) -> std::path::PathBuf {
    p.root.join(format!(".plain/deploys/{}-resume.json", conn.chain))
}

/// Sends what a deploy that stopped part way left, as it was signed then: nothing is simulated or signed again, so
/// what landed meanwhile or is still in the pool is recognized as such.
pub fn resume(p: &Project, conn: Conn, o: Opts) {
    let path = resume_path(p, &conn);
    let shown = path.strip_prefix(&p.root).unwrap_or(&path).display().to_string();
    let r: Resume = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| die(&format!("nothing to resume on {}: there's no {shown}", conn.name)));
    if let Some(why) = &r.void {
        die(&format!("{why}, and the rest of that deploy counted on it, so it can't be resumed. Once none of what was sent can still land (each is on chain or dropped), delete {shown} and deploy afresh"));
    }
    check_resume(&conn.url, &r.left).unwrap_or_else(|e| die(&e));
    for l in &r.left {
        println!("{}. {} (from {}, nonce {})", l.n, describe(&l.tx, &r.names), l.tx.from, l.nonce);
    }
    if o.confirm && !crate::ui::confirm(&format!("Send the {} transaction(s) left to {}?", r.left.len(), conn.name)) {
        println!("nothing sent");
        return;
    }
    broadcast(p, &conn, &o, r);
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
        arts.push(Artifact { name: "ProxyAdmin".into(), file: String::new(), abi, bytecode: String::new(), deployed: String::new(), links: Value::Null, deployed_links: Value::Null, immutables: Value::Null, shard: 0, tests: vec![], deploys: vec![], rewritten: false, profile: 0, methods: Value::Null });
    }
    decode::init(&arts);
    if o.broadcast && conn.is_none() {
        die("--broadcast needs --network <name|chain id|url>");
    }
    // an unfinished deploy's signed transactions may still land, at the nonces a new one would use
    if let Some(c) = conn.as_ref().filter(|c| o.broadcast && resume_path(p, c).exists()) {
        let path = resume_path(p, c);
        die(&format!("an earlier deploy to {} didn't finish ({}), and its signed transactions may still land at the nonces a new one would use. Finish it with plain deploy --resume --network {}; or, once none of them can land any more, delete that file and deploy afresh", c.name, path.strip_prefix(&p.root).unwrap_or(&path).display(), c.name));
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
    // forge script links a script's libraries at CREATE2 addresses when the chain has the CREATE2 deployer (a local
    // simulation always does)
    let create2 = is_script
        && fork.as_ref().is_none_or(|f| {
            let at = revm::DatabaseRef::basic_ref(&evm::Remote(Some(f.clone()), None), evm::CREATE2_DEPLOYER);
            at.unwrap_or_else(|e| die(&format!("couldn't read the CREATE2 deployer's code from the RPC ({e})"))).is_some_and(|i| i.code_hash != revm::primitives::KECCAK_EMPTY)
        });

    let simulate = |from: Address| -> Planned {
        let mut env = fresh_env.clone();
        let mut cheats = Cheats::new(arts.clone(), p.root.clone(), Arc::new(p.access.clone()));
        cheats.default_broadcaster = Some(from);
        if let Some(k) = key {
            cheats.keys.insert(key_addr(k), k);
        }
        let db = evm::base_db(evm::Remote(fork.clone(), None));
        cheats.track_creates = true;
        cheats.presume_creates = true;
        // libraries go on chain first (like forge script), except those the config says are on chain already
        let mut libs = p.libraries.clone();
        let mut txs: Vec<BroadcastTx> = vec![];
        let db = match create2 {
            true => link_create2(db, &mut env, &mut cheats, &arts, &art, &mut libs, from, p.create2_library_salt, &mut txs),
            false => crate::test::deploy_libs(db, &mut env, &mut cheats, &arts, &art, &mut libs, from, &mut Some(&mut txs)),
        };
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
    let run = |from: Address| loop {
        let plan = simulate(from);
        if !settle() {
            break plan;
        }
    };
    let mut plan = run(from);
    // forge script: a script that needs libraries runs again as the one account that creates contracts in it, when
    // that isn't the sender (a keyless rehearsal broadcasting as the owner), and that account deploys them
    if is_script && plan.libs.len() > p.libraries.len() && let Some(x) = new_sender(&plan.txs, from) {
        plan = run(x);
    }
    let Planned { txs, cheats, libs, proxies } = plan;
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
            if let Some(c) = c.as_ref().filter(|_| o.journal) {
                journal(p, c, &art, &txs, &sims, &cheats.names, &libs);
            }
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

    // a library the config says is on chain has to be there, or what links against it can't run
    let linked: std::collections::BTreeSet<Address> = planned.iter().flatten().flat_map(|(_, r)| r.libs.values().copied()).filter(|a| p.libraries.values().any(|l| l == a)).collect();
    let codes = rpc::batch(&conn.url, &linked.iter().map(|a| ("eth_getCode", json!([a, "latest"]))).collect::<Vec<_>>()).unwrap_or_else(|e| die(&e));
    if let Some((a, _)) = linked.iter().zip(&codes).find(|(_, c)| rpc::data(c).map_or(true, |c| c.is_empty())) {
        die(&format!("the library at {a} (libraries in the config) has no code on {}", conn.name));
    }
    let signed = sign(&conn, &txs, &sims, &keys, fork.as_ref().map_or(conn.block, |f| f.block()));
    let left = txs.into_iter().zip(sims).zip(signed).zip(planned).enumerate().map(|(i, (((tx, sim), (nonce, hash, raw)), records))| Left { n: i + 1, tx, sim, nonce, hash, raw, records }).collect();
    broadcast(p, &conn, &o, Resume { names: cheats.names.clone(), proxies, left, void: None });
    if o.proxy {
        println!("{} is upgradeable: plain upgrade {} --network {}", art.name, art.name, conn.name);
    }
}

/// forge script's `maybe_new_sender`: the one account other than `sender` whose transactions create contracts, if
/// there's exactly one (a create from `sender` after it means there are two).
fn new_sender(txs: &[BroadcastTx], sender: Address) -> Option<Address> {
    let mut new = None;
    for t in txs.iter().filter(|t| t.to.is_none()) {
        match new {
            Some(n) if t.from != n => return None,
            None if t.from != sender => new = Some(t.from),
            _ => {}
        }
    }
    new
}

/// forge script's linking on a chain with the CREATE2 deployer: each library the script needs (and the libraries
/// those need) at its CREATE2 address with `salt`, placed in forge's order (artifact ids sorted, then repeatedly the
/// first one whose own libraries are placed, taken out with swap_remove); only those the chain doesn't have yet are
/// deployed, by `from` through the deployer.
#[allow(clippy::too_many_arguments)]
fn link_create2(mut db: evm::Db, env: &mut Env, cheats: &mut Cheats, arts: &[Artifact], art: &Artifact, libs: &mut HashMap<String, Address>, from: Address, salt: B256, txs: &mut Vec<BroadcastTx>) -> evm::Db {
    let lib = |file: &str, name: &str| arts.iter().find(|a| a.file == file && a.name == name && a.profile == 0).unwrap_or_else(|| die(&format!("library {file}:{name} has no bytecode (is it outside src/test/script?)")));
    let mut needed = crate::test::needed_libs(arts, art);
    let mut todo: Vec<(String, String)> = needed.iter().cloned().collect();
    while let Some((f, n)) = todo.pop() {
        for (file, names) in lib(&f, &n).links.as_object().into_iter().flatten() {
            for name in names.as_object().into_iter().flatten().map(|(name, _)| name) {
                if needed.insert((file.clone(), name.clone())) {
                    todo.push((file.clone(), name.clone()));
                }
            }
        }
    }
    let base = |f: &str| f.rsplit('/').next().unwrap_or(f).to_string();
    let mut left: Vec<&Artifact> = needed.iter().filter(|(f, n)| !libs.contains_key(&format!("{f}:{n}"))).map(|(f, n)| lib(f, n)).collect();
    left.sort_by_key(|a| (base(&a.file), format!("{}.json", a.name)));
    while !left.is_empty() {
        let i = left.iter().position(|a| evm::link_code(&a.bytecode, &a.links, libs).is_ok()).unwrap_or_else(|| die("the libraries link against each other in a cycle"));
        let a = left.swap_remove(i);
        let code = evm::link_code(&a.bytecode, &a.links, libs).unwrap();
        let at = evm::CREATE2_DEPLOYER.create2_from_code(salt, &code);
        libs.insert(format!("{}:{}", a.file, a.name), at);
        cheats.names.insert(at, a.name.clone());
        let there = revm::Database::basic(&mut db, at).unwrap_or_else(|e| die(&format!("couldn't read {at} from the RPC ({e})")));
        if there.is_some_and(|i| i.code_hash != revm::primitives::KECCAK_EMPTY) {
            continue;
        }
        let data: Bytes = [salt.as_slice(), &code].concat().into();
        let r;
        (db, r) = evm::transact(db, env, cheats, from, TxKind::Call(evm::CREATE2_DEPLOYER), data.clone(), U256::ZERO, true);
        if !r.ok {
            die(&format!("deploying library {}:{} through the CREATE2 deployer failed: {}", a.file, a.name, why(&r.output)));
        }
        txs.push(BroadcastTx { from, to: Some(evm::CREATE2_DEPLOYER), value: U256::ZERO, data, created: Some(at) });
    }
    db
}

/// forge's dry-run journal of a simulation, broadcast/<script file>/<chain id>/dry-run/run-latest.json (and
/// run-<ms>.json), for tools that read forge's plans: every transaction as it would be sent, at the nonces its sender
/// has at the block the simulation ran on. `returns` and `arguments` stay empty.
fn journal(p: &Project, conn: &Conn, script: &Artifact, txs: &[BroadcastTx], sims: &[Sim], names: &HashMap<Address, String>, libs: &HashMap<String, Address>) {
    let mut senders: Vec<Address> = txs.iter().map(|t| t.from).collect();
    senders.sort();
    senders.dedup();
    let block = format!("{:#x}", conn.block);
    let r = rpc::batch(&conn.url, &senders.iter().map(|a| ("eth_getTransactionCount", json!([a, block]))).collect::<Vec<_>>()).unwrap_or_else(|e| die(&e));
    let mut nonces: HashMap<Address, u64> = senders.iter().zip(&r).map(|(a, n)| (*a, hexnum(n, &format!("{a}'s nonce at block {}", conn.block)) as u64)).collect();
    let addr = |a: &Address| format!("{a:#x}");
    let transactions: Vec<Value> = txs
        .iter()
        .zip(sims)
        .map(|(tx, s)| {
            let nonce = nonces.get_mut(&tx.from).unwrap();
            let (kind, at) = match tx.to {
                None => ("CREATE", tx.created),
                Some(evm::CREATE2_DEPLOYER) => ("CREATE2", tx.created),
                Some(to) => ("CALL", Some(to)),
            };
            let function = tx.to.filter(|_| kind == "CALL" && tx.data.len() >= 4).and_then(|_| decode::tables()?.functions.get(&<[u8; 4]>::try_from(&tx.data[..4]).ok()?)).map(|f| f.signature());
            let nested: Vec<Value> = s.creates.iter().filter(|(a, _)| Some(*a) != tx.created).map(|(a, init)| json!({"contractName": names.get(a), "address": addr(a), "initCode": format!("0x{}", hex::encode(init))})).collect();
            let t = json!({
                "hash": null,
                "transactionType": kind,
                "contractName": at.and_then(|a| names.get(&a)),
                "contractAddress": at.as_ref().map(addr),
                "function": function,
                "arguments": null,
                "transaction": {"from": addr(&tx.from), "to": tx.to.as_ref().map(addr), "gas": format!("{:#x}", gas_limit(conn, s)), "value": format!("{:#x}", tx.value), "input": format!("0x{}", hex::encode(&tx.data)), "nonce": format!("{:#x}", *nonce), "chainId": format!("{:#x}", conn.chain)},
                "additionalContracts": nested,
                "isFixedGasLimit": false,
            });
            *nonce += 1;
            t
        })
        .collect();
    let mut libraries: Vec<String> = libs.iter().map(|(id, a)| format!("{id}:{a}")).collect();
    libraries.sort();
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let j = json!({"transactions": transactions, "receipts": [], "libraries": libraries, "pending": [], "returns": {}, "timestamp": ms, "chain": conn.chain, "commit": null});
    let dir = p.root.join("broadcast").join(script.file.rsplit('/').next().unwrap_or(&script.file)).join(conn.chain.to_string()).join("dry-run");
    let text = serde_json::to_string_pretty(&j).unwrap();
    for name in [format!("run-{ms}.json"), "run-latest.json".into()] {
        std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(name), &text)).unwrap_or_else(|e| die(&format!("{}: {e}", dir.display())));
    }
    let latest = dir.join("run-latest.json");
    println!("plan written to {}", latest.strip_prefix(&p.root).unwrap_or(&latest).display());
}

/// Sends signed transactions (see send), saves where what they made landed and what explorers need to verify it,
/// keeps what didn't get on chain for --resume, and verifies. Dies unless everything went through.
fn broadcast(p: &Project, conn: &Conn, o: &Opts, r: Resume) {
    // forge's --slow: one at a time; a node on this machine takes everything at once
    let window = if o.slow { 1 } else if local(&conn.url) || matches!(conn.chain, 31337 | 1337) { usize::MAX } else { 8 };
    // until it's all in, what's signed is kept for --resume (even if plain is stopped): in .plain, which `plain init`
    // keeps out of git
    let path = resume_path(p, conn);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&r).unwrap()).unwrap();
    let (landed, deployed, sent) = send(conn, &r.left, &r.names, window);
    if !deployed.is_empty() {
        let path = addresses_path(p, conn);
        let mut all = addresses(p, conn);
        all.extend(deployed);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap() + "\n").unwrap();
        println!("addresses saved to {}", path.strip_prefix(&p.root).unwrap_or(&path).display());
    }
    // records for what made it on chain, whatever happened to the rest
    let mut recs = BTreeMap::new();
    for (l, ok) in r.left.iter().zip(&landed) {
        if *ok == Some(true) {
            for (addr, rec) in &l.records {
                recs.insert(*addr, Record { tx: Some(l.hash), ..rec.clone() });
            }
        }
    }
    let mut all = crate::verify::load(p, conn.chain);
    all.extend(recs.clone());
    crate::verify::save(p, conn.chain, &all);
    // a proxy this deploy makes is pointed at its implementation once it's on chain; one it upgrades, always
    let link: Vec<Address> = r.proxies.iter().copied().filter(|px| all.contains_key(px) || !r.left.iter().any(|l| l.sim.creates.iter().any(|c| c.0 == *px))).collect();
    // what didn't get on chain stays for --resume; after a failed transaction, which the rest counts on, it can't
    // be resumed, but stays (blocking new deploys) if some of it was sent and may still land
    let failed = r.left.iter().zip(&landed).find(|(_, l)| **l == Some(false)).map(|(l, _)| l.n);
    let in_flight = landed[..sent].contains(&None);
    let rest: Vec<Left> = r.left.into_iter().zip(&landed).filter(|(_, l)| l.is_none()).map(|(l, _)| l).collect();
    let resumable = !rest.is_empty() && failed.is_none();
    if resumable || (failed.is_some() && in_flight) {
        let void = failed.map(|n| format!("transaction {n} of the deploy to {} didn't go through", conn.name));
        std::fs::write(&path, serde_json::to_vec(&Resume { left: rest, void, ..r }).unwrap()).unwrap();
    } else {
        std::fs::remove_file(&path).ok();
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
            for px in link {
                crate::verify::link_proxy(p, conn.chain, px);
            }
            if !ok {
                println!("retry the failed ones with: plain verify --network {}", conn.name);
            }
        }
    }
    if resumable {
        die(&format!("not everything got on chain; to send the rest as signed: plain deploy --resume --network {}", conn.name));
    }
    if let Some(n) = failed {
        let wait = if in_flight { format!("; some of what was sent after it may still land, so a new deploy to {} waits until {} is deleted", conn.name, path.strip_prefix(&p.root).unwrap_or(&path).display()) } else { String::new() };
        die(&format!("transaction {n} didn't go through (see above){wait}"));
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
        vm.cheats().touched = Some(Default::default());
        let r = vm.call(tx.from, tx.to.map(TxKind::Call).unwrap_or(TxKind::Create), tx.data.clone(), tx.value, true);
        let touched = vm.cheats().touched.take().unwrap_or_default();
        if !r.ok {
            die(&format!("transaction {} would revert on chain: {}", i + 1, why(&r.output)));
        }
        let creates = vm.cheats().creates[n..].to_vec();
        if tx.to == Some(evm::CREATE2_DEPLOYER) && creates.is_empty() {
            die(&format!("transaction {} goes through the CREATE2 deployer {}, which this chain doesn't have", i + 1, evm::CREATE2_DEPLOYER));
        }
        out.push(Sim { gas: r.gas, spent: r.spent, creates, touched });
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

/// Signs every transaction, each sender's from its next nonce, which has to be the one the simulation ran with at
/// block `at`. Returns each one's nonce, hash and encoding.
fn sign(conn: &Conn, txs: &[BroadcastTx], sims: &[Sim], keys: &HashMap<Address, B256>, at: u64) -> Vec<(u64, B256, Bytes)> {
    let url = &conn.url;
    let mut senders: Vec<Address> = txs.iter().map(|t| t.from).collect();
    senders.sort();
    senders.dedup();
    let mut nonces = next_nonces(url, &senders).unwrap_or_else(|e| die(&e));
    let block = format!("0x{at:x}");
    let calls: Vec<(&str, Value)> = senders.iter().flat_map(|a| [("eth_getBalance", json!([a, "latest"])), ("eth_getTransactionCount", json!([a, block]))]).collect();
    let r = rpc::batch(url, &calls).unwrap_or_else(|e| die(&e));
    let balances: HashMap<Address, u128> = senders.iter().enumerate().map(|(i, a)| (*a, hexnum(&r[2 * i], &format!("{a}'s balance")))).collect();
    // contracts land at addresses that follow from the sender's nonce, and later transactions call them there: if
    // the next nonce isn't the one the simulation ran with, they'd call where nothing is
    for (i, a) in senders.iter().enumerate() {
        let (next, simulated) = (nonces[a], hexnum(&r[2 * i + 1], &format!("{a}'s nonce at block {at}")) as u64);
        if next != simulated {
            die(&format!("{a}'s next nonce is {next}, but the simulation ran at nonce {simulated}: a transaction of its own just landed. Run this again"));
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
        signed.push((*nonce, *env.tx_hash(), env.encoded_2718().into()));
        *nonce += 1;
    }
    for (a, need) in &cost {
        let have = balances[a];
        if have < *need {
            let sym = rpc::symbol(conn.chain);
            die(&format!("{a} has {} {sym} but these transactions can cost up to {} {sym} (at a {} gwei max fee); fund it first", eth(have), eth(*need), max_fee as f64 / 1e9));
        }
    }
    signed
}

/// Each sender's next nonce, refusing while one has transactions waiting in a node's pool: those (an aborted run's,
/// say) can land later at nonces this run uses, and put contracts that aren't the plan's where it puts its own.
fn next_nonces(url: &str, senders: &[Address]) -> Result<HashMap<Address, u64>, String> {
    let calls: Vec<(&str, Value)> = senders.iter().flat_map(|a| [("eth_getTransactionCount", json!([a, "latest"])), ("eth_getTransactionCount", json!([a, "pending"]))]).collect();
    let mut tries = 0;
    loop {
        let r = rpc::batch(url, &calls)?;
        let n = |v: &Value| rpc::quantity(v).map(|n| n.saturating_to::<u64>());
        let mut next = HashMap::new();
        let mut waiting = None;
        for (i, a) in senders.iter().enumerate() {
            let (latest, pending) = (n(&r[2 * i]).map_err(|e| format!("{a}'s nonce: {e}"))?, n(&r[2 * i + 1]).map_err(|e| format!("{a}'s pending nonce: {e}"))?);
            if pending != latest {
                waiting = Some(format!("{a} has transactions waiting in a node's pool (nonce {latest} on chain, {pending} counting the pool): wait until they land or drop, then run this again"));
            }
            next.insert(*a, latest);
        }
        // (a load-balanced RPC's nodes can be a block apart for a moment)
        match waiting {
            Some(e) if tries == 2 => return Err(e),
            Some(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(if cfg!(test) { 1 } else { 2000 }));
            }
            None => return Ok(next),
        }
    }
}

/// Whether the chain's copy of a transaction (eth_getTransactionByHash) is exactly the planned one: sender, nonce,
/// target, input and value.
fn exact(l: &Left, got: &Value) -> bool {
    let addr = |v: &Value| v.as_str().and_then(|s| s.parse::<Address>().ok());
    let input = got["input"].as_str().and_then(|s| hex::decode(s.trim_start_matches("0x")).ok());
    addr(&got["from"]) == Some(l.tx.from) && rpc::quantity(&got["nonce"]).ok() == Some(U256::from(l.nonce)) && addr(&got["to"]) == l.tx.to && input.as_deref() == Some(&l.tx.data[..]) && rpc::quantity(&got["value"]).ok() == Some(l.tx.value)
}

/// What --resume checks first: nothing waiting in a pool, and every nonce of what's left that the chain has used
/// taken by exactly that transaction of the plan.
fn check_resume(url: &str, left: &[Left]) -> Result<(), String> {
    let mut senders: Vec<Address> = left.iter().map(|l| l.tx.from).collect();
    senders.sort();
    senders.dedup();
    let next = next_nonces(url, &senders)?;
    let used: Vec<&Left> = left.iter().filter(|l| l.nonce < next[&l.tx.from]).collect();
    let got = rpc::batch(url, &used.iter().map(|l| ("eth_getTransactionByHash", json!([l.hash]))).collect::<Vec<_>>())?;
    if let Some((l, _)) = used.iter().zip(&got).find(|(l, g)| !exact(l, g)) {
        return Err(format!("nonce {} of {} is taken, and not by transaction {} of this deploy ({}): not resuming", l.nonce, l.tx.from, l.n, l.hash));
    }
    for a in &senders {
        if let Some(l) = left.iter().find(|l| l.tx.from == *a && l.nonce >= next[a]).filter(|l| l.nonce != next[a]) {
            return Err(format!("{a}'s next nonce is {}, but what's left of the deploy goes on at {}: not resuming", next[a], l.nonce));
        }
    }
    Ok(())
}

/// Sends signed transactions in nonce order, at most `window` of a sender in the pool at once, waiting for receipts
/// in between (public RPCs rate-limit, and op-geth keeps about 16 per sender, dropping the rest). A transaction's
/// hash is the truth: a node can answer a send with an error and still take it, so an error is looked up by hash
/// first, and what's sent again is only ever what was signed. One counts as in once the chain has exactly it with
/// success. Sending stops at the first rejection, revert or mismatch; what uses a contract an earlier transaction
/// creates goes out only once that one is in, and the last transaction (a script's final upgradeAndCall, say) only
/// once everything before it is. Returns whether each went through (None: not on chain), the addresses made, and
/// how many were sent.
fn send(conn: &Conn, left: &[Left], names: &HashMap<Address, String>, window: usize) -> (Vec<Option<bool>>, BTreeMap<String, String>, usize) {
    let url = &conn.url;
    let t = Instant::now();
    let n = left.len();
    let raw = |i: usize| ("eth_sendRawTransaction", json!([format!("0x{}", hex::encode(&left[i].raw))]));
    // a transaction that calls or passes on a contract an earlier one creates goes out only once that one is in
    // with success: if the create fails or is dropped, nothing that uses it gets mined
    let deps: Vec<Vec<usize>> = (0..n).map(|j| (0..j).filter(|&i| left[i].sim.creates.iter().any(|(a, _)| left[j].tx.to == Some(*a) || left[j].sim.touched.contains(a) || left[j].tx.data.windows(20).any(|w| w == a.as_slice()))).collect()).collect();
    let known = |e: &str| ["already known", "known transaction", "already imported"].iter().any(|k| e.to_lowercase().contains(k));
    // rate limits, timeouts, and a node that lost an earlier nonce: sent again on a later round
    let later = |e: &str| rpc::transient(e) || e.contains("nonce too high") || e.contains("gap");
    let pause = |ms: u64| std::thread::sleep(Duration::from_millis(if cfg!(test) { 1 } else { ms }));
    // whether the chain or a pool has it, asked `k` times 2 s apart: a node may answer a send with an error and
    // still have it (one forwarding to a sequencer, say), and a load-balanced RPC's nodes lag one another
    let has = |j: usize, k: u32| {
        (0..k).any(|t| {
            if t > 0 {
                pause(2000);
            }
            rpc::call(url, "eth_getTransactionByHash", json!([left[j].hash])).is_ok_and(|v| !v.is_null())
        })
    };
    // "nonce too low" for a transaction the chain doesn't have: its nonce went to another one, so it never lands
    let taken = |j: usize| json!({"status": "taken", "why": format!("nonce {} of {} is taken by a transaction that isn't this deploy's", left[j].nonce, left[j].tx.from)});
    let ok = |r: &Option<Value>| r.as_ref().is_some_and(|r| r["status"] == "0x1");
    let mut receipts: Vec<Option<Value>> = vec![None; n];
    // [0, sent) are in a pool or on chain; `stop` ends sending, though what's out is still waited for
    let (mut sent, mut stop, mut busy) = (0, None::<String>, 0);
    let mut next = 0;
    let mut deployed: BTreeMap<String, String> = BTreeMap::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut last = Instant::now();
    // block number of the last receipt (or resend): a block passing without any of ours means the node dropped
    // them (anvil does when one doesn't fit in a block; public mempools evict): send what's pending again,
    // nodes answer "already known" otherwise
    let mut seen = 0;
    while next < sent || (stop.is_none() && sent < n) {
        // what may go out now: up to `window` of one sender in the pool (the first transaction alone until it's in, so
        // a fee, balance or chain problem stops the deploy before anything else is sent), in the script's order
        // across senders, nothing waiting on an unconfirmed create, and the last transaction only once everything
        // before it is in. A node on this machine (no window) takes the rest at once: nothing real is at stake there,
        // and its blocks are what a deploy waits on
        let mut batch = vec![];
        let cap = if receipts[0].is_none() { 1 } else { window };
        let gated = window != usize::MAX;
        while stop.is_none() && sent + batch.len() < n {
            let j = sent + batch.len();
            let waiting: Vec<usize> = (0..j).filter(|&i| receipts[i].is_none()).collect();
            let blocked = (gated && (deps[j].iter().any(|&i| !ok(&receipts[i])) || (j + 1 == n && !(0..j).all(|i| ok(&receipts[i]))))) || waiting.iter().any(|&i| left[i].tx.from != left[j].tx.from) || waiting.len() >= cap;
            if blocked {
                break;
            }
            batch.push(j);
        }
        if !batch.is_empty() {
            let calls: Vec<(&str, Value)> = batch.iter().map(|&j| raw(j)).collect();
            // a batch that failed as a whole may still have gone through: each is looked up below
            let results = rpc::batch_each(url, &calls).unwrap_or_else(|e| vec![Err(e); calls.len()]);
            let before = sent;
            for (&j, r) in batch.iter().zip(results) {
                let Err(e) = r else {
                    sent = j + 1;
                    continue;
                };
                // its hash decides, given a few seconds to show up unless the node was only busy
                if known(&e) || has(j, if later(&e) { 1 } else { 6 }) {
                    if !known(&e) {
                        eprintln!("{}. the RPC answered {e:?}, but has the transaction", left[j].n);
                    }
                    sent = j + 1;
                    continue;
                }
                if e.contains("nonce too low") {
                    receipts[j] = Some(taken(j));
                    sent = j + 1;
                    stop = Some(taken(j)["why"].as_str().unwrap().into());
                } else if !later(&e) {
                    stop = Some(format!("transaction {} rejected: {e}", left[j].n));
                } else if busy == 10 {
                    stop = Some(format!("transaction {} couldn't be sent: {e}", left[j].n));
                }
                busy += 1;
                break;
            }
            if sent > before {
                busy = 0;
                last = Instant::now();
            } else if stop.is_none() {
                rpc::backoff(busy.min(4) as u32);
            }
        }
        let pending: Vec<usize> = (next..sent).filter(|i| receipts[*i].is_none()).collect();
        if pending.is_empty() {
            continue;
        }
        // a sender's transactions are mined in nonce order: ask for each sender's first pending receipt, and for
        // the rest once one of those is in (public RPCs rate-limit per call, batched or not)
        let heads: Vec<usize> = pending.iter().copied().filter(|&i| !pending.iter().any(|&j| j < i && left[j].tx.from == left[i].tx.from)).collect();
        let (mut block, mut polled) = (0, false);
        let mut found: Vec<(usize, Value)> = vec![];
        for ask in [heads, pending.clone()] {
            let ask: Vec<usize> = ask.into_iter().filter(|i| !found.iter().any(|(j, _)| j == i)).collect();
            if ask.is_empty() || (polled && found.is_empty()) {
                break;
            }
            let mut calls: Vec<(&str, Value)> = ask.iter().map(|i| ("eth_getTransactionReceipt", json!([left[*i].hash]))).collect();
            calls.push(("eth_blockNumber", json!([])));
            let Ok(mut rs) = rpc::batch_each(url, &calls) else { break };
            polled = true;
            // 0 (unknown) only means no resend this round
            block = rs.pop().and_then(|b| b.ok()).and_then(|b| rpc::quantity(&b).ok()).map_or(0, |b| b.saturating_to::<u128>());
            found.extend(ask.iter().zip(rs).filter_map(|(i, r)| Some((*i, r.ok().filter(|r| !r.is_null())?))));
        }
        // a receipt counts once the chain's copy of the transaction is exactly the planned one (else it's asked again)
        let mut progress = false;
        let calls: Vec<(&str, Value)> = found.iter().map(|(i, _)| ("eth_getTransactionByHash", json!([left[*i].hash]))).collect();
        if let Ok(got) = rpc::batch_each(url, &calls) {
            for ((i, mut r), got) in found.into_iter().zip(got) {
                let Some(got) = got.ok().filter(|g| !g.is_null()) else { continue };
                let tx = &left[i].tx;
                let made = r["contractAddress"].as_str().and_then(|a| a.parse::<Address>().ok());
                if !exact(&left[i], &got) || (tx.to.is_none() && tx.created.is_some() && made != tx.created) {
                    r["status"] = json!("not as planned");
                }
                if r["status"] != "0x1" && stop.is_none() {
                    stop = Some(format!("transaction {} {}", left[i].n, if r["status"] == "0x0" { "reverted on chain" } else { "is on chain, but isn't the planned transaction" }));
                }
                receipts[i] = Some(r);
                last = Instant::now();
                progress = true;
            }
        }
        if polled {
            if progress || seen == 0 {
                seen = block;
            } else if block > seen {
                let again: Vec<usize> = pending.iter().copied().take(16).collect();
                for (i, r) in again.iter().zip(rpc::batch_each(url, &again.iter().map(|&i| raw(i)).collect::<Vec<_>>()).unwrap_or_default()) {
                    if r.is_err_and(|e| e.contains("nonce too low")) && !has(*i, 6) {
                        receipts[*i] = Some(taken(*i));
                        stop.get_or_insert_with(|| taken(*i)["why"].as_str().unwrap().into());
                    }
                }
                seen = block;
            }
        }
        while let Some(Some(receipt)) = receipts.get(next) {
            let (l, tx) = (&left[next], &left[next].tx);
            let good = receipt["status"] == "0x1";
            let gas = rpc::quantity(&receipt["gasUsed"]).map_or(String::new(), |g| format!(" gas {g}"));
            let what = match receipt["status"].as_str() {
                Some("0x1") => "ok",
                Some("0x0") => "REVERTED",
                Some("taken") => "NONCE TAKEN",
                _ => "NOT AS PLANNED",
            };
            let mut line = format!("{}. {what} {}{gas}", l.n, l.hash);
            let addr = match tx.to {
                None => receipt["contractAddress"].as_str().and_then(|a| a.parse::<Address>().ok()),
                Some(evm::CREATE2_DEPLOYER) => l.sim.creates.first().map(|c| c.0),
                _ => None,
            };
            if let Some(addr) = addr.filter(|_| good) {
                let name = tx.created.and_then(|c| names.get(&c).cloned()).unwrap_or_else(|| format!("contract-{}", l.n - 1));
                line += &format!(" -> {name} at {addr}");
                // a name made more than once (proxies, say) keeps the latest under the name, earlier ones as name#n
                if let Some(old) = deployed.remove(&name) {
                    deployed.insert(format!("{name}#{}", index[&name]), old);
                }
                index.insert(name.clone(), l.n);
                deployed.insert(name, addr.to_checksum(None));
            }
            println!("{line}");
            next += 1;
        }
        if next < sent {
            if last.elapsed() > Duration::from_secs(300) {
                eprintln!("no receipt for {} after 5 minutes; it may still land", left[next].hash);
                stop.get_or_insert_with(|| format!("transaction {} wasn't mined", left[next].n));
                break;
            }
            pause(250);
        }
    }
    println!("sent {sent}/{n} transactions to {} in {:.1?}", conn.name, t.elapsed());
    if let Some(why) = &stop {
        // what's where, by nonce
        eprintln!("stopped: {why}");
        let state = |i: usize| match &receipts[i] {
            Some(r) if r["status"] == "0x1" => "on chain",
            Some(r) if r["status"] == "0x0" => "on chain, reverted",
            Some(r) if r["status"] == "taken" => "nonce taken by another transaction",
            Some(_) => "on chain, not as planned",
            None if i < sent => "sent, no receipt yet",
            None => "not sent",
        };
        let mut i = 0;
        while i < n {
            let (a, mut b) = (&left[i], i);
            while b + 1 < n && state(b + 1) == state(i) && left[b + 1].tx.from == a.tx.from {
                b += 1;
            }
            let z = &left[b];
            let which = if b == i { format!("{}. nonce {}", a.n, a.nonce) } else { format!("{}-{}. nonces {}-{}", a.n, z.n, a.nonce, z.nonce) };
            eprintln!("  {which} from {}: {}", a.tx.from, state(i));
            i = b + 1;
        }
    }
    (receipts.iter().map(|r| r.as_ref().map(|r| r["status"] == "0x1")).collect(), deployed, sent)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A chain behind a public RPC, the way Base's looked on a 34-transaction upgrade: every third request
    /// rate-limited, two transactions mined per block (a block per eth_blockNumber), and a sender's transactions past
    /// 16 in the pool accepted but dropped, as op-geth does. Transaction ids are the nonces (one sender).
    #[derive(Default)]
    struct Chain {
        /// mined, in nonce order: Some(id) ours, None someone else's
        mined: Vec<Option<u8>>,
        pool: std::collections::BTreeSet<u8>,
        /// each send: the id, and how many nonces were used by then
        sends: Vec<(u8, usize)>,
        most_in_pool: usize,
        blocks: u64,
        /// a create that fails on chain
        fails: Option<u8>,
        /// a nonce someone else's transaction takes when its turn comes
        foreign: Option<u8>,
        /// sends answered with an error although the transaction is taken
        errs: Vec<u8>,
        /// what eth_getTransactionByHash answers for ours: the planned fields
        plan: Vec<Value>,
    }

    fn chain(c: Chain) -> (String, Arc<Mutex<Chain>>) {
        let c = Arc::new(Mutex::new(Chain { plan: txs().iter().map(|l| json!({"from": l.tx.from, "nonce": format!("0x{:x}", l.nonce), "to": l.tx.to, "input": format!("0x{}", hex::encode(&l.tx.data)), "value": "0x0"})).collect(), ..c }));
        let state = c.clone();
        let from = Address::repeat_byte(1);
        let url = rpc::mock::serve(move |req, n| {
            if n % 3 == 2 {
                return (429, r#"{"code":-32007,"message":"25/second request limit reached"}"#.into());
            }
            let mut c = state.lock().unwrap();
            let mut one = |call: &Value| {
                let id = |v: &Value| (0..=255u8).find(|i| json!(alloy_primitives::keccak256([*i])) == *v);
                let r: Result<Value, &str> = match call["method"].as_str().unwrap() {
                    "eth_sendRawTransaction" => {
                        let i = hex::decode(call["params"][0].as_str().unwrap().trim_start_matches("0x")).unwrap()[0];
                        if (i as usize) < c.mined.len() {
                            Err("nonce too low")
                        } else if c.pool.contains(&i) {
                            Err("already known")
                        } else {
                            let at = c.mined.len();
                            c.sends.push((i, at));
                            if c.pool.len() < 16 {
                                c.pool.insert(i);
                                c.most_in_pool = c.most_in_pool.max(c.pool.len());
                            }
                            if c.errs.contains(&i) { Err("failed to forward the transaction") } else { Ok(json!(alloy_primitives::keccak256([i]))) }
                        }
                    }
                    "eth_blockNumber" => {
                        for _ in 0..2 {
                            let next = c.mined.len() as u8;
                            if c.foreign == Some(next) {
                                c.pool.remove(&next);
                                c.mined.push(None);
                            } else if c.pool.remove(&next) {
                                c.mined.push(Some(next));
                            }
                        }
                        c.blocks += 1;
                        Ok(json!(format!("0x{:x}", c.blocks)))
                    }
                    "eth_getTransactionCount" => {
                        let latest = c.mined.len();
                        let pending = (latest..).take_while(|i| c.pool.contains(&(*i as u8))).count() + latest;
                        Ok(json!(format!("0x{:x}", if call["params"][1] == "pending" { pending } else { latest })))
                    }
                    "eth_getTransactionReceipt" => Ok(match id(&call["params"][0]) {
                        Some(i) if c.mined.contains(&Some(i)) => json!({"status": if c.fails == Some(i) { "0x0" } else { "0x1" }, "gasUsed": "0x5208", "contractAddress": from.create(i as u64)}),
                        _ => Value::Null,
                    }),
                    "eth_getTransactionByHash" => Ok(match id(&call["params"][0]) {
                        Some(i) if c.mined.contains(&Some(i)) || c.pool.contains(&i) => c.plan[i as usize].clone(),
                        _ => Value::Null,
                    }),
                    m => panic!("{m}"),
                };
                match r {
                    Ok(r) => json!({"jsonrpc": "2.0", "id": call["id"], "result": r}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": call["id"], "error": {"code": -32000, "message": e}}),
                }
            };
            (200, match req.as_array() {
                Some(calls) => Value::Array(calls.iter().map(&mut one).collect()),
                None => one(req),
            }.to_string())
        });
        (url, c)
    }

    /// 20 creates from one sender, except the 4th, which calls the contract the 3rd creates, and the last, a call
    fn txs() -> Vec<Left> {
        let from = Address::repeat_byte(1);
        (0..20u8)
            .map(|i| {
                let to = match i {
                    3 => Some(from.create(2)),
                    19 => Some(Address::repeat_byte(9)),
                    _ => None,
                };
                let created = from.create(i as u64);
                let creates = if to.is_none() { vec![(created, Bytes::new())] } else { vec![] };
                let tx = BroadcastTx { from, to, value: U256::ZERO, data: Bytes::new(), created: to.is_none().then_some(created) };
                Left { n: i as usize + 1, tx, sim: Sim { gas: 21000, spent: 21000, creates, touched: Default::default() }, nonce: i as u64, hash: alloy_primitives::keccak256([i]), raw: Bytes::from(vec![i]), records: vec![] }
            })
            .collect()
    }

    fn conn(url: String) -> Conn {
        Conn { name: "base".into(), url, chain: 8453, block: 1, timestamp: 0, gas_limit: 0, base_fee: Some(1), prio: Some(1), gas_price: 2 }
    }

    #[test]
    fn sends_a_window_at_a_time_through_rate_limits() {
        for window in [8, 1] {
            let (url, c) = chain(Chain::default());
            let (landed, deployed, _) = send(&conn(url), &txs(), &HashMap::new(), window);
            let c = c.lock().unwrap();
            assert_eq!(landed, vec![Some(true); 20]);
            assert_eq!(c.mined, (0..20).map(Some).collect::<Vec<_>>());
            assert_eq!(c.most_in_pool, window, "at most a window in the pool at once, and that many");
            // the first goes out alone and the rest once it's in; the last once everything else is
            assert!(c.sends.iter().all(|&(i, at)| i == 0 || at >= 1));
            assert_eq!(c.sends.iter().find(|s| s.0 == 19), Some(&(19, 19)));
            assert_eq!(deployed.len(), 18);
        }
        // all at once (a node on this machine): what the pool drops is sent again
        let (url, c) = chain(Chain::default());
        let (landed, _, _) = send(&conn(url), &txs(), &HashMap::new(), usize::MAX);
        assert_eq!(landed, vec![Some(true); 20]);
        assert_eq!(c.lock().unwrap().most_in_pool, 16);
    }

    #[test]
    fn an_error_is_looked_up_by_hash() {
        // the RPC answers two sends with errors though it took them: they're in, not sent again or given up on
        let (url, c) = chain(Chain { errs: vec![0, 5], ..Default::default() });
        let (landed, _, _) = send(&conn(url), &txs(), &HashMap::new(), 8);
        assert_eq!(landed, vec![Some(true); 20]);
        assert_eq!(c.lock().unwrap().sends.iter().filter(|s| s.0 == 5).count(), 1);
        // the chain's copy of a transaction isn't the planned one: that's where it stops
        let (url, c) = chain(Chain::default());
        c.lock().unwrap().plan[4]["input"] = json!("0x00");
        let (landed, _, _) = send(&conn(url), &txs(), &HashMap::new(), 8);
        assert_eq!(landed[4], Some(false));
        assert!(c.lock().unwrap().sends.iter().all(|s| s.0 != 19), "the last waits for everything before it");
    }

    #[test]
    fn never_sends_what_uses_a_failed_create() {
        let (url, c) = chain(Chain { fails: Some(2), ..Default::default() });
        let (landed, _, sent) = send(&conn(url), &txs(), &HashMap::new(), 8);
        let c = c.lock().unwrap();
        assert_eq!(landed[..3], [Some(true), Some(true), Some(false)]);
        assert!(landed[3..].iter().all(|l| l.is_none()));
        assert!(c.sends.iter().all(|&(i, _)| i < 3), "{:?}", c.sends);
        assert_eq!(sent, 3);
    }

    #[test]
    fn stops_at_a_nonce_someone_else_took() {
        // nonce 6 goes to another transaction: 7 to 13, out already, still land on top, but nothing more is sent
        let (url, c) = chain(Chain { foreign: Some(6), ..Default::default() });
        let t = Instant::now();
        let (landed, _, sent) = send(&conn(url), &txs(), &HashMap::new(), 8);
        assert!(t.elapsed() < Duration::from_secs(10));
        assert_eq!(landed[..6], [Some(true); 6]);
        assert_eq!(landed[6], Some(false));
        assert_eq!(landed[7..14], [Some(true); 7]);
        assert_eq!(sent, 14);
        assert!(c.lock().unwrap().sends.iter().all(|s| s.0 < 14), "{:?}", c.lock().unwrap().sends);
    }

    #[test]
    fn resumes_by_hash() {
        // stopped after 3 of 20; then 2 more landed. What was saved is sent as it was signed
        let (url, c) = chain(Chain::default());
        c.lock().unwrap().mined = (0..5).map(Some).collect();
        let saved = serde_json::to_vec(&Resume { names: HashMap::new(), proxies: vec![], left: txs().split_off(3), void: None }).unwrap();
        let r: Resume = serde_json::from_slice(&saved).unwrap();
        assert_eq!((r.left[0].n, r.left[0].tx.to, r.left[1].sim.creates.len()), (4, Some(Address::repeat_byte(1).create(2)), 1));
        check_resume(&url, &r.left).unwrap();
        check_resume(&url, &r.left[2..]).unwrap();
        let (landed, deployed, _) = send(&conn(url), &r.left, &HashMap::new(), 8);
        assert_eq!(landed, vec![Some(true); 17]);
        assert_eq!(c.lock().unwrap().mined, (0..20).map(Some).collect::<Vec<_>>());
        assert_eq!(deployed.len(), 15);
        // not over a nonce someone else took, nor while the pool holds the sender's transactions
        let (url, c) = chain(Chain::default());
        c.lock().unwrap().mined = vec![Some(0), Some(1), None, Some(3)];
        assert!(check_resume(&url, &txs()[1..]).unwrap_err().contains("nonce 2 of"));
        let (url, c) = chain(Chain::default());
        c.lock().unwrap().mined = vec![Some(0)];
        c.lock().unwrap().pool = [1].into();
        assert!(check_resume(&url, &txs()[1..]).unwrap_err().contains("waiting in a node's pool"));
    }
}
