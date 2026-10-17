//! revm wiring: database (optionally forked over JSON-RPC), executor, cheatcode inspector.
use crate::decode;
use crate::solc::Artifact;
use alloy_dyn_abi::{DynSolType, DynSolValue};
use alloy_primitives::{Address, B256, Bytes, I256, Log, U256, address, keccak256};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::journaled_state::account::JournaledAccountTr;
use revm::context_interface::result::{ExecutionResult, Output};
use revm::database::CacheDB;
use revm::database_interface::{DBErrorMarker, DatabaseRef};
use revm::interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Gas, InstructionResult, InterpreterResult};
use revm::primitives::{TxKind, hardfork::SpecId};
use revm::state::{AccountInfo, Bytecode};
use revm::{Context, DatabaseCommit, InspectEvm, Inspector, Journal, MainBuilder};
use revm::context::JournalTr;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub const CHEATS: Address = address!("7109709ECfa91a80626fF3989D68f67F5b1DD12D");
pub const CONSOLE: Address = address!("000000000000000000636F6e736F6c652e6c6f67");
pub const SENDER: Address = address!("1804c8AB1F12E6bbf3894d4083f33e07309d1f38");
pub const CREATE2_DEPLOYER: Address = address!("4e59b44847b379578588920cA78FbF26c0B4956C");
const CREATE2_DEPLOYER_CODE: &str = "7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";
pub const GAS_LIMIT: u64 = 1 << 30;
pub const ASSUME_MAGIC: &[u8] = b"FOUNDRY::ASSUME";
pub const SKIP_MAGIC: &[u8] = b"FOUNDRY::SKIP";

// ---------- database ----------

#[derive(Debug)]
pub struct DbErr(pub String);
impl std::fmt::Display for DbErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DbErr {}
impl DBErrorMarker for DbErr {}

/// Read-through backend: a frozen parent state (see `overlay`), else a remote chain at a fixed block
/// (results memoized across clones), else empty.
#[derive(Clone, Default)]
pub struct Remote(pub Option<Arc<Fork>>, pub Option<Arc<Db>>);

/// An empty, writable state on top of `base`: what a test run needs, without copying `base`.
pub fn overlay(base: &Arc<Db>) -> Db {
    Db::new(Remote(None, Some(base.clone())))
}

pub struct Fork {
    pub url: String,
    /// the block read from; steps back while the RPC's nodes don't have it yet (load-balanced public RPCs
    /// answer `latest` from the newest node and state from a lagging one), then stays put
    block: std::sync::atomic::AtomicU64,
    settled: std::sync::atomic::AtomicBool,
    accounts: Mutex<HashMap<Address, AccountInfo>>,
    slots: Mutex<HashMap<(Address, U256), U256>>,
}

impl Fork {
    pub fn new(url: &str, block: Option<u64>) -> Fork {
        let block = block.unwrap_or_else(|| {
            let b = crate::rpc::call(url, "eth_blockNumber", serde_json::json!([])).unwrap_or_else(|e| crate::die(&format!("fork {url}: {e}")));
            u64::from_str_radix(b.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0)
        });
        Fork { url: url.into(), block: block.into(), settled: false.into(), accounts: Default::default(), slots: Default::default() }
    }

    pub fn block(&self) -> u64 {
        self.block.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Calls with the fork's block appended to each call's params.
    fn q(&self, calls: &[(&str, serde_json::Value)]) -> Result<Vec<serde_json::Value>, DbErr> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut steps = 0;
        loop {
            let b = serde_json::json!(format!("0x{:x}", self.block()));
            let at: Vec<(&str, serde_json::Value)> = calls.iter().map(|(m, p)| (*m, p.as_array().map(|a| a.iter().cloned().chain([b.clone()]).collect()).unwrap_or_default())).collect();
            match crate::rpc::batch(&self.url, &at) {
                Err(e) if !self.settled.load(Relaxed) && steps < 8 && ["unknown block", "header not found"].iter().any(|t| e.to_lowercase().contains(t)) => {
                    steps += 1;
                    self.block.fetch_sub(1, Relaxed);
                }
                r => {
                    self.settled.store(r.is_ok(), Relaxed);
                    return r.map_err(DbErr);
                }
            }
        }
    }
}

fn hexu(v: &serde_json::Value) -> U256 {
    v.as_str().and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or_default()
}

impl DatabaseRef for Remote {
    type Error = DbErr;
    fn basic_ref(&self, a: Address) -> Result<Option<AccountInfo>, DbErr> {
        if let Some(p) = &self.1 {
            return p.basic_ref(a);
        }
        let Some(f) = &self.0 else { return Ok(None) };
        if let Some(i) = f.accounts.lock().unwrap().get(&a) {
            return Ok(Some(i.clone()));
        }
        let p = serde_json::json!([a]);
        let r = f.q(&[("eth_getBalance", p.clone()), ("eth_getTransactionCount", p.clone()), ("eth_getCode", p)])?;
        let (bal, nonce, code) = (hexu(&r[0]), hexu(&r[1]).to::<u64>(), &r[2]);
        let code = hex::decode(code.as_str().unwrap_or("0x").trim_start_matches("0x")).unwrap_or_default();
        let bc = Bytecode::new_raw(code.into());
        let info = AccountInfo::new(bal, nonce, bc.hash_slow(), bc);
        f.accounts.lock().unwrap().insert(a, info.clone());
        Ok(Some(info))
    }
    fn code_by_hash_ref(&self, h: B256) -> Result<Bytecode, DbErr> {
        match &self.1 {
            Some(p) => p.code_by_hash_ref(h),
            None => Ok(Bytecode::default()), // forked code always arrives with basic_ref
        }
    }
    fn storage_ref(&self, a: Address, k: U256) -> Result<U256, DbErr> {
        if let Some(p) = &self.1 {
            return p.storage_ref(a, k);
        }
        let Some(f) = &self.0 else { return Ok(U256::ZERO) };
        if let Some(v) = f.slots.lock().unwrap().get(&(a, k)) {
            return Ok(*v);
        }
        let v = hexu(&f.q(&[("eth_getStorageAt", serde_json::json!([a, format!("0x{k:x}")]))])?[0]);
        f.slots.lock().unwrap().insert((a, k), v);
        Ok(v)
    }
    fn block_hash_ref(&self, n: u64) -> Result<B256, DbErr> {
        match &self.1 {
            Some(p) => p.block_hash_ref(n),
            None => Ok(keccak256(n.to_be_bytes())),
        }
    }
}

pub type Db = CacheDB<Remote>;
pub type Ctx = Context<BlockEnv, TxEnv, CfgEnv, Db, Journal<Db>, ()>;

pub fn base_db(remote: Remote) -> Db {
    let mut db = Db::new(remote);
    db.insert_account_info(CHEATS, AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[0x01]))));
    let c2 = Bytecode::new_raw(hex::decode(CREATE2_DEPLOYER_CODE).unwrap().into());
    db.insert_account_info(CREATE2_DEPLOYER, AccountInfo::default().with_code(c2));
    db.insert_account_info(SENDER, AccountInfo::from_balance(U256::from(u128::MAX)));
    db
}

#[derive(Clone)]
pub struct Env {
    pub block: BlockEnv,
    pub chain_id: u64,
    pub gas_price: u128,
}

impl Env {
    pub fn new(chain_id: u64) -> Env {
        let mut block = BlockEnv::default();
        block.number = U256::from(1);
        block.timestamp = U256::from(1);
        block.gas_limit = GAS_LIMIT;
        block.basefee = 0;
        block.prevrandao = Some(B256::ZERO);
        Env { block, chain_id, gas_price: 0 }
    }
}

pub struct Exec {
    pub ok: bool,
    pub output: Bytes,
    pub gas: u64,
    pub logs: Vec<Log>,
    pub address: Option<Address>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Commit {
    Always,
    Never,
    OnSuccess,
}

/// Run one transaction through the cheatcode inspector, committing state into `db` per `commit`.
pub fn transact(db: Db, env: &mut Env, cheats: &mut Cheats, from: Address, to: TxKind, data: Bytes, value: U256, commit: impl Into<Commit>) -> (Db, Exec) {
    let mut vm = Vm::new(db, env, cheats);
    let r = vm.call(from, to, data, value, commit);
    (vm.finish(env), r)
}

/// One EVM for a sequence of transactions (e.g. an invariant run): its frames and buffers are reused.
pub struct Vm<'c> {
    evm: revm::MainnetEvm<Ctx, &'c mut Cheats>,
    gas_price: u128,
}

impl<'c> Vm<'c> {
    pub fn new(db: Db, env: &Env, cheats: &'c mut Cheats) -> Vm<'c> {
        let mut ctx: Ctx = Context::new(db, SpecId::OSAKA);
        ctx.block = env.block.clone();
        ctx.cfg.chain_id = env.chain_id;
        ctx.cfg.disable_nonce_check = true;
        ctx.cfg.disable_block_gas_limit = true;
        ctx.cfg.disable_base_fee = true;
        ctx.cfg.tx_gas_limit_cap = Some(u64::MAX);
        ctx.cfg.limit_contract_code_size = Some(usize::MAX);
        ctx.cfg.limit_contract_initcode_size = Some(usize::MAX);
        ctx.cfg.disable_eip3607 = true; // pranked/broadcast senders may be contracts
        Vm { evm: ctx.build_mainnet_with_inspector(cheats), gas_price: env.gas_price }
    }

    pub fn call(&mut self, from: Address, to: TxKind, data: Bytes, value: U256, commit: impl Into<Commit>) -> Exec {
        let commit = commit.into();
        let mut tx = TxEnv::default();
        tx.caller = from;
        tx.gas_limit = GAS_LIMIT;
        tx.gas_price = self.gas_price;
        tx.kind = to;
        tx.data = data;
        tx.value = value;
        tx.chain_id = Some(self.evm.ctx.cfg.chain_id);
        let res = self.evm.inspect_tx(tx).map_err(|e| e.to_string()).map(|r| {
            if commit == Commit::Always || (commit == Commit::OnSuccess && r.result.is_success()) {
                self.evm.ctx.journaled_state.database.commit(r.state);
            }
            r.result
        });
        match res {
            Ok(ExecutionResult::Success { gas, logs, output, .. }) => {
                let (output, address) = match output {
                    Output::Call(o) => (o, None),
                    Output::Create(o, a) => (o, a),
                };
                Exec { ok: true, output, gas: gas.tx_gas_used(), logs, address }
            }
            Ok(ExecutionResult::Revert { gas, logs, output }) => Exec { ok: false, output, gas: gas.tx_gas_used(), logs, address: None },
            Ok(ExecutionResult::Halt { reason, gas, logs }) => {
                Exec { ok: false, output: decode::revert_msg(&format!("EvmError: {reason:?}")), gas: gas.tx_gas_used(), logs, address: None }
            }
            Err(e) => Exec { ok: false, output: decode::revert_msg(&format!("EvmError: {e}")), gas: 0, logs: vec![], address: None },
        }
    }

    pub fn cheats(&mut self) -> &mut Cheats {
        self.evm.inspector
    }

    /// The state, with block/chain changes made by cheatcodes copied back into `env`.
    pub fn finish(self, env: &mut Env) -> Db {
        env.block = self.evm.ctx.block.clone();
        env.chain_id = self.evm.ctx.cfg.chain_id;
        self.evm.ctx.journaled_state.database
    }
}

impl From<bool> for Commit {
    fn from(b: bool) -> Commit {
        if b { Commit::Always } else { Commit::Never }
    }
}

// ---------- cheatcodes ----------

#[derive(Clone)]
struct Prank {
    sender: Address,
    origin: Option<Address>,
    depth: usize,
    single: bool,
}

#[derive(Clone)]
struct ExpectRevert {
    data: Option<Vec<u8>>,
    partial: bool,
    depth: usize,
}

#[derive(Clone)]
struct ExpectEmit {
    depth: usize,
    checks: [bool; 4],
    emitter: Option<Address>,
    log: Option<Log>,
    found: bool,
}

#[derive(Clone)]
pub struct BroadcastTx {
    pub from: Address,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    /// address the simulation created (for creates)
    pub created: Option<Address>,
}

#[derive(Clone)]
pub struct TraceNode {
    pub depth: usize,
    pub to: Address,
    pub input: Bytes,
    pub value: U256,
    pub create: bool,
    pub result: Option<(InstructionResult, Bytes)>,
}

#[derive(Clone)]
struct Broadcast {
    sender: Address,
    depth: usize,
    single: bool,
}

/// Cheatcode + console state. Cloned per test from the post-setUp state.
#[derive(Clone, Default)]
pub struct Cheats {
    pub artifacts: Arc<Vec<Artifact>>,
    pub root: std::path::PathBuf,
    pub console: Vec<String>,
    pub labels: HashMap<Address, String>,
    pub broadcasts: Vec<BroadcastTx>,
    pub keys: HashMap<Address, B256>,
    pub default_broadcaster: Option<Address>,
    pub failed_expectations: Vec<String>,
    pub fork_url: Option<String>,
    prank: Option<Prank>,
    origin_restore: Option<(usize, Address)>,
    expect_revert: Option<ExpectRevert>,
    revert_armed: Option<usize>,
    emits: Vec<ExpectEmit>,
    recorded: Option<Vec<Log>>,
    snapshots: Vec<Option<(revm::state::EvmState, BlockEnv)>>,
    broadcast: Option<Broadcast>,
    mocks: Vec<(Address, Vec<u8>, Vec<u8>, bool)>,
    expected_calls: Vec<(Address, Vec<u8>, usize)>,
    last_call_gas: HashMap<usize, u64>,
    gas_snapshot: Option<u64>,
    pub allow_internal_revert: bool,
    cheat_gas: u64,
    /// address -> contract name for contracts created while `track_creates` is on
    pub names: HashMap<Address, String>,
    pub track_creates: bool,
    /// every contract created while `track_creates` is on, with its init code (for verification)
    pub creates: Vec<(Address, Bytes)>,
    pub trace: Option<Vec<TraceNode>>,
    trace_stack: Vec<usize>,
    bc_creates: Vec<Option<usize>>,
    /// set when ds-test/forge-std `fail()` writes the global "failed" slot
    pub failed: bool,
    /// libraries deployed for the suite ("file:Name" -> address), used to link getCode results
    pub libs: HashMap<String, Address>,
}

fn revert_bytes(msg: &str) -> Vec<u8> {
    decode::revert_msg(msg).to_vec()
}

const DUMMY_OUTPUT: [u8; 8192] = [0; 8192];

fn outcome(ok: bool, out: Vec<u8>, inputs: &CallInputs) -> CallOutcome {
    CallOutcome::new(
        InterpreterResult {
            result: if ok { InstructionResult::Return } else { InstructionResult::Revert },
            output: out.into(),
            gas: Gas::new(inputs.gas_limit),
        },
        inputs.return_memory_offset.clone(),
    )
}

macro_rules! sigs {
    ($($s:literal),* $(,)?) => { &[$($s),*] };
}

/// Supported cheatcodes, by canonical signature.
const SUPPORTED: &[&str] = sigs![
    "prank(address)", "prank(address,address)", "startPrank(address)", "startPrank(address,address)", "stopPrank()",
    "deal(address,uint256)", "warp(uint256)", "roll(uint256)", "fee(uint256)", "chainId(uint256)", "prevrandao(bytes32)",
    "prevrandao(uint256)", "difficulty(uint256)", "coinbase(address)", "txGasPrice(uint256)",
    "getBlockTimestamp()", "getBlockNumber()", "store(address,bytes32,bytes32)", "load(address,bytes32)", "etch(address,bytes)",
    "getNonce(address)", "setNonce(address,uint64)", "setNonceUnsafe(address,uint64)", "resetNonce(address)",
    "addr(uint256)", "sign(uint256,bytes32)", "signCompact(uint256,bytes32)", "rememberKey(uint256)", "label(address,string)", "getLabel(address)",
    "assume(bool)", "skip(bool)", "expectRevert()", "expectRevert(bytes4)", "expectRevert(bytes)", "expectRevert(address)",
    "expectRevert(bytes4,address)", "expectRevert(bytes,address)", "expectPartialRevert(bytes4)", "expectPartialRevert(bytes4,address)",
    "expectEmit()", "expectEmit(address)", "expectEmit(bool,bool,bool,bool)", "expectEmit(bool,bool,bool,bool,address)",
    "recordLogs()", "getRecordedLogs()", "snapshotState()", "snapshot()", "revertToState(uint256)", "revertTo(uint256)",
    "revertToStateAndDelete(uint256)", "revertToAndDelete(uint256)", "deleteStateSnapshot(uint256)", "deleteSnapshot(uint256)",
    "envUint(string)", "envInt(string)", "envAddress(string)", "envBool(string)", "envBytes32(string)", "envString(string)",
    "envBytes(string)", "envExists(string)", "envOr(string,uint256)", "envOr(string,int256)", "envOr(string,address)",
    "envOr(string,bool)", "envOr(string,bytes32)", "envOr(string,string)", "envOr(string,bytes)", "setEnv(string,string)",
    "toString(address)", "toString(uint256)", "toString(int256)", "toString(bytes32)", "toString(bool)", "toString(bytes)",
    "getCode(string)", "getDeployedCode(string)", "mockCall(address,bytes,bytes)", "mockCall(address,uint256,bytes,bytes)",
    "mockCallRevert(address,bytes,bytes)", "clearMockedCalls()", "expectCall(address,bytes)", "expectCall(address,uint256,bytes)",
    "pauseGasMetering()", "resumeGasMetering()", "startBroadcast()", "startBroadcast(address)", "startBroadcast(uint256)",
    "broadcast()", "broadcast(address)", "broadcast(uint256)", "stopBroadcast()", "computeCreateAddress(address,uint256)",
    "computeCreate2Address(bytes32,bytes32,address)", "computeCreate2Address(bytes32,bytes32)", "projectRoot()",
    "readFile(string)", "writeFile(string,string)", "exists(string)", "unixTime()", "rpcUrl(string)", "activeFork()",
    "createSelectFork(string)", "createSelectFork(string,uint256)", "createFork(string)", "createFork(string,uint256)",
    "selectFork(uint256)", "isPersistent(address)", "makePersistent(address)", "allowCheatcodes(address)",
    "snapshotGasLastCall(string)", "snapshotGasLastCall(string,string)", "startSnapshotGas(string)", "startSnapshotGas(string,string)",
    "stopSnapshotGas()", "stopSnapshotGas(string)", "stopSnapshotGas(string,string)", "snapshotValue(string,uint256)",
    "snapshotValue(string,string,uint256)", "ffi(string[])", "readFileBinary(string)",
    "parseUint(string)", "parseInt(string)", "parseAddress(string)", "parseBool(string)", "parseBytes32(string)", "parseBytes(string)",
];

pub struct Sig {
    pub name: String,
    pub sig: String,
    pub ty: DynSolType,
}

fn sig(s: &str) -> Option<([u8; 4], Sig)> {
    let (name, rest) = s.split_once('(')?;
    let ty = DynSolType::parse(&format!("({}", rest)).ok()?;
    let sel: [u8; 4] = keccak256(s.as_bytes())[..4].try_into().unwrap();
    Some((sel, Sig { name: name.into(), sig: s.into(), ty }))
}

pub fn supported() -> &'static HashMap<[u8; 4], Sig> {
    static T: OnceLock<HashMap<[u8; 4], Sig>> = OnceLock::new();
    T.get_or_init(|| SUPPORTED.iter().filter_map(|s| sig(s)).collect())
}

/// Every Vm signature from the project's forge-std Vm.sol (for assert formatting + naming unsupported cheatcodes).
pub fn vm_sol() -> &'static HashMap<[u8; 4], Sig> {
    static T: OnceLock<HashMap<[u8; 4], Sig>> = OnceLock::new();
    T.get_or_init(|| {
        let p = crate::project::Project::load();
        let Some(text) = p.read(&p.resolve_import("", "forge-std/Vm.sol")) else { return HashMap::new() };
        let mut out = HashMap::new();
        for chunk in text.split("function ").skip(1) {
            let Some((name, rest)) = chunk.split_once('(') else { continue };
            let Some(params) = rest.split(')').next() else { continue };
            let types: Vec<&str> = params.split(',').filter_map(|p| p.split_whitespace().next()).collect();
            if let Some((sel, s)) = sig(&format!("{}({})", name.trim(), types.join(","))) {
                out.insert(sel, s);
            }
        }
        out
    })
}

pub fn pk_to_addr(pk: U256) -> Result<Address, String> {
    let key = k256::ecdsa::SigningKey::from_slice(&pk.to_be_bytes::<32>()).map_err(|e| format!("invalid private key: {e}"))?;
    let pt = key.verifying_key().to_encoded_point(false);
    Ok(Address::from_slice(&keccak256(&pt.as_bytes()[1..])[12..]))
}

/// Hex bytecode with library placeholders filled from `libs` ("file:Name" -> address).
pub fn link_code(code: &str, links: &serde_json::Value, libs: &HashMap<String, Address>) -> Result<Vec<u8>, String> {
    let mut code = code.to_string();
    for (file, names) in links.as_object().into_iter().flatten() {
        for (name, refs) in names.as_object().into_iter().flatten() {
            let addr = libs.get(&format!("{file}:{name}")).ok_or(format!("library {file}:{name} is not deployed"))?;
            let hexaddr = hex::encode(addr);
            for r in refs.as_array().into_iter().flatten() {
                let start = r["start"].as_u64().unwrap_or_default() as usize * 2;
                code.replace_range(start..start + 40, &hexaddr);
            }
        }
    }
    hex::decode(&code).map_err(|e| format!("bad bytecode: {e}"))
}

fn find_artifact<'a>(arts: &'a [Artifact], what: &str) -> Option<&'a Artifact> {
    let (file, name) = match what.split_once(':') {
        Some((f, n)) => (f, Some(n)),
        None if what.ends_with(".sol") => (what, None),
        None => ("", Some(what)),
    };
    let fname = file.rsplit('/').next().unwrap_or(file);
    arts.iter().find(|a| {
        (file.is_empty() || a.file == file || a.file.ends_with(&format!("/{file}")) || a.file.rsplit('/').next() == Some(fname))
            && name.is_none_or(|n| a.name == n)
            && (name.is_some() || Some(a.name.as_str()) == fname.strip_suffix(".sol"))
            && !a.bytecode.is_empty()
    })
}

fn env(k: &str) -> Result<String, String> {
    std::env::var(k).map_err(|_| format!("environment variable {k:?} not found"))
}

/// Parse a string the way forge's env/parse cheatcodes do, according to the target type.
fn parse_as(ty: &DynSolType, s: &str) -> Result<DynSolValue, String> {
    let s = s.trim();
    let bad = || format!("failed parsing {s:?} as {ty}");
    Ok(match ty {
        DynSolType::Uint(_) => DynSolValue::Uint(
            if let Some(h) = s.strip_prefix("0x") { U256::from_str_radix(h, 16) } else { U256::from_str_radix(s, 10) }.map_err(|_| bad())?,
            256,
        ),
        DynSolType::Int(_) => DynSolValue::Int(s.parse::<I256>().map_err(|_| bad())?, 256),
        DynSolType::Address => DynSolValue::Address(s.parse().map_err(|_| bad())?),
        DynSolType::Bool => DynSolValue::Bool(match s.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err(bad()),
        }),
        DynSolType::FixedBytes(32) => {
            let b = hex::decode(s.trim_start_matches("0x")).map_err(|_| bad())?;
            let mut w = [0u8; 32];
            w[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
            DynSolValue::FixedBytes(B256::from(w), 32)
        }
        DynSolType::Bytes => DynSolValue::Bytes(hex::decode(s.trim_start_matches("0x")).map_err(|_| bad())?),
        _ => DynSolValue::String(s.to_string()),
    })
}

fn u(v: &DynSolValue) -> U256 {
    v.as_uint().map(|x| x.0).unwrap_or_default()
}
fn a(v: &DynSolValue) -> Address {
    v.as_address().unwrap_or_default()
}
fn b32(v: &DynSolValue) -> B256 {
    v.as_fixed_bytes().map(|(b, _)| B256::left_padding_from(b)).unwrap_or_default()
}
fn by(v: &DynSolValue) -> Vec<u8> {
    match v {
        DynSolValue::FixedBytes(b, n) => b[..*n].to_vec(),
        v => v.as_bytes().unwrap_or_default().to_vec(),
    }
}
fn st(v: &DynSolValue) -> String {
    v.as_str().unwrap_or_default().to_string()
}
fn enc(v: DynSolValue) -> Vec<u8> {
    DynSolValue::Tuple(vec![v]).abi_encode_params()
}

impl Cheats {
    pub fn new(artifacts: Arc<Vec<Artifact>>, root: std::path::PathBuf) -> Cheats {
        Cheats { artifacts, root, ..Default::default() }
    }

    /// Reset per-transaction expectations; returns problems left pending at end of a test.
    pub fn finish(&mut self) -> Option<String> {
        let mut problems = std::mem::take(&mut self.failed_expectations);
        if self.expect_revert.take().is_some() {
            problems.push("next call did not revert as expected".into());
        }
        if self.emits.drain(..).any(|e| !e.found) {
            problems.push("expected an emit that was never made".into());
        }
        for (addr, data, n) in self.expected_calls.drain(..) {
            if n == 0 {
                problems.push(format!("expected call to {addr} with data 0x{} was not made", hex::encode(data)));
            }
        }
        self.prank = None;
        (!problems.is_empty()).then(|| problems.join("; "))
    }

    fn cheat(&mut self, ctx: &mut Ctx, caller: Address, depth: usize, data: &[u8]) -> Result<Vec<u8>, String> {
        if data.len() < 4 {
            return Err("invalid cheatcode call".into());
        }
        let sel: [u8; 4] = data[..4].try_into().unwrap();
        let Some(s) = supported().get(&sel) else {
            if let Some(s) = vm_sol().get(&sel) {
                if s.name.starts_with("assert") {
                    // fast path: forge-std calls some asserts unconditionally; equal static words need no decoding
                    let d = &data[4..];
                    let word = |t: &DynSolType| matches!(t, DynSolType::Uint(_) | DynSolType::Int(_) | DynSolType::FixedBytes(_) | DynSolType::Address | DynSolType::Bool);
                    if let DynSolType::Tuple(ts) = &s.ty {
                        if d.len() >= 64 && ts.len() >= 2 && word(&ts[0]) && word(&ts[1]) && (s.name == "assertEq") == (d[..32] == d[32..64]) && s.name.starts_with("assert") && (s.name == "assertEq" || s.name == "assertNotEq") {
                            return Ok(vec![]);
                        }
                    }
                    return decode::assertion(&s.name, &s.ty, d).map(|_| vec![]);
                }
                return Err(format!("plain: unsupported cheatcode vm.{}", s.sig));
            }
            return Err(format!("plain: unsupported cheatcode 0x{}", hex::encode(sel)));
        };
        let args = match s.ty.abi_decode_params(&data[4..]) {
            Ok(DynSolValue::Tuple(v)) => v,
            _ => return Err(format!("bad arguments for vm.{}", s.sig)),
        };
        let j = &mut ctx.journaled_state;
        let ok = Ok(vec![]);
        let gas_left = self.cheat_gas;
        match s.sig.as_str() {
            "prank(address)" | "startPrank(address)" | "prank(address,address)" | "startPrank(address,address)" => {
                self.prank = Some(Prank { sender: a(&args[0]), origin: args.get(1).map(a), depth, single: s.name == "prank" });
                ok
            }
            "stopPrank()" => {
                self.prank = None;
                ok
            }
            "deal(address,uint256)" => {
                let mut acc = j.load_account_mut(a(&args[0])).map_err(|e| e.to_string())?;
                acc.set_balance(u(&args[1]));
                acc.touch();
                ok
            }
            "warp(uint256)" => {
                ctx.block.timestamp = u(&args[0]);
                ok
            }
            "roll(uint256)" => {
                ctx.block.number = u(&args[0]);
                ok
            }
            "fee(uint256)" => {
                ctx.block.basefee = u(&args[0]).to();
                ok
            }
            "chainId(uint256)" => {
                ctx.cfg.chain_id = u(&args[0]).to();
                ok
            }
            "prevrandao(bytes32)" | "prevrandao(uint256)" | "difficulty(uint256)" => {
                let v = if s.sig.ends_with("(bytes32)") { b32(&args[0]) } else { B256::from(u(&args[0])) };
                ctx.block.prevrandao = Some(v);
                ctx.block.difficulty = v.into();
                ok
            }
            "coinbase(address)" => {
                ctx.block.beneficiary = a(&args[0]);
                ok
            }
            "txGasPrice(uint256)" => {
                ctx.tx.gas_price = u(&args[0]).to();
                ok
            }
            "getBlockTimestamp()" => Ok(enc(DynSolValue::Uint(ctx.block.timestamp, 256))),
            "getBlockNumber()" => Ok(enc(DynSolValue::Uint(ctx.block.number, 256))),
            "store(address,bytes32,bytes32)" => {
                let addr = a(&args[0]);
                if addr == CHEATS && b32(&args[1]) == B256::right_padding_from(b"failed") && !b32(&args[2]).is_zero() {
                    self.failed = true;
                }
                j.load_account(addr).map_err(|e| e.to_string())?;
                j.sstore(addr, b32(&args[1]).into(), b32(&args[2]).into()).map_err(|e| e.to_string())?;
                j.touch_account(addr);
                ok
            }
            "load(address,bytes32)" => {
                let addr = a(&args[0]);
                j.load_account(addr).map_err(|e| e.to_string())?;
                let v = j.sload(addr, b32(&args[1]).into()).map_err(|e| e.to_string())?.data;
                Ok(enc(DynSolValue::FixedBytes(B256::from(v), 32)))
            }
            "etch(address,bytes)" => {
                let addr = a(&args[0]);
                let mut acc = j.load_account_mut(addr).map_err(|e| e.to_string())?;
                acc.set_code_and_hash_slow(Bytecode::new_raw(by(&args[1]).into()));
                acc.touch();
                ok
            }
            "getNonce(address)" => {
                let n = j.load_account(a(&args[0])).map_err(|e| e.to_string())?.info.nonce;
                Ok(enc(DynSolValue::Uint(U256::from(n), 64)))
            }
            "setNonce(address,uint64)" | "setNonceUnsafe(address,uint64)" | "resetNonce(address)" => {
                let mut acc = j.load_account_mut(a(&args[0])).map_err(|e| e.to_string())?;
                acc.unsafe_set_nonce(args.get(1).map(|v| u(v).to()).unwrap_or(0));
                acc.touch();
                ok
            }
            "addr(uint256)" => Ok(enc(DynSolValue::Address(pk_to_addr(u(&args[0]))?))),
            "rememberKey(uint256)" => {
                let addr = pk_to_addr(u(&args[0]))?;
                self.keys.insert(addr, B256::from(u(&args[0])));
                Ok(enc(DynSolValue::Address(addr)))
            }
            "sign(uint256,bytes32)" | "signCompact(uint256,bytes32)" => {
                let key = k256::ecdsa::SigningKey::from_slice(&u(&args[0]).to_be_bytes::<32>()).map_err(|e| e.to_string())?;
                let (sig, rec) = key.sign_prehash_recoverable(b32(&args[1]).as_slice()).map_err(|e| e.to_string())?;
                let (r, s_) = (B256::from_slice(&sig.r().to_bytes()), B256::from_slice(&sig.s().to_bytes()));
                let fb = |b: B256| DynSolValue::FixedBytes(b, 32);
                if s.name == "sign" {
                    Ok(DynSolValue::Tuple(vec![DynSolValue::Uint(U256::from(27 + rec.to_byte()), 8), fb(r), fb(s_)]).abi_encode_params())
                } else {
                    let mut vs = s_;
                    if rec.is_y_odd() {
                        vs.0[0] |= 0x80;
                    }
                    Ok(DynSolValue::Tuple(vec![fb(r), fb(vs)]).abi_encode_params())
                }
            }
            "label(address,string)" => {
                self.labels.insert(a(&args[0]), st(&args[1]));
                ok
            }
            "getLabel(address)" => Ok(enc(DynSolValue::String(self.labels.get(&a(&args[0])).cloned().unwrap_or_default()))),
            "assume(bool)" => {
                if args[0].as_bool() == Some(true) {
                    ok
                } else {
                    Err(String::from_utf8_lossy(ASSUME_MAGIC).into())
                }
            }
            "skip(bool)" => {
                if args[0].as_bool() == Some(true) {
                    Err(String::from_utf8_lossy(SKIP_MAGIC).into())
                } else {
                    ok
                }
            }
            "expectRevert()" | "expectRevert(bytes4)" | "expectRevert(bytes)" | "expectRevert(address)" | "expectRevert(bytes4,address)"
            | "expectRevert(bytes,address)" | "expectPartialRevert(bytes4)" | "expectPartialRevert(bytes4,address)" => {
                // ponytail: the optional reverter address is accepted but not checked
                let data = args.iter().find(|v| !matches!(v, DynSolValue::Address(_))).map(by);
                self.expect_revert = Some(ExpectRevert { data, partial: s.name == "expectPartialRevert", depth });
                ok
            }
            "expectEmit()" | "expectEmit(address)" | "expectEmit(bool,bool,bool,bool)" | "expectEmit(bool,bool,bool,bool,address)" => {
                let mut checks = [true; 4];
                let mut emitter = None;
                for (i, v) in args.iter().enumerate() {
                    match v {
                        DynSolValue::Bool(b) => checks[i] = *b,
                        DynSolValue::Address(x) => emitter = Some(*x),
                        _ => {}
                    }
                }
                self.emits.push(ExpectEmit { depth, checks, emitter, log: None, found: false });
                ok
            }
            "recordLogs()" => {
                self.recorded = Some(vec![]);
                ok
            }
            "getRecordedLogs()" => {
                let logs = self.recorded.as_mut().map(std::mem::take).unwrap_or_default();
                let v = logs
                    .into_iter()
                    .map(|l| {
                        DynSolValue::Tuple(vec![
                            DynSolValue::Array(l.topics().iter().map(|t| DynSolValue::FixedBytes(*t, 32)).collect()),
                            DynSolValue::Bytes(l.data.data.to_vec()),
                            DynSolValue::Address(l.address),
                        ])
                    })
                    .collect();
                Ok(enc(DynSolValue::Array(v)))
            }
            "snapshotState()" | "snapshot()" => {
                // ponytail: snapshots copy the whole journaled state; fine for test-sized states
                self.snapshots.push(Some((j.inner.state.clone(), ctx.block.clone())));
                Ok(enc(DynSolValue::Uint(U256::from(self.snapshots.len() - 1), 256)))
            }
            "revertToState(uint256)" | "revertTo(uint256)" | "revertToStateAndDelete(uint256)" | "revertToAndDelete(uint256)" => {
                let id = u(&args[0]).to::<usize>();
                let Some(Some((state, block))) = self.snapshots.get(id).cloned() else {
                    return Ok(enc(DynSolValue::Bool(false)));
                };
                j.inner.state = state;
                ctx.block = block;
                if s.name.ends_with("AndDelete") {
                    self.snapshots[id] = None;
                }
                Ok(enc(DynSolValue::Bool(true)))
            }
            "deleteStateSnapshot(uint256)" | "deleteSnapshot(uint256)" => {
                let id = u(&args[0]).to::<usize>();
                let had = self.snapshots.get_mut(id).and_then(Option::take).is_some();
                Ok(enc(DynSolValue::Bool(had)))
            }
            "envExists(string)" => Ok(enc(DynSolValue::Bool(std::env::var(st(&args[0])).is_ok()))),
            "setEnv(string,string)" => {
                // SAFETY-ish: tests run in threads; forge has the same process-global semantics
                unsafe { std::env::set_var(st(&args[0]), st(&args[1])) };
                ok
            }
            sg if sg.starts_with("env") => {
                let key = st(&args[0]);
                let ty = match &s.ty {
                    DynSolType::Tuple(t) if t.len() == 2 => t[1].clone(),
                    _ => match s.name.as_str() {
                        "envUint" => DynSolType::Uint(256),
                        "envInt" => DynSolType::Int(256),
                        "envAddress" => DynSolType::Address,
                        "envBool" => DynSolType::Bool,
                        "envBytes32" => DynSolType::FixedBytes(32),
                        "envBytes" => DynSolType::Bytes,
                        _ => DynSolType::String,
                    },
                };
                match (std::env::var(&key), args.get(1)) {
                    (Ok(v), _) => Ok(enc(parse_as(&ty, &v)?)),
                    (Err(_), Some(default)) => Ok(enc(default.clone())),
                    (Err(_), None) => env(&key).map(|_| vec![]),
                }
            }
            sg if sg.starts_with("parse") => {
                let ty = match s.name.as_str() {
                    "parseUint" => DynSolType::Uint(256),
                    "parseInt" => DynSolType::Int(256),
                    "parseAddress" => DynSolType::Address,
                    "parseBool" => DynSolType::Bool,
                    "parseBytes32" => DynSolType::FixedBytes(32),
                    _ => DynSolType::Bytes,
                };
                Ok(enc(parse_as(&ty, &st(&args[0]))?))
            }
            sg if sg.starts_with("toString(") => Ok(enc(DynSolValue::String(decode::display(&args[0])))),
            "getCode(string)" | "getDeployedCode(string)" => {
                let what = st(&args[0]);
                let art = find_artifact(&self.artifacts, &what).ok_or(format!("no artifact for {what:?} (only project src/test/script contracts are compiled to bytecode)"))?;
                if s.name == "getCode" {
                    return Ok(enc(DynSolValue::Bytes(link_code(&art.bytecode, &art.links, &self.libs).map_err(|e| format!("getCode {what}: {e}"))?)));
                }
                if art.deployed.contains("__$") {
                    return Err(format!("{what} needs library linking, unsupported in getDeployedCode"));
                }
                Ok(enc(DynSolValue::Bytes(hex::decode(&art.deployed).unwrap_or_default())))
            }
            "mockCall(address,bytes,bytes)" | "mockCallRevert(address,bytes,bytes)" => {
                self.mocks.push((a(&args[0]), by(&args[1]), by(&args[2]), s.name == "mockCallRevert"));
                ok
            }
            "mockCall(address,uint256,bytes,bytes)" => {
                self.mocks.push((a(&args[0]), by(&args[2]), by(&args[3]), false));
                ok
            }
            "clearMockedCalls()" => {
                self.mocks.clear();
                ok
            }
            "expectCall(address,bytes)" | "expectCall(address,uint256,bytes)" => {
                self.expected_calls.push((a(&args[0]), by(args.last().unwrap()), 0));
                ok
            }
            "pauseGasMetering()" | "resumeGasMetering()" | "allowCheatcodes(address)" | "makePersistent(address)" => ok,
            "isPersistent(address)" => Ok(enc(DynSolValue::Bool(true))),
            "startBroadcast()" | "startBroadcast(address)" | "startBroadcast(uint256)" | "broadcast()" | "broadcast(address)" | "broadcast(uint256)" => {
                let sender = match args.first() {
                    Some(DynSolValue::Address(x)) => *x,
                    Some(v) => {
                        let addr = pk_to_addr(u(v))?;
                        self.keys.insert(addr, B256::from(u(v)));
                        addr
                    }
                    None => self.default_broadcaster.unwrap_or(caller),
                };
                self.broadcast = Some(Broadcast { sender, depth, single: s.name == "broadcast" });
                ok
            }
            "stopBroadcast()" => {
                self.broadcast = None;
                ok
            }
            "computeCreateAddress(address,uint256)" => Ok(enc(DynSolValue::Address(a(&args[0]).create(u(&args[1]).to())))),
            "computeCreate2Address(bytes32,bytes32,address)" | "computeCreate2Address(bytes32,bytes32)" => {
                let deployer = args.get(2).map(a).unwrap_or(CREATE2_DEPLOYER);
                Ok(enc(DynSolValue::Address(deployer.create2(b32(&args[0]), b32(&args[1])))))
            }
            "projectRoot()" => Ok(enc(DynSolValue::String(self.root.to_string_lossy().into()))),
            "readFile(string)" => {
                let p = self.root.join(st(&args[0]));
                Ok(enc(DynSolValue::String(std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)))
            }
            "writeFile(string,string)" => {
                let p = self.root.join(st(&args[0]));
                std::fs::write(&p, st(&args[1])).map_err(|e| format!("{}: {e}", p.display()))?;
                ok
            }
            "exists(string)" => Ok(enc(DynSolValue::Bool(self.root.join(st(&args[0])).exists()))),
            "unixTime()" => Ok(enc(DynSolValue::Uint(
                U256::from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()),
                256,
            ))),
            "rpcUrl(string)" => {
                let p = crate::project::Project::load();
                let name = st(&args[0]);
                Ok(enc(DynSolValue::String(crate::rpc::network_url(&p, &name).ok_or(format!("unknown network {name:?}"))?)))
            }
            "activeFork()" => Ok(enc(DynSolValue::Uint(U256::ZERO, 256))),
            "selectFork(uint256)" => ok,
            "createSelectFork(string)" | "createSelectFork(string,uint256)" | "createFork(string)" | "createFork(string,uint256)" => {
                // ponytail: one fork per run; pick it with `plain test --fork <url|network>`
                match &self.fork_url {
                    Some(_) => Ok(enc(DynSolValue::Uint(U256::ZERO, 256))),
                    None => Err("forking inside tests is unsupported; run with `plain test --fork <url|network>`".into()),
                }
            }
            sg if sg.starts_with("snapshotGasLastCall") => Ok(enc(DynSolValue::Uint(U256::from(self.last_call_gas.get(&depth).copied().unwrap_or(0)), 256))),
            sg if sg.starts_with("startSnapshotGas") => {
                self.gas_snapshot = Some(gas_left);
                ok
            }
            sg if sg.starts_with("stopSnapshotGas") => {
                // ponytail: measured from gas forwarded to the cheatcode calls; forge's numbers differ slightly. No snapshot files written.
                let used = self.gas_snapshot.take().map(|g| g.saturating_sub(gas_left)).unwrap_or(0);
                Ok(enc(DynSolValue::Uint(U256::from(used), 256)))
            }
            sg if sg.starts_with("snapshotValue") => ok,
            "ffi(string[])" => {
                let argv: Vec<String> = args[0].as_array().unwrap_or_default().iter().map(st).collect();
                let out = std::process::Command::new(argv.first().ok_or("ffi: empty command")?)
                    .args(&argv[1..])
                    .current_dir(&self.root)
                    .output()
                    .map_err(|e| format!("ffi {}: {e}", argv[0]))?;
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let bytes = hex::decode(text.trim_start_matches("0x")).unwrap_or_else(|_| text.into_bytes());
                Ok(enc(DynSolValue::Bytes(bytes)))
            }
            "readFileBinary(string)" => {
                let p = self.root.join(st(&args[0]));
                Ok(enc(DynSolValue::Bytes(std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?)))
            }
            other => Err(format!("plain: unsupported cheatcode vm.{other}")),
        }
    }
}

fn matches_revert(exp: &ExpectRevert, out: &[u8]) -> bool {
    let Some(d) = &exp.data else { return true };
    if out == d.as_slice() || ((exp.partial || d.len() == 4) && out.starts_with(d)) {
        return true;
    }
    // expectRevert("message") matches Error(string)
    decode::error_string(out).is_some_and(|s| s.as_bytes() == d.as_slice())
}

impl Cheats {
    fn check_revert(&mut self, depth: usize, result: &mut InterpreterResult) -> bool {
        let Some(armed) = self.revert_armed else { return false };
        if armed != depth {
            return false;
        }
        self.revert_armed = None;
        let exp = self.expect_revert.take().unwrap();
        if result.result.is_ok() {
            result.result = InstructionResult::Revert;
            result.output = decode::revert_msg("call did not revert as expected");
        } else if !matches_revert(&exp, &result.output) {
            let want = exp.data.as_deref().map(|d| decode::describe_bytes(d)).unwrap_or_default();
            result.output = decode::revert_msg(&format!("reverted with {:?}, expected {:?}", decode::reason(&result.output), want));
            result.result = InstructionResult::Revert;
        } else {
            result.result = InstructionResult::Return;
            result.output = Bytes::from_static(&DUMMY_OUTPUT);
        }
        true
    }

    fn check_emits(&mut self, depth: usize, result: &mut InterpreterResult) {
        if !self.emits.iter().any(|e| e.depth == depth && e.log.is_some()) {
            return;
        }
        let missing = self.emits.iter().any(|e| e.depth == depth && e.log.is_some() && !e.found);
        self.emits.retain(|e| !(e.depth == depth && e.log.is_some()));
        if missing && result.result.is_ok() {
            result.result = InstructionResult::Revert;
            result.output = decode::revert_msg("log != expected log");
        }
    }

    /// Shared pre-call logic for calls and creates made by a frame at `depth`.
    fn before(&mut self, ctx: &mut Ctx, depth: usize, caller: &mut Address, delegate: bool) -> Option<BroadcastTx> {
        let mut bc = None;
        if let Some(b) = self.broadcast.clone() {
            if b.depth == depth && !delegate {
                *caller = b.sender;
                if b.single {
                    self.broadcast = None;
                }
                bc = Some(BroadcastTx { from: b.sender, to: None, value: U256::ZERO, data: Bytes::new(), created: None });
            }
        }
        if let Some(p) = self.prank.clone() {
            if p.depth == depth && !delegate {
                *caller = p.sender;
                if let Some(o) = p.origin {
                    self.origin_restore = Some((depth, ctx.tx.caller));
                    ctx.tx.caller = o;
                }
                if p.single {
                    self.prank = None;
                }
            }
        }
        if self.expect_revert.as_ref().is_some_and(|e| e.depth == depth) && self.revert_armed.is_none() {
            self.revert_armed = Some(depth);
        }
        bc
    }

    fn after(&mut self, ctx: &mut Ctx, depth: usize) {
        if let Some((d, o)) = self.origin_restore {
            if d == depth {
                ctx.tx.caller = o;
                self.origin_restore = None;
            }
        }
    }
}

impl Inspector<Ctx> for Cheats {
    fn call(&mut self, ctx: &mut Ctx, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let depth = ctx.journaled_state.depth();
        if inputs.target_address == CHEATS {
            self.cheat_gas = inputs.gas_limit;
            let data = inputs.input.bytes(ctx);
            let caller = inputs.caller;
            let out = match self.cheat(ctx, caller, depth, &data) {
                Ok(out) => outcome(true, out, inputs),
                Err(msg) if msg.as_bytes() == ASSUME_MAGIC || msg.as_bytes() == SKIP_MAGIC => outcome(false, msg.into_bytes(), inputs),
                Err(msg) => outcome(false, revert_bytes(&msg), inputs),
            };
            if let Some(t) = &mut self.trace {
                t.push(TraceNode { depth, to: CHEATS, input: data, value: U256::ZERO, create: false, result: Some((out.result.result, out.result.output.clone())) });
            }
            return Some(out);
        }
        if inputs.target_address == CONSOLE {
            let data = inputs.input.bytes(ctx);
            self.console.push(decode::console(&data));
            return Some(outcome(true, vec![], inputs));
        }
        if !self.mocks.is_empty() || !self.expected_calls.is_empty() {
            let data = inputs.input.bytes(ctx);
            for (addr, d, n) in self.expected_calls.iter_mut() {
                if *addr == inputs.target_address && data.starts_with(d) {
                    *n += 1;
                }
            }
            if let Some((_, _, ret, rev)) = self.mocks.iter().rev().find(|(addr, d, _, _)| *addr == inputs.target_address && data.starts_with(d)) {
                let out = outcome(!rev, ret.clone(), inputs);
                if let Some(t) = &mut self.trace {
                    self.trace_stack.push(t.len()); // call_end still runs and pops it
                    t.push(TraceNode { depth, to: inputs.target_address, input: data.clone(), value: inputs.call_value(), create: false, result: None });
                }
                return Some(out);
            }
        }
        let delegate = inputs.scheme.is_delegate_call() || inputs.scheme.is_call_code();
        let mut caller = inputs.caller;
        if let Some(mut bc) = self.before(ctx, depth, &mut caller, delegate) {
            bc.to = Some(inputs.target_address);
            bc.value = inputs.call_value();
            bc.data = inputs.input.bytes(ctx);
            // a real tx bumps the sender nonce; keep simulated CREATE addresses in sync
            if let Ok(mut acc) = ctx.journaled_state.load_account_mut(caller) {
                acc.bump_nonce();
            }
            self.broadcasts.push(bc);
        }
        if caller != inputs.caller {
            let _ = ctx.journaled_state.load_account(caller); // revm's transfer expects the sender loaded
            inputs.caller = caller;
        }
        if let Some(t) = &mut self.trace {
            self.trace_stack.push(t.len());
            t.push(TraceNode { depth, to: inputs.target_address, input: inputs.input.bytes(ctx), value: inputs.call_value(), create: false, result: None });
        }
        None
    }

    fn call_end(&mut self, ctx: &mut Ctx, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == CHEATS || inputs.target_address == CONSOLE {
            return;
        }
        let depth = ctx.journaled_state.depth();
        self.after(ctx, depth);
        self.last_call_gas.insert(depth, outcome.result.gas.total_gas_spent());
        let traced = if self.trace.is_some() { self.trace_stack.pop() } else { None };
        if self.allow_internal_revert && self.revert_armed.is_none() && self.expect_revert.as_ref().is_some_and(|e| e.depth == depth + 1) {
            // the frame that called expectRevert reverted itself (internal call)
            self.revert_armed = Some(depth);
        }
        self.check_revert(depth, &mut outcome.result);
        self.check_emits(depth, &mut outcome.result);
        if let (Some(i), Some(t)) = (traced, &mut self.trace) {
            t[i].result = Some((outcome.result.result, outcome.result.output.clone()));
        }
    }

    fn create(&mut self, ctx: &mut Ctx, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let depth = ctx.journaled_state.depth();
        let mut caller = inputs.caller();
        if let Some(mut bc) = self.before(ctx, depth, &mut caller, false) {
            bc.value = inputs.value();
            bc.data = inputs.init_code().clone();
            if let revm::context_interface::CreateScheme::Create2 { salt } = inputs.scheme() {
                // forge semantics: CREATE2 in broadcasts goes through the deterministic deployer,
                // so simulate it from there too (same address and msg.sender as on chain)
                let mut d = salt.to_be_bytes::<32>().to_vec();
                d.extend_from_slice(inputs.init_code());
                bc.to = Some(CREATE2_DEPLOYER);
                bc.data = d.into();
                if let Ok(mut acc) = ctx.journaled_state.load_account_mut(caller) {
                    acc.bump_nonce();
                }
                caller = CREATE2_DEPLOYER;
            }
            self.bc_creates.push(Some(self.broadcasts.len()));
            self.broadcasts.push(bc);
        } else {
            self.bc_creates.push(None);
        }
        if caller != inputs.caller() {
            let _ = ctx.journaled_state.load_account(caller);
            inputs.set_call(caller);
        }
        if let Some(t) = &mut self.trace {
            self.trace_stack.push(t.len());
            t.push(TraceNode { depth, to: Address::ZERO, input: inputs.init_code().clone(), value: inputs.value(), create: true, result: None });
        }
        None
    }

    fn create_end(&mut self, ctx: &mut Ctx, inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        let depth = ctx.journaled_state.depth();
        self.after(ctx, depth);
        if let Some(Some(i)) = self.bc_creates.pop() {
            self.broadcasts[i].created = outcome.address;
        }
        if self.track_creates && outcome.result.result.is_ok() {
            if let Some(a) = outcome.address {
                self.creates.push((a, inputs.init_code().clone()));
                if let Some(n) = decode::contract_name(inputs.init_code(), &outcome.result.output) {
                    self.names.insert(a, n);
                }
            }
        }
        if self.check_revert(depth, &mut outcome.result) && outcome.result.result.is_ok() {
            outcome.address = Some(Address::with_last_byte(1));
        }
        self.check_emits(depth, &mut outcome.result);
        if let (Some(i), Some(t)) = (if self.trace.is_some() { self.trace_stack.pop() } else { None }, &mut self.trace) {
            t[i].to = outcome.address.unwrap_or_default();
            t[i].result = Some((outcome.result.result, outcome.result.output.clone()));
        }
    }

    fn log(&mut self, ctx: &mut Ctx, log: Log) {
        let depth = ctx.journaled_state.depth();
        if let Some(r) = &mut self.recorded {
            r.push(log.clone());
        }
        if self.emits.is_empty() {
            return;
        }
        if let Some(e) = self.emits.iter_mut().find(|e| e.log.is_none()) {
            if e.depth == depth {
                e.log = Some(log);
                return;
            }
        }
        if let Some(e) = self.emits.iter_mut().find(|e| e.log.is_some() && !e.found) {
            let want = e.log.as_ref().unwrap();
            let (wt, gt) = (want.topics(), log.topics());
            let mut ok = wt.len() == gt.len() && wt.first() == gt.first();
            for i in 1..wt.len().min(gt.len()).min(4) {
                ok &= !e.checks[i - 1] || wt[i] == gt[i];
            }
            ok &= !e.checks[3] || want.data.data == log.data.data;
            ok &= e.emitter.is_none_or(|x| x == log.address);
            if ok {
                e.found = true;
            }
        }
    }
}
