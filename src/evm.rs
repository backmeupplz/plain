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
/// where forge's test contract lands (SENDER's nonce 1), and the account forge deploys libraries from
pub const TEST_CONTRACT: Address = address!("7FA9385bE102ac3EAc297483Dd6233D62b3e1496");
pub const LIBRARY_DEPLOYER: Address = address!("1F95D37F27EA0dEA9C252FC09D5A6eaA97647353");
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
    hashes: Mutex<HashMap<u64, B256>>,
    /// accounts taken as empty without asking (see `presume`)
    presumed: Mutex<Vec<Address>>,
    /// a block a test asked for: never stepped back from
    pinned: bool,
    env: Mutex<Option<Env>>,
}

/// A u64 from a JSON-RPC quantity, strictly (see `rpc::quantity`).
fn num(v: &serde_json::Value) -> Result<u64, String> {
    crate::rpc::quantity(v)?.try_into().map_err(|_| format!("{v} is over 64 bits"))
}

impl Fork {
    pub fn new(url: &str, block: Option<u64>) -> Fork {
        let block = block.unwrap_or_else(|| crate::rpc::call(url, "eth_blockNumber", serde_json::json!([])).and_then(|b| num(&b)).unwrap_or_else(|e| crate::die(&format!("fork {url}: {e}"))));
        Fork { url: url.into(), block: block.into(), settled: false.into(), accounts: Default::default(), slots: Default::default(), hashes: Default::default(), presumed: Default::default(), pinned: false, env: Default::default() }
    }

    /// The fork `vm.createFork(url, block)` makes: one per RPC and block for the whole run, so tests that fork the
    /// same block read each slot once. Without a block, the RPC's latest one when it was first forked.
    pub fn at(url: &str, block: Option<u64>) -> Result<Arc<Fork>, String> {
        static LATEST: std::sync::LazyLock<Mutex<HashMap<String, u64>>> = std::sync::LazyLock::new(Default::default);
        static FORKS: std::sync::LazyLock<Mutex<HashMap<(String, u64), Arc<Fork>>>> = std::sync::LazyLock::new(Default::default);
        let latest = || -> Result<u64, String> {
            if let Some(b) = LATEST.lock().unwrap().get(url) {
                return Ok(*b);
            }
            let b = crate::rpc::call(url, "eth_blockNumber", serde_json::json!([])).and_then(|b| num(&b)).map_err(|e| format!("forking {url}: {e}"))?;
            Ok(*LATEST.lock().unwrap().entry(url.into()).or_insert(b))
        };
        let (block, pinned) = match block {
            Some(b) => (b, true),
            None => (latest()?, false),
        };
        let mut forks = FORKS.lock().unwrap();
        Ok(forks.entry((url.into(), block)).or_insert_with(|| Arc::new(Fork { pinned, ..Fork::new(url, Some(block)) })).clone())
    }

    /// Takes a contract's address-to-be as empty, as it nearly always is, instead of a round trip per contract
    /// created; `settle` checks them all at once.
    pub fn presume(&self, a: Address) {
        let mut accounts = self.accounts.lock().unwrap();
        if !accounts.contains_key(&a) {
            accounts.insert(a, AccountInfo::default());
            self.presumed.lock().unwrap().push(a);
        }
    }

    /// Fetches every presumed account in one batch; true if any isn't empty after all.
    pub fn settle(&self) -> Result<bool, DbErr> {
        let list = std::mem::take(&mut *self.presumed.lock().unwrap());
        if list.is_empty() {
            return Ok(false);
        }
        // chunks at once: a provider capping batch size falls back to single calls per chunk, not for all
        let infos: Vec<AccountInfo> = std::thread::scope(|s| {
            let hs: Vec<_> = list.chunks(8).map(|c| s.spawn(move || self.q(true, |b| c.iter().flat_map(|a| ACCOUNT.map(|m| (m, serde_json::json!([a, b.clone()])))).collect(), |r| r.chunks(3).map(account).collect::<Result<Vec<_>, _>>()))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect::<Result<Vec<_>, _>>()
        })?
        .concat();
        let mut accounts = self.accounts.lock().unwrap();
        let mut surprise = false;
        for (a, info) in list.iter().zip(infos) {
            surprise |= !info.is_empty();
            accounts.insert(*a, info);
        }
        Ok(surprise)
    }

    pub fn block(&self) -> u64 {
        self.block.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The forked block's environment (chain id, number, timestamp, fees), as tests on a fork see it.
    pub fn env(&self) -> Result<Env, DbErr> {
        if let Some(e) = &*self.env.lock().unwrap() {
            return Ok(e.clone());
        }
        let (chain, b) = self.q(true, |b| vec![("eth_chainId", serde_json::json!([])), ("eth_getBlockByNumber", serde_json::json!([b, false]))], |r| match r[1].is_object() {
            true => Ok((num(&r[0])?, r[1].clone())),
            false => Err("header not found (the RPC answered null for the block)".into()),
        })?;
        let bad = |e: String| DbErr(format!("block {} from {}: {e}", self.block(), self.url));
        let mut env = Env::new(chain);
        env.block.number = crate::rpc::quantity(&b["number"]).map_err(bad)?;
        env.block.timestamp = crate::rpc::quantity(&b["timestamp"]).map_err(bad)?;
        // blocks before London have no base fee
        env.block.basefee = if b["baseFeePerGas"].is_null() { 0 } else { num(&b["baseFeePerGas"]).map_err(bad)? };
        env.block.beneficiary = b["miner"].as_str().and_then(|a| a.parse().ok()).unwrap_or_default();
        env.block.prevrandao = b["mixHash"].as_str().and_then(|h| h.parse().ok()).or(Some(B256::ZERO));
        *self.env.lock().unwrap() = Some(env.clone());
        Ok(env)
    }

    /// The calls `calls` makes for the fork's block, answered and parsed. A read never guesses: an RPC error, or an
    /// answer `parse` rejects (null, "0x" for a number, a node that lost the block), is asked again a few times, and
    /// then fails the read. With `step`, a block the RPC doesn't have yet moves the fork back a block, until a read
    /// at the fork's block has succeeded: it stays put from then on.
    fn q<T>(&self, step: bool, calls: impl Fn(serde_json::Value) -> Vec<(&'static str, serde_json::Value)>, parse: impl Fn(&[serde_json::Value]) -> Result<T, String>) -> Result<T, DbErr> {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut steps, mut tries) = (0, 0);
        loop {
            let at = calls(serde_json::json!(format!("0x{:x}", self.block())));
            // (an odd answer, which is worth asking again: rpc::batch already retried what's transient, the rest is final)
            let (odd, e) = match crate::rpc::batch(&self.url, &at) {
                Ok(r) => match parse(&r) {
                    Ok(t) => {
                        // the RPC has the fork's block: it stays put from now on
                        if step {
                            self.settled.store(true, Relaxed);
                        }
                        return Ok(t);
                    }
                    Err(e) => (true, e),
                },
                Err(e) => (false, e),
            };
            let missing = ["unknown block", "header not found", "block not found"].iter().any(|t| e.to_lowercase().contains(t));
            if missing && step && !self.pinned && !self.settled.load(Relaxed) && steps < 8 {
                steps += 1;
                self.block.fetch_sub(1, Relaxed);
                continue;
            }
            if std::env::var_os("PLAIN_DEBUG").is_some() {
                eprintln!("fork read at block {}: {e}", self.block());
            }
            if !(odd || missing) || tries == 4 {
                let what = at.iter().map(|c| c.0).collect::<Vec<_>>().join(", ");
                return Err(DbErr(format!("reading the fork (block {}) failed: {what} from {}: {e}", self.block(), self.url)));
            }
            tries += 1;
            crate::rpc::backoff(tries);
        }
    }
}

const ACCOUNT: [&str; 3] = ["eth_getBalance", "eth_getTransactionCount", "eth_getCode"];

/// An account from [eth_getBalance, eth_getTransactionCount, eth_getCode] results.
fn account(r: &[serde_json::Value]) -> Result<AccountInfo, String> {
    let bc = Bytecode::new_raw(crate::rpc::data(&r[2]).map_err(|e| format!("eth_getCode: {e}"))?.into());
    let balance = crate::rpc::quantity(&r[0]).map_err(|e| format!("eth_getBalance: {e}"))?;
    let nonce = num(&r[1]).map_err(|e| format!("eth_getTransactionCount: {e}"))?;
    Ok(AccountInfo::new(balance, nonce, bc.hash_slow(), bc))
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
        let info = f.q(true, |b| ACCOUNT.map(|m| (m, serde_json::json!([a, b.clone()]))).to_vec(), account)?;
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
        let v = f.q(true, |b| vec![("eth_getStorageAt", serde_json::json!([a, format!("0x{k:x}"), b]))], |r| crate::rpc::quantity(&r[0]).map_err(|e| format!("eth_getStorageAt: {e}")))?;
        f.slots.lock().unwrap().insert((a, k), v);
        Ok(v)
    }
    fn block_hash_ref(&self, n: u64) -> Result<B256, DbErr> {
        if let Some(p) = &self.1 {
            return p.block_hash_ref(n);
        }
        match &self.0 {
            // the chain's own hashes up to the forked block
            Some(f) if n <= f.block() => {
                if let Some(h) = f.hashes.lock().unwrap().get(&n) {
                    return Ok(*h);
                }
                let h = f.q(false, |_| vec![("eth_getBlockByNumber", serde_json::json!([format!("0x{n:x}"), false]))], |r| {
                    let h = crate::rpc::data(&r[0]["hash"]).map_err(|e| format!("block {n}'s hash: {e}"))?;
                    (h.len() == 32).then(|| B256::from_slice(&h)).ok_or_else(|| format!("block {n}'s hash: {} bytes", h.len()))
                })?;
                f.hashes.lock().unwrap().insert(n, h);
                Ok(h)
            }
            // what forge's in-memory backend answers (revm's EmptyDB)
            _ => Ok(keccak256(n.to_string().as_bytes())),
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
    /// the EVM rules: the evm_version the code was compiled for
    pub spec: SpecId,
    /// enforce the contract size limits (EIP-170, EIP-3860), as a chain does: deploys, not tests
    pub strict: bool,
}

impl Env {
    pub fn new(chain_id: u64) -> Env {
        let mut block = BlockEnv::default();
        block.number = U256::from(1);
        block.timestamp = U256::from(1);
        block.gas_limit = GAS_LIMIT;
        block.basefee = 0;
        block.prevrandao = Some(B256::ZERO);
        Env { block, chain_id, gas_price: 0, spec: SpecId::OSAKA, strict: false }
    }
}

/// solc's name for an EVM version to revm's.
pub fn spec(evm_version: &str) -> SpecId {
    match evm_version {
        "homestead" => SpecId::HOMESTEAD,
        "tangerineWhistle" => SpecId::TANGERINE,
        "spuriousDragon" => SpecId::SPURIOUS_DRAGON,
        "byzantium" => SpecId::BYZANTIUM,
        "constantinople" | "petersburg" => SpecId::PETERSBURG,
        "istanbul" => SpecId::ISTANBUL,
        "berlin" => SpecId::BERLIN,
        "london" => SpecId::LONDON,
        "paris" => SpecId::MERGE,
        "shanghai" => SpecId::SHANGHAI,
        "cancun" => SpecId::CANCUN,
        "prague" => SpecId::PRAGUE,
        _ => SpecId::OSAKA,
    }
}

pub struct Exec {
    pub ok: bool,
    pub output: Bytes,
    /// gas used, as a receipt reports it (after refunds)
    pub gas: u64,
    /// gas spent before refunds: what the transaction needs to be given
    pub spent: u64,
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
        let mut ctx: Ctx = Context::new(db, env.spec);
        ctx.block = env.block.clone();
        ctx.cfg.chain_id = env.chain_id;
        ctx.cfg.disable_nonce_check = true;
        ctx.cfg.disable_block_gas_limit = true;
        ctx.cfg.disable_base_fee = true;
        ctx.cfg.tx_gas_limit_cap = Some(u64::MAX);
        if !env.strict {
            ctx.cfg.limit_contract_code_size = Some(usize::MAX);
            ctx.cfg.limit_contract_initcode_size = Some(usize::MAX);
        }
        ctx.cfg.disable_eip3607 = true; // pranked/broadcast senders may be contracts
        Vm { evm: ctx.build_mainnet_with_inspector(cheats), gas_price: env.gas_price }
    }

    pub fn call(&mut self, from: Address, to: TxKind, data: Bytes, value: U256, commit: impl Into<Commit>) -> Exec {
        let commit = commit.into();
        let mut tx = TxEnv::default();
        tx.caller = from;
        // a test's gas is the block's (forge's gas_limit, --gas-limit)
        tx.gas_limit = self.evm.ctx.block.gas_limit;
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
                Exec { ok: true, output, gas: gas.tx_gas_used(), spent: gas.total_gas_spent(), logs, address }
            }
            Ok(ExecutionResult::Revert { gas, logs, output }) => Exec { ok: false, output, gas: gas.tx_gas_used(), spent: gas.total_gas_spent(), logs, address: None },
            Ok(ExecutionResult::Halt { reason, gas, logs }) => {
                Exec { ok: false, output: decode::revert_msg(&format!("EvmError: {reason:?}")), gas: gas.tx_gas_used(), spent: gas.total_gas_spent(), logs, address: None }
            }
            Err(e) => Exec { ok: false, output: decode::revert_msg(&format!("EvmError: {e}")), gas: 0, spent: 0, logs: vec![], address: None },
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
    /// the contract that has to revert, when given
    reverter: Option<Address>,
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
    /// what tests may reach outside the EVM (ffi, fs_permissions)
    pub access: Arc<crate::project::Access>,
    pub console: Vec<String>,
    pub labels: HashMap<Address, String>,
    pub broadcasts: Vec<BroadcastTx>,
    pub keys: HashMap<Address, B256>,
    pub default_broadcaster: Option<Address>,
    pub failed_expectations: Vec<String>,
    /// forks the suite made (vm.createFork, or --fork as fork 0), by id, and the one in use
    pub forks: Vec<Arc<Fork>>,
    pub active_fork: Option<usize>,
    /// accounts that keep their state when another fork is selected: the test contract and libraries (test.rs sets
    /// them, as forge makes what it deploys persistent) and vm.makePersistent's; the cheatcode account, CREATE2
    /// deployer and SENDER always do
    pub persistent: Vec<Address>,
    prank: Option<Prank>,
    origin_restore: Option<(usize, Address)>,
    expect_revert: Option<ExpectRevert>,
    revert_armed: Option<usize>,
    emits: Vec<ExpectEmit>,
    recorded: Option<Vec<Log>>,
    snapshots: Vec<Option<(revm::state::EvmState, BlockEnv)>>,
    broadcast: Option<Broadcast>,
    /// (callee, value if given, calldata prefix, return data, reverts)
    mocks: Vec<(Address, Option<U256>, Vec<u8>, Vec<u8>, bool)>,
    /// (callee, value if given, calldata prefix, calls seen)
    expected_calls: Vec<(Address, Option<U256>, Vec<u8>, usize)>,
    /// the gas of the last call that ended (any depth), for vm.lastCallGas and snapshotGasLastCall
    last_gas: Option<Gas>,
    gas_snapshot: Option<u64>,
    /// vm.pauseGasMetering: the gas each running frame had when metering paused, which its steps keep going back to
    paused: Option<Vec<Gas>>,
    /// vm.resetGasMetering: the running frame's gas goes back to its limit at its next step
    reset_gas: bool,
    /// vm.record: the storage slots each account read and wrote since (an SSTORE reads too, as in forge)
    recording: bool,
    accesses: HashMap<Address, (Vec<U256>, Vec<U256>)>,
    /// vm.serialize*: the objects so far, by key (sorted, as forge's)
    json: HashMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
    /// the solc version the project compiles with, which artifact ids may name ("File.sol:Name:0.8.28")
    pub solc: String,
    /// the compiler profile of the running test: what it deploys by name comes from that profile's build
    pub profile: u32,
    /// forge's --isolate: every call the test contract makes is a transaction of its own (see `isolated`)
    pub isolate: bool,
    /// inside one of those transactions, and the outer transaction's origin, which it keeps
    in_inner: bool,
    inner_origin: Address,
    pub allow_internal_revert: bool,
    cheat_gas: u64,
    /// address -> contract name for contracts created while `track_creates` is on
    pub names: HashMap<Address, String>,
    /// address -> artifact ("path:Name"), where that's certain
    pub ids: HashMap<Address, String>,
    /// the frame a revert came from, and its data: deeper frames bubble the same data up
    revert_src: Option<(Address, Bytes)>,
    /// the last frame that failed without reverting (a limit or an invalid operation), to explain a bare revert
    pub last_halt: Option<InstructionResult>,
    pub track_creates: bool,
    /// deploys: take where contracts land on the fork as empty (`Fork::presume`); tests must see the chain as is
    pub presume_creates: bool,
    /// every contract created while `track_creates` is on, with its init code (for verification)
    pub creates: Vec<(Address, Bytes)>,
    pub trace: Option<Vec<TraceNode>>,
    trace_stack: Vec<usize>,
    bc_creates: Vec<Option<usize>>,
    /// set when ds-test/forge-std `fail()` writes the global "failed" slot
    pub failed: bool,
    /// libraries deployed for the suite ("file:Name" -> address), used to link getCode results
    pub libs: HashMap<String, Address>,
    /// what the suite read from outside the EVM, when its results may be cached (see test.rs)
    pub reads: Option<Arc<Mutex<Reads>>>,
    /// getCode results, linked and decoded, shared by the suite's tests
    code_memo: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

/// Inputs a test read besides its own bytecode, "kind:name" -> hash or value; `impure` when it did something
/// a cached result can't stand for (ffi, writing files, wall-clock time, forks, setEnv).
#[derive(Default)]
pub struct Reads {
    pub deps: std::collections::BTreeMap<String, String>,
    pub impure: bool,
}

/// What `Reads` records for an input, as of now (None: the kind is unknown).
pub fn read_now(key: &str, arts: &[Artifact], root: &std::path::Path) -> Option<String> {
    let (kind, name) = key.split_once(':')?;
    Some(match kind {
        "art" => arts.iter().find(|a| a.shard == 0 && format!("{}:{}", a.file, a.name) == name).map_or("missing".into(), art_hash),
        "file" => match root.join(name) {
            p if p.is_dir() => "dir".into(),
            p => std::fs::read(p).map_or("missing".into(), |b| keccak256(b).to_string()),
        },
        "env" => lookup(name).unwrap_or_else(|| "\0unset".into()),
        "root" => root.to_string_lossy().into(),
        _ => return None,
    })
}

pub fn art_hash(a: &Artifact) -> String {
    keccak256(format!("{}|{}|{}", a.bytecode, a.deployed, a.links).as_bytes()).to_string()
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
    "envUint(string,string)", "envInt(string,string)", "envAddress(string,string)", "envBool(string,string)",
    "envBytes32(string,string)", "envString(string,string)", "envBytes(string,string)", "envOr(string,string,uint256[])",
    "envOr(string,string,int256[])", "envOr(string,string,address[])", "envOr(string,string,bool[])",
    "envOr(string,string,bytes32[])", "envOr(string,string,string[])", "envOr(string,string,bytes[])",
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
    "makePersistent(address,address)", "makePersistent(address,address,address)", "makePersistent(address[])",
    "revokePersistent(address)", "revokePersistent(address[])", "rollFork(uint256)", "rollFork(uint256,uint256)",
    "snapshotGasLastCall(string)", "snapshotGasLastCall(string,string)", "startSnapshotGas(string)", "startSnapshotGas(string,string)",
    "stopSnapshotGas()", "stopSnapshotGas(string)", "stopSnapshotGas(string,string)", "snapshotValue(string,uint256)",
    "snapshotValue(string,string,uint256)", "ffi(string[])", "readFileBinary(string)",
    "parseUint(string)", "parseInt(string)", "parseAddress(string)", "parseBool(string)", "parseBytes32(string)", "parseBytes(string)",
    "cool(address)", "coolSlot(address,bytes32)", "lastCallGas()", "resetGasMetering()", "removeFile(string)", "dumpState(string)",
    "record()", "stopRecord()", "accesses(address)",
    "parseJson(string)", "parseJson(string,string)", "parseJsonUint(string,string)", "parseJsonUintArray(string,string)",
    "parseJsonInt(string,string)", "parseJsonIntArray(string,string)", "parseJsonBool(string,string)", "parseJsonBoolArray(string,string)",
    "parseJsonAddress(string,string)", "parseJsonAddressArray(string,string)", "parseJsonString(string,string)",
    "parseJsonStringArray(string,string)", "parseJsonBytes(string,string)", "parseJsonBytesArray(string,string)",
    "parseJsonBytes32(string,string)", "parseJsonBytes32Array(string,string)", "parseJsonType(string,string)",
    "parseJsonType(string,string,string)", "parseJsonTypeArray(string,string,string)", "parseJsonKeys(string,string)",
    "keyExists(string,string)", "keyExistsJson(string,string)", "serializeJson(string,string)", "serializeBool(string,string,bool)",
    "serializeUint(string,string,uint256)", "serializeInt(string,string,int256)", "serializeAddress(string,string,address)",
    "serializeBytes32(string,string,bytes32)", "serializeString(string,string,string)", "serializeBytes(string,string,bytes)",
    "serializeBool(string,string,bool[])", "serializeUint(string,string,uint256[])", "serializeInt(string,string,int256[])",
    "serializeAddress(string,string,address[])", "serializeBytes32(string,string,bytes32[])", "serializeString(string,string,string[])",
    "serializeBytes(string,string,bytes[])", "serializeUintToHex(string,string,uint256)", "writeJson(string,string)",
    "writeJson(string,string,string)",
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

/// The artifact `what` ("path:Name", "File.sol:Name", "File.sol" or "Name", each optionally followed by
/// ":<solc version>" and a compiler profile, as forge writes them) names: the exact path first, then a path suffix,
/// then the file name. Two matches at the same level are an error, never a guess. Like forge, the profile isn't
/// read: plain compiles each contract once.
fn find_artifact<'a>(arts: &'a [Artifact], what: &str, solc: &str, profile: u32) -> Result<&'a Artifact, String> {
    let mut parts = what.split(':');
    let first = parts.next().unwrap_or_default();
    let (file, name, version) = match first.contains('.') {
        true => match parts.next() {
            Some(v) if v.contains('.') => (first, None, Some(v)),
            n => (first, n, parts.next()),
        },
        false => ("", Some(first), parts.next()),
    };
    if let Some(v) = version.filter(|v| !solc.is_empty() && v != &solc) {
        return Err(format!("no artifact for {what:?}: this project compiles with solc {solc}, not {v}"));
    }
    let fname = file.rsplit('/').next().unwrap_or(file);
    let named: Vec<&Artifact> = arts.iter().filter(|a| a.shard == 0 && !a.bytecode.is_empty() && name.map_or(Some(a.name.as_str()) == fname.strip_suffix(".sol"), |n| a.name == n)).collect();
    let tiers: [&dyn Fn(&Artifact) -> bool; 3] = [&|a| file.is_empty() || a.file == file, &|a| a.file.ends_with(&format!("/{file}")), &|a| a.file.rsplit('/').next() == Some(fname)];
    for tier in tiers {
        let hits: Vec<&&Artifact> = named.iter().filter(|a| tier(a)).collect();
        // a contract compiled with several profiles: the running test's (forge's choice)
        let mine: Vec<&&Artifact> = hits.iter().copied().filter(|a| a.profile == profile).collect();
        match if hits.len() > 1 && !mine.is_empty() { mine } else { hits }.as_slice() {
            [] => continue,
            [a] => return Ok(a),
            many => return Err(format!("{what:?} matches several contracts: {}", many.iter().map(|a| format!("{}:{}", a.file, a.name)).collect::<Vec<_>>().join(", "))),
        }
    }
    Err(format!("no artifact for {what:?} (only project src/test/script contracts are compiled to bytecode)"))
}

/// Every storage slot `a` holds in `db` and the layers under it (a fork's own slots are read when asked for).
fn storage_of(db: &Db, a: Address) -> HashMap<U256, U256> {
    let mut out = db.db.1.as_ref().map(|base| storage_of(base, a)).unwrap_or_default();
    if let Some(acc) = db.cache.accounts.get(&a) {
        if matches!(acc.account_state, revm::database::AccountState::StorageCleared | revm::database::AccountState::NotExisting) {
            out.clear();
        }
        out.extend(acc.storage.iter().map(|(k, v)| (*k, *v)));
    }
    out
}

/// The structs of the sources the artifacts come from, which order the members of objects vm.parseJson decodes.
fn json_defs(arts: &[Artifact], root: &std::path::Path) -> &'static std::collections::BTreeMap<String, Vec<String>> {
    static DEFS: OnceLock<std::collections::BTreeMap<String, Vec<String>>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let files: std::collections::BTreeSet<&str> = arts.iter().map(|a| a.file.as_str()).collect();
        let texts: Vec<String> = files.iter().filter_map(|f| std::fs::read_to_string(root.join(f)).ok()).collect();
        crate::json::struct_defs(texts.iter().map(String::as_str))
    })
}

/// What tests set with vm.setEnv: process-wide like forge's, without changing the real environment, which other
/// threads read at the same time.
static SET_ENV: std::sync::LazyLock<Mutex<HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

fn lookup(k: &str) -> Option<String> {
    SET_ENV.lock().unwrap().get(k).cloned().or_else(|| crate::project::var(k))
}

fn env(k: &str) -> Result<String, String> {
    lookup(k).ok_or_else(|| format!("environment variable {k:?} not found"))
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
    pub fn new(artifacts: Arc<Vec<Artifact>>, root: std::path::PathBuf, access: Arc<crate::project::Access>) -> Cheats {
        Cheats { artifacts, root, access, ..Default::default() }
    }

    /// The next top-level call comes from `who` (msg.sender and tx.origin), though the transaction stays the original
    /// sender's: how a script's run() sees its deployer without spending the deployer's nonce.
    pub fn act_as(&mut self, who: Address) {
        self.prank = Some(Prank { sender: who, origin: Some(who), depth: 0, single: true });
    }

    /// Where a file cheatcode may go: inside a path fs_permissions opens for reading (or writing), with `..` and
    /// symlinks resolved. Nothing is open unless configured, like forge.
    fn fs(&self, path: &str, write: bool) -> Result<std::path::PathBuf, String> {
        let p = crate::project::resolve(&self.root.join(path));
        let open = self.access.fs.iter().any(|(dir, r, w)| (if write { *w } else { *r }) && p.starts_with(dir));
        if write && self.access.protected.iter().any(|x| p.starts_with(x)) {
            return Err(format!("{path:?} can't be written from a test or script (plain's state, git's or the config)"));
        }
        match open {
            true => Ok(p),
            false => Err(format!(
                "{path:?} isn't open for {} from tests and scripts; allow it in foundry.toml or plain.toml, e.g. fs_permissions = [{{ access = \"{}\", path = \"./\" }}]",
                if write { "writing" } else { "reading" },
                if write { "read-write" } else { "read" }
            )),
        }
    }

    pub fn read(&self, key: String) {
        if let Some(r) = &self.reads {
            let mut r = r.lock().unwrap();
            if !r.deps.contains_key(&key) {
                let v = read_now(&key, &self.artifacts, &self.root).unwrap_or_default();
                r.deps.insert(key, v);
            }
        }
    }

    fn impure(&self) {
        if let Some(r) = &self.reads {
            r.lock().unwrap().impure = true;
        }
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
        for (addr, value, data, n) in self.expected_calls.drain(..) {
            if n == 0 {
                let value = value.map(|v| format!(" and value {v}")).unwrap_or_default();
                problems.push(format!("expected call to {addr} with data 0x{}{value} was not made", hex::encode(data)));
            }
        }
        // a vm.prank that no call used ends with its test; a startPrank in setUp lasts into the tests, as in forge
        if self.prank.as_ref().is_some_and(|p| p.single) {
            self.prank = None;
        }
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
                let data = args.iter().find(|v| !matches!(v, DynSolValue::Address(_))).map(by);
                let reverter = args.iter().find_map(|v| v.as_address());
                self.expect_revert = Some(ExpectRevert { data, partial: s.name == "expectPartialRevert", depth, reverter });
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
            "envExists(string)" => {
                self.read(format!("env:{}", st(&args[0])));
                Ok(enc(DynSolValue::Bool(lookup(&st(&args[0])).is_some())))
            }
            "setEnv(string,string)" => {
                self.impure();
                SET_ENV.lock().unwrap().insert(st(&args[0]), st(&args[1]));
                ok
            }
            sg if sg.starts_with("env") => {
                let key = st(&args[0]);
                self.read(format!("env:{key}"));
                let types = match &s.ty {
                    DynSolType::Tuple(t) => t.clone(),
                    _ => vec![],
                };
                let named = match s.name.as_str() {
                    "envUint" => DynSolType::Uint(256),
                    "envInt" => DynSolType::Int(256),
                    "envAddress" => DynSolType::Address,
                    "envBool" => DynSolType::Bool,
                    "envBytes32" => DynSolType::FixedBytes(32),
                    "envBytes" => DynSolType::Bytes,
                    _ => DynSolType::String,
                };
                // (the value's type, the delimiter of an array, the default): envX(name), envX(name, delim),
                // envOr(name, default), envOr(name, delim, default[])
                let (ty, delim, default) = match (s.name.as_str(), &types[..]) {
                    ("envOr", [_, t]) => (t.clone(), None, args.get(1)),
                    ("envOr", [_, _, DynSolType::Array(t)]) => ((**t).clone(), Some(st(&args[1])), args.get(2)),
                    (_, [_, _]) => (named, Some(st(&args[1])), None),
                    _ => (named, None, None),
                };
                let value = env(&key).and_then(|v| match &delim {
                    None => crate::json::parse_value(&v, &ty),
                    // forge's: items trimmed, an empty value an empty array
                    Some(d) => match v.split(d.as_str()).map(str::trim).collect::<Vec<_>>().as_slice() {
                        [] | [""] => Ok(DynSolValue::Array(vec![])),
                        parts => parts.iter().map(|x| crate::json::parse_value(x, &ty)).collect::<Result<Vec<_>, _>>().map(DynSolValue::Array),
                    },
                });
                // envOr: the default when the variable is unset or doesn't parse, as in forge
                match (value, default) {
                    (Ok(v), _) => Ok(enc(v)),
                    (Err(_), Some(d)) => Ok(enc(d.clone())),
                    (Err(e), None) => Err(e),
                }
            }
            sg if sg.starts_with("parseJson") || sg.starts_with("keyExists") => {
                let json: serde_json::Value = serde_json::from_str(&st(&args[0])).map_err(|e| format!("failed parsing JSON: {e}"))?;
                // the key: none for parseJson(json) and parseJsonType(json, type), the second argument otherwise
                let path = if args.len() == 1 || s.sig == "parseJsonType(string,string)" { "$".into() } else { st(&args[1]) };
                let found = crate::json::select(&json, &path)?;
                let ty = |t: &str| DynSolType::parse(t).map_err(|e| format!("{t}: {e}"));
                let want = match s.name.as_str() {
                    "keyExists" | "keyExistsJson" => return Ok(enc(DynSolValue::Bool(!found.is_empty()))),
                    "parseJsonKeys" => {
                        let [serde_json::Value::Object(o)] = found.as_slice() else { return Err(format!("key {path:?} must return exactly one JSON object")) };
                        return Ok(enc(DynSolValue::Array(o.keys().map(|k| DynSolValue::String(k.clone())).collect())));
                    }
                    "parseJson" => {
                        let defs = json_defs(&self.artifacts, &self.root);
                        let values = found.iter().map(|v| crate::json::token(v, defs)).collect::<Result<Vec<_>, _>>()?;
                        return Ok(enc(DynSolValue::Bytes(crate::json::encode(values))));
                    }
                    "parseJsonType" if args.len() == 2 => ty(&st(&args[1]))?,
                    "parseJsonType" => ty(&st(&args[2]))?,
                    "parseJsonTypeArray" => DynSolType::Array(Box::new(ty(&st(&args[2]))?)),
                    n => {
                        let base = n.trim_start_matches("parseJson").trim_end_matches("Array");
                        let t = match base {
                            "Uint" => DynSolType::Uint(256),
                            "Int" => DynSolType::Int(256),
                            "Bool" => DynSolType::Bool,
                            "Address" => DynSolType::Address,
                            "String" => DynSolType::String,
                            "Bytes" => DynSolType::Bytes,
                            _ => DynSolType::FixedBytes(32),
                        };
                        if n.ends_with("Array") { DynSolType::Array(Box::new(t)) } else { t }
                    }
                };
                let [v] = found.as_slice() else { return Err(format!("path {path:?} must return exactly one JSON value")) };
                let value = crate::json::coerce(v, &want)?.abi_encode();
                // the typed getters return the value itself; parseJsonType returns it ABI-encoded, as bytes
                Ok(if s.name.starts_with("parseJsonType") { enc(DynSolValue::Bytes(value)) } else { value })
            }
            sg if sg.starts_with("serialize") => {
                let obj = st(&args[0]);
                if s.name == "serializeJson" {
                    let v: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&st(&args[1])).map_err(|e| format!("failed parsing JSON: {e}"))?;
                    self.json.insert(obj, v.into_iter().collect());
                    return Ok(enc(args[1].clone()));
                }
                let value = match s.name.as_str() {
                    "serializeUintToHex" => serde_json::Value::String(format!("0x{:x}", u(&args[2]))),
                    _ => crate::json::to_json(args[2].clone()),
                };
                let map = self.json.entry(obj).or_default();
                map.insert(st(&args[1]), value);
                Ok(enc(DynSolValue::String(serde_json::to_string(map).unwrap())))
            }
            "writeJson(string,string)" | "writeJson(string,string,string)" => {
                let path = st(&args[1]);
                let data = match args.get(2) {
                    None => serde_json::from_str(&st(&args[0])).unwrap_or_else(|_| serde_json::Value::String(st(&args[0]))),
                    Some(key) => {
                        // the file's JSON with the value put in at the key; a file that isn't there yet starts empty
                        let p = self.fs(&path, false)?;
                        let mut data = match std::fs::read_to_string(&p) {
                            Ok(t) => serde_json::from_str(&t).unwrap_or(serde_json::Value::String(t)),
                            Err(_) => serde_json::json!({}),
                        };
                        crate::json::upsert(&mut data, &st(&args[0]), &st(key))?;
                        data
                    }
                };
                let p = self.fs(&path, true)?;
                self.impure();
                std::fs::write(&p, serde_json::to_string_pretty(&data).unwrap()).map_err(|e| format!("{}: {e}", p.display()))?;
                ok
            }
            "removeFile(string)" => {
                let p = self.fs(&st(&args[0]), true)?;
                self.impure();
                std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                ok
            }
            "dumpState(string)" => {
                // what this transaction has loaded, as genesis allocs; not the system accounts, nor empty ones (forge's)
                let skip = [CHEATS, SENDER, CONSOLE, TEST_CONTRACT, caller];
                let alloc: std::collections::BTreeMap<String, serde_json::Value> = j
                    .inner
                    .state
                    .iter()
                    .filter(|(a, acc)| !skip.contains(a) && !acc.is_empty())
                    .map(|(a, acc)| {
                        let code = acc.info.code.as_ref().map(|c| c.original_bytes()).unwrap_or_default();
                        let mut slots: Vec<(U256, U256)> = acc.storage.iter().map(|(k, v)| (*k, v.present_value())).collect();
                        slots.sort();
                        let storage: serde_json::Map<String, serde_json::Value> = slots.into_iter().map(|(k, v)| (B256::from(k).to_string(), B256::from(v).to_string().into())).collect();
                        (a.to_string().to_lowercase(), serde_json::json!({"nonce": format!("0x{:x}", acc.info.nonce), "balance": format!("0x{:x}", acc.info.balance), "code": format!("0x{}", hex::encode(&code)), "storage": storage}))
                    })
                    .collect();
                let p = self.fs(&st(&args[0]), true)?;
                self.impure();
                std::fs::write(&p, serde_json::to_string(&alloc).unwrap()).map_err(|e| format!("{}: {e}", p.display()))?;
                ok
            }
            "cool(address)" | "coolSlot(address,bytes32)" => {
                // forge's: storage cold again (and the account untouched), as if a new transaction read it
                if let Some(acc) = j.inner.state.get_mut(&a(&args[0])) {
                    acc.unmark_touch();
                    match args.get(1) {
                        Some(k) => acc.storage.get_mut(&b32(k).into()).into_iter().for_each(|v| v.mark_cold()),
                        None => acc.storage.values_mut().for_each(|v| v.mark_cold()),
                    }
                }
                ok
            }
            "record()" => {
                self.recording = true;
                self.accesses.clear();
                ok
            }
            "stopRecord()" => {
                self.recording = false;
                ok
            }
            "accesses(address)" => {
                let (reads, writes) = self.accesses.get(&a(&args[0])).cloned().unwrap_or_default();
                let words = |v: Vec<U256>| DynSolValue::Array(v.into_iter().map(|s| DynSolValue::FixedBytes(s.into(), 32)).collect());
                Ok(DynSolValue::Tuple(vec![words(reads), words(writes)]).abi_encode_params())
            }
            "lastCallGas()" => {
                let g = self.last_gas.ok_or("no external call was made yet")?;
                let w = |n: u64| DynSolValue::Uint(U256::from(n), 64);
                Ok(DynSolValue::Tuple(vec![w(g.limit()), w(g.total_gas_spent()), w(0), DynSolValue::Int(I256::try_from(g.refunded()).unwrap_or_default(), 64), w(g.remaining())]).abi_encode())
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
                Ok(enc(crate::json::parse_value(&st(&args[0]), &ty)?))
            }
            sg if sg.starts_with("toString(") => Ok(enc(DynSolValue::String(decode::display(&args[0])))),
            "getCode(string)" | "getDeployedCode(string)" => {
                let what = st(&args[0]);
                let art = find_artifact(&self.artifacts, &what, &self.solc, self.profile)?;
                self.read(format!("art:{}:{}", art.file, art.name));
                if s.name == "getCode" {
                    // tests deploy the same contracts run after run: link and decode each once per suite
                    let id = format!("{}:{}#{}", art.file, art.name, art.profile);
                    if let Some(c) = self.code_memo.lock().unwrap().get(&id) {
                        return Ok(enc(DynSolValue::Bytes(c.clone())));
                    }
                    let code = link_code(&art.bytecode, &art.links, &self.libs).map_err(|e| format!("getCode {what}: {e}"))?;
                    self.code_memo.lock().unwrap().insert(id, code.clone());
                    return Ok(enc(DynSolValue::Bytes(code)));
                }
                // libraries by their placeholders, "__$" + keccak256("file:Name")[..17] + "$__", as solc writes them
                let mut code = art.deployed.clone();
                for (id, addr) in &self.libs {
                    code = code.replace(&format!("__${}$__", &hex::encode(keccak256(id.as_bytes()))[..34]), &hex::encode(addr));
                }
                if code.contains("__$") {
                    return Err(format!("{what} links a library that isn't deployed"));
                }
                Ok(enc(DynSolValue::Bytes(hex::decode(&code).map_err(|e| format!("{what}: {e}"))?)))
            }
            "mockCall(address,bytes,bytes)" | "mockCallRevert(address,bytes,bytes)" => {
                self.mocks.push((a(&args[0]), None, by(&args[1]), by(&args[2]), s.name == "mockCallRevert"));
                ok
            }
            "mockCall(address,uint256,bytes,bytes)" => {
                self.mocks.push((a(&args[0]), Some(u(&args[1])), by(&args[2]), by(&args[3]), false));
                ok
            }
            "clearMockedCalls()" => {
                self.mocks.clear();
                ok
            }
            "expectCall(address,bytes)" | "expectCall(address,uint256,bytes)" => {
                let value = (args.len() == 3).then(|| u(&args[1]));
                self.expected_calls.push((a(&args[0]), value, by(args.last().unwrap()), 0));
                ok
            }
            "pauseGasMetering()" => {
                // the frames running now keep their gas from here (see `step`)
                self.paused.get_or_insert_default();
                ok
            }
            "resumeGasMetering()" => {
                self.paused = None;
                ok
            }
            "resetGasMetering()" => {
                self.reset_gas = true;
                ok
            }
            "allowCheatcodes(address)" => ok,
            sg if sg.starts_with("makePersistent") || sg.starts_with("revokePersistent") => {
                let list: Vec<Address> = args.iter().flat_map(|v| v.as_array().map_or_else(|| vec![a(v)], |l| l.iter().map(a).collect())).collect();
                match s.name.as_str() {
                    "makePersistent" => self.persistent.extend(list),
                    _ => self.persistent.retain(|x| !list.contains(x)),
                }
                ok
            }
            "isPersistent(address)" => Ok(enc(DynSolValue::Bool([CHEATS, CREATE2_DEPLOYER, SENDER].contains(&a(&args[0])) || self.persistent.contains(&a(&args[0]))))),
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
            "projectRoot()" => {
                self.read("root:".into());
                Ok(enc(DynSolValue::String(self.root.to_string_lossy().into())))
            }
            "readFile(string)" => {
                let p = self.fs(&st(&args[0]), false)?;
                self.read(format!("file:{}", st(&args[0])));
                Ok(enc(DynSolValue::String(std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)))
            }
            "writeFile(string,string)" => {
                let p = self.fs(&st(&args[0]), true)?;
                self.impure();
                std::fs::write(&p, st(&args[1])).map_err(|e| format!("{}: {e}", p.display()))?;
                ok
            }
            "exists(string)" => {
                let p = self.fs(&st(&args[0]), false)?;
                self.read(format!("file:{}", st(&args[0])));
                Ok(enc(DynSolValue::Bool(p.exists())))
            }
            "unixTime()" => {
                self.impure();
                Ok(enc(DynSolValue::Uint(U256::from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()), 256)))
            }
            "rpcUrl(string)" => {
                self.impure();
                let p = crate::project::Project::load();
                let name = st(&args[0]);
                Ok(enc(DynSolValue::String(crate::rpc::network_url(&p, &name).ok_or(format!("unknown network {name:?}"))?)))
            }
            "activeFork()" => Ok(enc(DynSolValue::Uint(U256::from(self.active_fork.ok_or("no active fork")?), 256))),
            "createSelectFork(string)" | "createSelectFork(string,uint256)" | "createFork(string)" | "createFork(string,uint256)" => {
                self.impure();
                let name = st(&args[0]);
                let url = if name.contains("://") { name } else { crate::rpc::network_url(&crate::project::Project::load(), &name).ok_or(format!("unknown network or RPC {name:?}"))? };
                let fork = Fork::at(&url, args.get(1).map(|b| u(b).saturating_to()))?;
                self.forks.push(fork);
                let id = self.forks.len() - 1;
                if s.name == "createSelectFork" {
                    self.select_fork(ctx, id)?;
                }
                Ok(enc(DynSolValue::Uint(U256::from(id), 256)))
            }
            "selectFork(uint256)" => {
                self.impure();
                self.select_fork(ctx, u(&args[0]).saturating_to())?;
                ok
            }
            "rollFork(uint256)" | "rollFork(uint256,uint256)" => {
                // ponytail: rolling forks the RPC again at the block (persistent accounts kept); forge rolls in place
                self.impure();
                let id = if args.len() == 2 { u(&args[0]).saturating_to() } else { self.active_fork.ok_or("no active fork")? };
                let fork = Fork::at(&self.forks.get(id).ok_or(format!("no fork {id}"))?.url, Some(u(args.last().unwrap()).saturating_to()))?;
                self.forks[id] = fork;
                self.select_fork(ctx, id)?;
                ok
            }
            sg if sg.starts_with("snapshotGasLastCall") => Ok(enc(DynSolValue::Uint(U256::from(self.last_gas.map_or(0, |g| g.total_gas_spent())), 256))),
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
                if !self.access.ffi {
                    return Err("vm.ffi runs commands on this machine, so it's off unless the project enables it: ffi = true in foundry.toml or plain.toml".into());
                }
                self.impure();
                let argv: Vec<String> = args[0].as_array().unwrap_or_default().iter().map(st).collect();
                let out = std::process::Command::new(argv.first().ok_or("ffi: empty command")?)
                    .args(&argv[1..])
                    .current_dir(&self.root)
                    // the environment forge would give it: .env and vm.setEnv included
                    .envs(crate::project::dotenv())
                    .envs(SET_ENV.lock().unwrap().clone())
                    .output()
                    .map_err(|e| format!("ffi {}: {e}", argv[0]))?;
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let bytes = hex::decode(text.trim_start_matches("0x")).unwrap_or_else(|_| text.into_bytes());
                Ok(enc(DynSolValue::Bytes(bytes)))
            }
            "readFileBinary(string)" => {
                let p = self.fs(&st(&args[0]), false)?;
                self.read(format!("file:{}", st(&args[0])));
                Ok(enc(DynSolValue::Bytes(std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?)))
            }
            other => Err(format!("plain: unsupported cheatcode vm.{other}")),
        }
    }
}

fn matches_revert(exp: &ExpectRevert, out: &[u8]) -> bool {
    let Some(d) = &exp.data else { return true };
    // a bare selector matches only an error without arguments; expectPartialRevert matches any (forge 1.0)
    if out == d.as_slice() || (exp.partial && out.starts_with(d)) {
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
        // any failure counts, as in forge: a revert, a halt, or one before any code ran (no balance for the value)
        let from = self.revert_src.take().map(|(a, _)| a);
        if result.result.is_ok() {
            result.result = InstructionResult::Revert;
            result.output = decode::revert_msg("call did not revert as expected");
        } else if let Some(want) = exp.reverter.filter(|w| from != Some(*w)) {
            result.output = decode::revert_msg(&format!("reverted in {}, expected the revert from {want}", from.map_or("?".into(), |a| a.to_string())));
            result.result = InstructionResult::Revert;
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
        } else if missing {
            // a reverted call's logs are gone: the expected one wasn't emitted
            self.failed_expectations.push("expected an emit, but the call reverted".into());
        }
    }

    /// Shared pre-call logic for calls and creates made by a frame at `depth`.
    fn before(&mut self, ctx: &mut Ctx, depth: usize, caller: &mut Address, delegate: bool, view: bool) -> Option<BroadcastTx> {
        let mut bc = None;
        if let Some(b) = self.broadcast.clone() {
            if b.depth == depth && !delegate && !view {
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
            self.revert_src = None;
        }
        bc
    }

    /// forge's --isolate: a call or create the test contract makes runs as a transaction of its own, on the state so
    /// far with every account and slot cold again (each slot's original value its current one) and no transient
    /// storage. The frame making it pays what the transaction used: 21000 and calldata included, refunds taken off.
    fn isolated(&mut self, ctx: &mut Ctx, caller: Address, kind: TxKind, data: Bytes, value: U256, gas_limit: u64) -> (InterpreterResult, Option<Address>) {
        let mut state = ctx.journaled_state.inner.state.clone();
        for (a, acc) in state.iter_mut() {
            if ctx.journaled_state.inner.warm_addresses.is_cold(a) {
                acc.mark_cold();
            }
            for v in acc.storage.values_mut() {
                v.is_cold = true;
                v.original_value = v.present_value;
            }
        }
        let db = std::mem::replace(&mut ctx.journaled_state.database, Db::new(Remote::default()));
        let mut inner: Ctx = Context::new(db, ctx.cfg.spec);
        inner.cfg = ctx.cfg.clone();
        inner.block = ctx.block.clone();
        inner.block.basefee = 0;
        inner.journaled_state.inner.state = state;
        // the inner frames run at the depths they'd have in place, so cheatcode depths still match
        inner.journaled_state.inner.depth = 1;
        let mut tx = TxEnv::default();
        tx.caller = caller;
        tx.kind = kind;
        tx.data = data;
        tx.value = value;
        tx.gas_price = 0;
        tx.chain_id = Some(ctx.cfg.chain_id);
        tx.gas_limit = (gas_limit + 21_000).min(ctx.block.gas_limit);
        let origin = std::mem::replace(&mut self.inner_origin, ctx.tx.caller);
        self.in_inner = true;
        let mut evm = inner.build_mainnet_with_inspector(&mut *self);
        let res = evm.inspect_tx(tx);
        ctx.journaled_state.database = std::mem::replace(&mut evm.ctx.journaled_state.database, Db::new(Remote::default()));
        // cheatcodes inside it (warp, roll, ...) still count
        let basefee = ctx.block.basefee;
        ctx.block = evm.ctx.block.clone();
        ctx.block.basefee = basefee;
        ctx.cfg.chain_id = evm.ctx.cfg.chain_id;
        drop(evm);
        self.in_inner = false;
        self.inner_origin = origin;
        let mut gas = Gas::new(gas_limit);
        // a transaction the chain wouldn't take (no balance for the value, gas under the intrinsic cost) reverts empty
        let Ok(res) = res else { return (InterpreterResult { result: InstructionResult::Revert, output: Bytes::new(), gas }, None) };
        let outer = &mut ctx.journaled_state.inner.state;
        for (a, mut acc) in res.state {
            let Some(o) = outer.get_mut(&a) else {
                outer.insert(a, acc);
                continue;
            };
            // what was warm before stays warm
            if acc.status.contains(revm::state::AccountStatus::Cold) && !o.status.contains(revm::state::AccountStatus::Cold) {
                acc.status -= revm::state::AccountStatus::Cold;
            }
            o.info = acc.info;
            o.status |= acc.status;
            for (k, v) in acc.storage {
                match o.storage.get_mut(&k) {
                    Some(slot) => {
                        slot.present_value = v.present_value;
                        slot.is_cold &= v.is_cold;
                    }
                    None => {
                        o.storage.insert(k, v);
                    }
                }
            }
        }
        match res.result {
            ExecutionResult::Success { reason, gas: g, output, .. } => {
                gas.set_refund(g.final_refunded() as i64);
                let _ = gas.record_regular_cost(g.tx_gas_used());
                let (out, address) = match output {
                    Output::Call(o) => (o, None),
                    Output::Create(o, a) => (o, a),
                };
                (InterpreterResult { result: reason.into(), output: out, gas }, address)
            }
            ExecutionResult::Revert { gas: g, output, .. } => {
                let _ = gas.record_regular_cost(g.tx_gas_used());
                (InterpreterResult { result: InstructionResult::Revert, output, gas }, None)
            }
            ExecutionResult::Halt { reason, gas: g, .. } => {
                let _ = gas.record_regular_cost(g.tx_gas_used());
                (InterpreterResult { result: reason.into(), output: Bytes::new(), gas }, None)
            }
        }
    }

    /// Makes fork `id` the chain the rest of the run sees: its state and its block (number, time, fees, chain id; the
    /// test's gas stays). Persistent accounts move over with all they hold; everything else is the fork's.
    /// ponytail: a fork selected again starts over from the chain (forge keeps what was done on it)
    fn select_fork(&mut self, ctx: &mut Ctx, id: usize) -> Result<(), String> {
        let fork = self.forks.get(id).cloned().ok_or(format!("no fork {id}"))?;
        let env = fork.env().map_err(|e| e.0)?;
        let keep: std::collections::HashSet<Address> = [CHEATS, CREATE2_DEPLOYER, SENDER].into_iter().chain(self.persistent.iter().copied()).collect();
        let old = std::mem::replace(&mut ctx.journaled_state.database, Db::new(Remote(Some(fork), None)));
        let db = &mut ctx.journaled_state.database;
        for a in &keep {
            let Some(mut info) = old.basic_ref(*a).map_err(|e| e.0)? else { continue };
            if info.code.is_none() && info.code_hash != revm::primitives::KECCAK_EMPTY {
                info.code = Some(old.code_by_hash_ref(info.code_hash).map_err(|e| e.0)?);
            }
            db.insert_account_info(*a, info);
            for (k, v) in storage_of(&old, *a) {
                db.insert_account_storage(*a, k, v).map_err(|e| e.0)?;
            }
        }
        // what this transaction loaded: only persistent accounts stay, and what was done to the others can't be
        // reverted any more (their journal entries become no-ops)
        let inner = &mut ctx.journaled_state.inner;
        inner.state.retain(|a, _| keep.contains(a));
        inner.selfdestructed_addresses.retain(|a| keep.contains(a));
        use revm::context_interface::journaled_state::entry::JournalEntry as E;
        for e in inner.journal.iter_mut() {
            let addrs: Vec<Address> = match e {
                E::AccountWarmed { address } | E::AccountTouched { address } | E::BalanceChange { address, .. } | E::NonceChange { address, .. } | E::NonceBump { address } | E::AccountCreated { address, .. } | E::StorageChanged { address, .. } | E::StorageWarmed { address, .. } | E::CodeChange { address, .. } => vec![*address],
                E::AccountDestroyed { address, target, .. } => vec![*address, *target],
                E::BalanceTransfer { from, to, .. } => vec![*from, *to],
                E::TransientStorageChange { .. } => vec![],
                #[allow(unreachable_patterns)]
                _ => vec![Address::ZERO, Address::repeat_byte(0xff)],
            };
            if !addrs.iter().all(|a| keep.contains(a)) {
                *e = E::TransientStorageChange { key: U256::ZERO, had_value: U256::ZERO, address: Address::ZERO };
            }
        }
        ctx.block.number = env.block.number;
        ctx.block.timestamp = env.block.timestamp;
        ctx.block.basefee = env.block.basefee;
        ctx.block.beneficiary = env.block.beneficiary;
        ctx.block.prevrandao = env.block.prevrandao;
        ctx.cfg.chain_id = env.chain_id;
        ctx.tx.chain_id = Some(env.chain_id);
        self.active_fork = Some(id);
        Ok(())
    }

    /// Notes where a revert started: the innermost frame that failed with these bytes.
    fn note_revert(&mut self, addr: Address, result: &InterpreterResult) {
        if !result.result.is_ok() && self.revert_src.as_ref().is_none_or(|(_, o)| *o != result.output) {
            self.revert_src = Some((addr, result.output.clone()));
        }
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
    fn initialize_interp(&mut self, interp: &mut revm::interpreter::Interpreter, _: &mut Ctx) {
        if let Some(frames) = &mut self.paused {
            frames.push(interp.gas);
        }
    }

    fn step(&mut self, interp: &mut revm::interpreter::Interpreter, _: &mut Ctx) {
        // vm.pauseGasMetering: every step goes back to the frame's gas from when it paused (memory still grows)
        if let Some(frames) = &mut self.paused {
            match frames.last() {
                Some(g) => {
                    let memory = *interp.gas.memory();
                    interp.gas = *g;
                    *interp.gas.memory_mut() = memory;
                }
                None => frames.push(interp.gas),
            }
        }
        if self.recording {
            let op = revm::interpreter::interpreter_types::Jumps::opcode(&interp.bytecode);
            if op == 0x54 || op == 0x55 {
                if let Ok(slot) = interp.stack.peek(0) {
                    let (reads, writes) = self.accesses.entry(interp.input.target_address).or_default();
                    reads.push(slot);
                    if op == 0x55 {
                        writes.push(slot);
                    }
                }
            }
        }
        if self.reset_gas {
            let memory = *interp.gas.memory();
            interp.gas = Gas::new(interp.gas.limit());
            *interp.gas.memory_mut() = memory;
            self.reset_gas = false;
        }
    }

    fn step_end(&mut self, interp: &mut revm::interpreter::Interpreter, _: &mut Ctx) {
        if let Some(frames) = &mut self.paused {
            if interp.bytecode.action.as_ref().is_some_and(|a| a.is_return()) {
                frames.pop();
            }
        }
    }

    fn call(&mut self, ctx: &mut Ctx, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let depth = ctx.journaled_state.depth();
        if self.in_inner && depth == 1 {
            // the top frame of an isolated transaction: everything for this call was done in the outer one
            ctx.tx.caller = self.inner_origin;
            return None;
        }
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
            let value = inputs.call_value();
            for (addr, v, d, n) in self.expected_calls.iter_mut() {
                if *addr == inputs.target_address && v.is_none_or(|v| v == value) && data.starts_with(d) {
                    *n += 1;
                }
            }
            if let Some((_, _, _, ret, rev)) = self.mocks.iter().rev().find(|(addr, v, d, _, _)| *addr == inputs.target_address && v.is_none_or(|v| v == value) && data.starts_with(d)) {
                let out = outcome(!rev, ret.clone(), inputs);
                if self.expect_revert.as_ref().is_some_and(|e| e.depth == depth) && self.revert_armed.is_none() {
                    self.revert_armed = Some(depth);
                    self.revert_src = None;
                }
                if let Some(t) = &mut self.trace {
                    self.trace_stack.push(t.len()); // call_end still runs and pops it
                    t.push(TraceNode { depth, to: inputs.target_address, input: data.clone(), value: inputs.call_value(), create: false, result: None });
                }
                return Some(out);
            }
        }
        let delegate = inputs.scheme.is_delegate_call() || inputs.scheme.is_call_code();
        // a view call (STATICCALL) from a broadcast block reads on chain; forge doesn't send it either
        let view = inputs.scheme == revm::interpreter::interpreter_action::CallScheme::StaticCall;
        let mut caller = inputs.caller;
        if let Some(mut bc) = self.before(ctx, depth, &mut caller, delegate, view) {
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
        if self.isolate && depth == 1 {
            use revm::interpreter::interpreter_action::CallScheme;
            match inputs.scheme {
                CallScheme::Call => {
                    let data = inputs.input.bytes(ctx);
                    let (result, _) = self.isolated(ctx, caller, TxKind::Call(inputs.target_address), data, inputs.call_value(), inputs.gas_limit);
                    return Some(CallOutcome::new(result, inputs.return_memory_offset.clone()));
                }
                // a view call starts cold too, though it runs in place (forge's)
                CallScheme::StaticCall => {
                    let inner = &mut ctx.journaled_state.inner;
                    for (a, acc) in inner.state.iter_mut() {
                        if inner.warm_addresses.is_cold(a) {
                            acc.mark_cold();
                        }
                        acc.storage.values_mut().for_each(|v| v.mark_cold());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn call_end(&mut self, ctx: &mut Ctx, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let depth = ctx.journaled_state.depth();
        if inputs.target_address == CHEATS || (self.in_inner && depth == 1) {
            return;
        }
        self.last_gas = Some(outcome.result.gas);
        if inputs.target_address == CONSOLE {
            return;
        }
        self.after(ctx, depth);
        if self.revert_armed.is_some() {
            self.note_revert(inputs.target_address, &outcome.result);
        }
        if !outcome.result.result.is_ok() && outcome.result.result != InstructionResult::Revert {
            self.last_halt = Some(outcome.result.result);
        }
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
        if self.in_inner && depth == 1 {
            ctx.tx.caller = self.inner_origin;
            return None;
        }
        let mut caller = inputs.caller();
        if let Some(mut bc) = self.before(ctx, depth, &mut caller, false, false) {
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
        // deploys: where the contract lands is taken as empty (the caller settles the fork in one batch)
        if let Some(f) = ctx.journaled_state.database.db.0.clone().filter(|_| self.presume_creates) {
            let addr = match inputs.scheme() {
                revm::context_interface::CreateScheme::Create => ctx.journaled_state.load_account(caller).ok().map(|a| caller.create(a.info.nonce)),
                revm::context_interface::CreateScheme::Create2 { salt } => Some(caller.create2(salt.to_be_bytes::<32>(), keccak256(inputs.init_code()))),
                _ => None,
            };
            if let Some(a) = addr {
                f.presume(a);
            }
        }
        if let Some(t) = &mut self.trace {
            self.trace_stack.push(t.len());
            t.push(TraceNode { depth, to: Address::ZERO, input: inputs.init_code().clone(), value: inputs.value(), create: true, result: None });
        }
        if self.isolate && depth == 1 && !matches!(inputs.scheme(), revm::context_interface::CreateScheme::Create2 { .. }) {
            let (result, address) = self.isolated(ctx, caller, TxKind::Create, inputs.init_code().clone(), inputs.value(), inputs.gas_limit());
            return Some(CreateOutcome::new(result, address));
        }
        None
    }

    fn create_end(&mut self, ctx: &mut Ctx, inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        let depth = ctx.journaled_state.depth();
        if self.in_inner && depth == 1 {
            return;
        }
        self.after(ctx, depth);
        if self.revert_armed.is_some() {
            self.note_revert(outcome.address.unwrap_or_default(), &outcome.result);
        }
        if !outcome.result.result.is_ok() && outcome.result.result != InstructionResult::Revert {
            self.last_halt = Some(outcome.result.result);
        }
        if let Some(Some(i)) = self.bc_creates.pop() {
            self.broadcasts[i].created = outcome.address;
        }
        if self.track_creates && outcome.result.result.is_ok() {
            if let Some(a) = outcome.address {
                self.creates.push((a, inputs.init_code().clone()));
                if let Some(n) = decode::contract_name(inputs.init_code(), &outcome.result.output) {
                    self.names.insert(a, n);
                }
                if let Some(id) = decode::contract_id(inputs.init_code(), &outcome.result.output) {
                    self.ids.insert(a, id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::mock;
    use serde_json::{Value, json};

    #[test]
    fn fork_reads_never_guess() {
        // a slot answered null, then "0x", then for real: the read gets the real value, never a zero
        let url = mock::results(|_, _, n| match n {
            0 => Value::Null,
            1 => json!("0x"),
            _ => json!("0x2a"),
        });
        let remote = Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None);
        assert_eq!(remote.storage_ref(Address::ZERO, U256::ZERO).unwrap(), U256::from(42));
        // rate limited as mainnet.base.org answers it (HTTP 429, no JSON-RPC envelope), then served: the real value
        let url = mock::serve(|req, n| match n {
            0 | 1 => (429, r#"{"code":-32007,"message":"25/second request limit reached"}"#.into()),
            _ => (200, json!({"jsonrpc": "2.0", "id": req["id"], "result": "0x2a"}).to_string()),
        });
        let remote = Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None);
        assert_eq!(remote.storage_ref(Address::ZERO, U256::ZERO).unwrap(), U256::from(42));
        let url = mock::serve(|_, _| (429, r#"{"code":-32007,"message":"25/second request limit reached"}"#.into()));
        let e = Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None).storage_ref(Address::ZERO, U256::ZERO).unwrap_err();
        assert!(e.0.contains("HTTP 429") && e.0.contains("request limit"), "{e}");
        // a node that keeps answering oddly fails the read: never an empty account or a zero slot
        for odd in [Value::Null, json!("0x"), json!("nope")] {
            let url = mock::results(move |_, _, _| odd.clone());
            let remote = Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None);
            assert!(remote.storage_ref(Address::ZERO, U256::ZERO).is_err());
            assert!(remote.basic_ref(Address::ZERO).is_err());
        }
        // "0x" is empty code, though not a balance or a nonce; reads ask for the fork's block
        let url = mock::results(|m, params, _| {
            assert_eq!(params.as_array().unwrap().last().unwrap(), "0x7", "{m} {params}");
            json!(if m == "eth_getCode" { "0x" } else { "0x5" })
        });
        let info = Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None).basic_ref(Address::ZERO).unwrap().unwrap();
        assert_eq!((info.balance, info.nonce, info.code_hash), (U256::from(5), 5, revm::primitives::KECCAK_EMPTY));
        let url = mock::results(|m, _, _| json!(if m == "eth_getBalance" { "0x" } else { "0x5" }));
        assert!(Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None).basic_ref(Address::ZERO).is_err());
    }

    #[test]
    fn artifact_ids() {
        let art = |file: &str, name: &str| Artifact { name: name.into(), file: file.into(), abi: json!([]), bytecode: "00".into(), deployed: "00".into(), links: Value::Null, deployed_links: Value::Null, immutables: Value::Null, shard: 0, tests: vec![], deploys: vec![], rewritten: false, profile: 0 };
        let arts = [art("src/Game.sol", "Game"), art("src/Game.sol", "Lib"), art("src/v2/Game.sol", "Game")];
        let find = |what: &str| find_artifact(&arts, what, "0.8.28", 0).map(|a| format!("{}:{}", a.file, a.name));
        // forge's ids: a version and a profile may follow; the profile isn't read
        assert_eq!(find("src/Game.sol:Game:0.8.28:default").unwrap(), "src/Game.sol:Game");
        assert_eq!(find("src/Game.sol:0.8.28").unwrap(), "src/Game.sol:Game");
        assert_eq!(find("Lib:0.8.28").unwrap(), "src/Game.sol:Lib");
        assert_eq!(find("v2/Game.sol:Game:0.8.28:tests").unwrap(), "src/v2/Game.sol:Game");
        assert!(find("Game.sol:Game:0.8.28:default").unwrap_err().contains("several"));
        assert!(find("src/Game.sol:Game:0.8.27").unwrap_err().contains("0.8.27"));
    }

    #[test]
    fn block_hashes() {
        // forge's in-memory backend hashes the decimal block number; a fork has the chain's own hashes
        assert_eq!(Remote::default().block_hash_ref(5).unwrap(), keccak256(b"5"));
        let h = B256::repeat_byte(0xab);
        let url = mock::results(move |m, params, _| {
            assert_eq!((m, &params[0]), ("eth_getBlockByNumber", &json!("0x5")));
            json!({"hash": h})
        });
        let remote = Remote(Some(Arc::new(Fork::new(&url, Some(7)))), None);
        assert_eq!(remote.block_hash_ref(5).unwrap(), h);
        assert_eq!(remote.block_hash_ref(9).unwrap(), keccak256(b"9"), "past the forked block");
    }
}
