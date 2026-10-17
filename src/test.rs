//! Solidity test runner (forge-std compatible): parallel suites, parallel tests, fuzzing.
use crate::decode;
use crate::evm::{self, ASSUME_MAGIC, CHEATS, Cheats, Db, Env, Exec, SENDER, SKIP_MAGIC};
use crate::solc::Artifact;
use alloy_dyn_abi::{DynSolType, DynSolValue, JsonAbiExt, Specifier};
use alloy_json_abi::{Function, JsonAbi};
use alloy_primitives::{Address, B256, Bytes, I256, U256};
use rayon::prelude::*;
use revm::database_interface::DatabaseRef;
use revm::primitives::TxKind;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub struct Opts {
    pub filter: Option<String>,
    pub verbose: bool,
    pub fuzz_runs: u32,
    pub fork: Option<String>,
    pub seed: u64,
    pub allow_internal_revert: bool,
    /// (runs, depth, fail_on_revert)
    pub invariant: (u32, u32, bool),
    pub quiet_if_empty: bool,
}

enum Status {
    Pass,
    Fail(String),
    Skip(String),
}

struct TestResult {
    name: String,
    status: Status,
    gas: u64,
    runs: Option<u32>,
    logs: Vec<String>,
    args: Option<String>,
    trace: Vec<String>,
}

struct Suite {
    id: String,
    results: Vec<TestResult>,
    elapsed: std::time::Duration,
}

/// Deploy `art` (linking libraries first) from SENDER; returns the CREATE result.
pub fn deploy(db: Db, env: &mut Env, cheats: &mut Cheats, arts: &[Artifact], art: &Artifact, args: &[u8], libs: &mut HashMap<String, Address>) -> (Db, Exec) {
    let db = deploy_libs(db, env, cheats, arts, art, libs, SENDER, &mut None);
    let mut data = evm::link_code(&art.bytecode, &art.links, libs).unwrap_or_else(|e| crate::die(&format!("{}: {e}", art.name)));
    data.extend_from_slice(args);
    evm::transact(db, env, cheats, SENDER, TxKind::Create, data.into(), U256::ZERO, true)
}

/// Deploy from `from` every library that `art`, and the contracts it deploys from artifacts, link against
/// (each library's own libraries first), adding them to `libs`. With `record`, the deployments are also
/// transactions to broadcast (`plain deploy`).
pub fn deploy_libs(mut db: Db, env: &mut Env, cheats: &mut Cheats, arts: &[Artifact], art: &Artifact, libs: &mut HashMap<String, Address>, from: Address, record: &mut Option<&mut Vec<evm::BroadcastTx>>) -> Db {
    // libraries of contracts deployed from artifacts too, in the order embedding them would give
    // (transitively: a script the test deploys can deploy contracts from artifacts in turn)
    let mut needed: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut todo = vec![art];
    let mut seen = std::collections::HashSet::new();
    while let Some(a) = todo.pop() {
        for (file, names) in a.links.as_object().into_iter().flatten() {
            needed.extend(names.as_object().into_iter().flatten().map(|(name, _)| (file.clone(), name.clone())));
        }
        for id in &a.deploys {
            if seen.insert(id) {
                todo.extend(arts.iter().find(|x| x.shard == 0 && format!("{}:{}", x.file, x.name) == *id));
            }
        }
    }
    for (file, name) in needed {
        let key = format!("{file}:{name}");
        if libs.contains_key(&key) {
            continue;
        }
        let Some(lib) = arts.iter().find(|a| a.file == file && a.name == name) else {
            crate::die(&format!("library {key} has no bytecode (is it outside src/test/script?)"));
        };
        db = deploy_libs(db, env, cheats, arts, lib, libs, from, record);
        let code = evm::link_code(&lib.bytecode, &lib.links, libs).unwrap_or_else(|e| crate::die(&format!("{key}: {e}")));
        let r;
        (db, r) = evm::transact(db, env, cheats, from, TxKind::Create, code.clone().into(), U256::ZERO, true);
        let a = r.address.filter(|_| r.ok).unwrap_or_else(|| crate::die(&format!("deploying library {key} failed: {}", decode::reason(&r.output))));
        if let Some(rec) = record {
            rec.push(evm::BroadcastTx { from, to: None, value: U256::ZERO, data: code.into(), created: Some(a) });
        }
        cheats.names.insert(a, name);
        libs.insert(key, a);
    }
    db
}

pub fn is_test_contract(a: &Artifact) -> bool {
    !a.bytecode.is_empty()
        && a.abi.as_array().is_some_and(|f| f.iter().any(|x| x["type"] == "function" && x["name"].as_str().is_some_and(|n| n.starts_with("test") || n.starts_with("invariant"))))
}

pub fn run(arts: Vec<Artifact>, root: &std::path::Path, opts: &Opts) -> bool {
    let t0 = Instant::now();
    decode::init(&arts);
    let arts = Arc::new(arts);
    let remote = evm::Remote(opts.fork.as_ref().map(|u| Arc::new(evm::Fork::new(u, None))), None);
    let suites: Vec<&Artifact> = arts.iter().filter(|a| is_test_contract(a)).collect();
    // a sharded test contract has one artifact per shard; each test runs on the first shard that has it
    let selectors = |a: &Artifact| -> Vec<[u8; 4]> {
        serde_json::from_value::<JsonAbi>(a.abi.clone()).map(|abi| abi.functions().map(|f| f.selector().0).collect()).unwrap_or_default()
    };
    let skips: Vec<std::collections::HashSet<[u8; 4]>> = suites
        .iter()
        .map(|a| suites.iter().filter(|b| b.file == a.file && b.name == a.name && b.shard < a.shard).flat_map(|b| selectors(b)).collect())
        .collect();
    let filter = opts.filter.as_deref();
    let mut results: Vec<Suite> = suites
        .par_iter()
        .zip(skips.par_iter())
        .filter_map(|(art, skip)| run_suite(art, &arts, root, opts, &remote, filter, skip))
        .collect();
    results.sort_by(|a, b| a.id.cmp(&b.id));
    results.dedup_by(|b, a| {
        let same = a.id == b.id;
        if same {
            a.results.append(&mut b.results);
            a.results.sort_by(|x, y| x.name.cmp(&y.name));
            a.elapsed = a.elapsed.max(b.elapsed);
        }
        same
    });
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for s in &results {
        let p = s.results.iter().filter(|r| matches!(r.status, Status::Pass)).count();
        let f = s.results.iter().filter(|r| matches!(r.status, Status::Fail(_))).count();
        let k = s.results.len() - p - f;
        pass += p;
        fail += f;
        skip += k;
        let mut line = format!("{}: {p} passed", s.id);
        if f > 0 {
            line += &format!(", {f} failed");
        }
        if k > 0 {
            line += &format!(", {k} skipped");
        }
        println!("{line} ({:.0?})", s.elapsed);
        for r in &s.results {
            let runs = r.runs.map(|n| format!(", runs: {n}")).unwrap_or_default();
            match &r.status {
                Status::Pass if opts.verbose && r.gas == 0 => println!("  pass {} ({})", r.name, runs.trim_start_matches(", ")),
                Status::Pass if opts.verbose => println!("  pass {} (gas: {}{runs})", r.name, r.gas),
                Status::Skip(why) if opts.verbose => println!("  skip {}: {why}", r.name),
                Status::Fail(why) => {
                    println!("  FAIL {}: {why}", r.name);
                    if let Some(a) = &r.args {
                        println!("       args: {a}");
                    }
                    for l in &r.logs {
                        println!("       log: {l}");
                    }
                    if !r.trace.is_empty() {
                        println!("       trace:");
                        for l in &r.trace {
                            println!("         {l}");
                        }
                    }
                }
                _ => {}
            }
            if opts.verbose && !matches!(r.status, Status::Fail(_)) {
                for l in &r.logs {
                    println!("       log: {l}");
                }
            }
        }
    }
    if results.is_empty() {
        if opts.quiet_if_empty {
            return true;
        }
        println!("no tests found{}", filter.map(|f| format!(" matching {f:?}")).unwrap_or_default());
    }
    println!("{pass} passed, {fail} failed, {skip} skipped ({} suites, {:.2?})", results.len(), t0.elapsed());
    fail == 0
}

fn run_suite(art: &Artifact, arts: &Arc<Vec<Artifact>>, root: &std::path::Path, opts: &Opts, remote: &evm::Remote, filter: Option<&str>, skip: &std::collections::HashSet<[u8; 4]>) -> Option<Suite> {
    let id = format!("{}:{}", art.file, art.name);
    let abi: JsonAbi = serde_json::from_value(art.abi.clone()).ok()?;
    let mut tests: Vec<&Function> = abi
        .functions()
        .filter(|f| f.name.starts_with("test") || f.name.starts_with("invariant"))
        .filter(|f| !skip.contains(&f.selector().0))
        .filter(|f| filter.is_none_or(|flt| id.contains(flt) || f.name.contains(flt)))
        .collect();
    if tests.is_empty() {
        return None;
    }
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    let t0 = Instant::now();
    let mut cheats = Cheats::new(arts.clone(), root.to_path_buf());
    cheats.fork_url = opts.fork.clone();
    cheats.track_creates = true;
    cheats.allow_internal_revert = opts.allow_internal_revert;
    let mut env = Env::new(31337);
    let db = evm::base_db(remote.clone());
    let mut libs = HashMap::new();
    let (db, r) = deploy(db, &mut env, &mut cheats, arts, art, &[], &mut libs);
    cheats.libs = libs;
    let fail_all = |why: String, logs: Vec<String>| Suite {
        id: id.clone(),
        results: tests.iter().map(|f| TestResult { name: sig(f), status: Status::Fail(why.clone()), gas: 0, runs: None, logs: logs.clone(), args: None, trace: vec![] }).collect(),
        elapsed: t0.elapsed(),
    };
    let Some(addr) = r.address.filter(|_| r.ok) else {
        return Some(fail_all(format!("constructor reverted: {}", decode::reason(&r.output)), cheats.console));
    };
    let mut db = db;
    // forge gives the test contract 2^96-1 wei
    if let Some(acc) = db.cache.accounts.get_mut(&addr) {
        acc.info.balance = U256::from((1u128 << 96) - 1);
    }
    if abi.functions().any(|f| f.name == "setUp" && f.inputs.is_empty()) {
        let r;
        (db, r) = evm::transact(db, &mut env, &mut cheats, SENDER, TxKind::Call(addr), Bytes::from_static(&[0x0a, 0x92, 0x54, 0xe4]), U256::ZERO, true);
        let pending = cheats.finish();
        if !r.ok || pending.is_some() {
            let why = if r.ok { pending.unwrap() } else { decode::reason(&r.output) };
            return Some(fail_all(format!("setUp() failed: {why}"), cheats.console));
        }
    }
    cheats.console.clear();
    cheats.track_creates = false;
    cheats.names.insert(addr, art.name.clone());
    // the post-setUp state, shared read-only: each test/run executes on an overlay of it
    let db = Arc::new(db);
    let inline = inline_runs(&root.join(&art.file));
    let (invariants, tests): (Vec<&Function>, Vec<&Function>) = tests.into_iter().partition(|f| f.name.starts_with("invariant"));
    let mut results: Vec<TestResult> = tests
        .par_iter()
        .map(|f| {
            let t = Instant::now();
            let r = invert_fail(run_test(f, addr, &db, &env, &cheats, inline.get(&f.name).copied().unwrap_or(opts.fuzz_runs), opts), f);
            if std::env::var_os("PLAIN_DEBUG").is_some() {
                eprintln!("test {:.1?} {}:{}", t.elapsed(), id, r.name);
            }
            r
        })
        .collect();
    if !invariants.is_empty() {
        results.extend(run_invariants(&invariants, addr, &db, &env, &cheats, opts));
        results.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Some(Suite { id, results, elapsed: t0.elapsed() })
}

/// `/// forge-config: default.fuzz.runs = N` natspec above a test function.
fn inline_runs(path: &std::path::Path) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    let Ok(src) = std::fs::read_to_string(path) else { return out };
    let mut pending = None;
    for line in src.lines() {
        let t = line.trim();
        if let Some(cfg) = t.split("forge-config:").nth(1) {
            if let Some((k, v)) = cfg.split_once('=') {
                let k = k.trim();
                if k == "default.fuzz.runs" || k == "fuzz.runs" {
                    pending = v.trim().parse().ok();
                }
            }
        } else if let Some(rest) = t.strip_prefix("function ") {
            if let Some(n) = pending.take() {
                out.insert(rest.split('(').next().unwrap_or_default().trim().to_string(), n);
            }
        } else if !t.is_empty() && !t.starts_with("//") && !t.starts_with('*') && !t.starts_with("/*") {
            pending = None;
        }
    }
    out
}

fn sig(f: &Function) -> String {
    format!("{}({})", f.name, f.inputs.iter().map(|p| p.ty.as_str()).collect::<Vec<_>>().join(","))
}

fn failed_slot(db: &Db) -> bool {
    let slot = U256::from_be_bytes(B256::right_padding_from(b"failed").0);
    db.storage_ref(CHEATS, slot).is_ok_and(|v| !v.is_zero())
}

/// One call of the test function on a fresh copy of the post-setUp state.
fn exec_once(data: Bytes, addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats) -> (Exec, Cheats, Option<String>) {
    let mut c = cheats.clone();
    let mut e = env.clone();
    let (db, r) = evm::transact(evm::overlay(db), &mut e, &mut c, SENDER, TxKind::Call(addr), data, U256::ZERO, true);
    let mut why = None;
    if !r.ok {
        why = Some(decode::reason(&r.output));
    } else if let Some(p) = c.finish() {
        why = Some(p);
    } else if failed_slot(&db) {
        why = Some("assertion failed".into());
    }
    (r, c, why)
}

fn run_test(f: &Function, addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats, fuzz_runs: u32, opts: &Opts) -> TestResult {
    let name = sig(f);
    let mut res = TestResult { name, status: Status::Pass, gas: 0, runs: None, logs: vec![], args: None, trace: vec![] };
    let sel = f.selector();
    if f.inputs.is_empty() {
        let data = Bytes::copy_from_slice(sel.as_slice());
        let (r, c, why) = exec_once(data.clone(), addr, db, env, cheats);
        res.gas = r.gas;
        res.logs = c.console;
        res.status = match why {
            _ if r.output.as_ref() == SKIP_MAGIC || r.output.as_ref() == ASSUME_MAGIC => Status::Skip("skipped".into()),
            Some(w) => Status::Fail(w),
            None => Status::Pass,
        };
        if matches!(res.status, Status::Fail(_)) && !f.name.starts_with("testFail") {
            let (lines, root) = trace(data, addr, db, env, cheats);
            res.trace = lines;
            if let (Status::Fail(w), Some(root)) = (&mut res.status, root) {
                if w == "reverted without reason" {
                    *w = root;
                }
            }
        }
        return res;
    }
    // fuzz
    let Ok(types) = f.inputs.iter().map(|p| p.resolve()).collect::<Result<Vec<DynSolType>, _>>() else {
        res.status = Status::Skip("unsupported fuzz parameter types".into());
        return res;
    };
    let base_seed = opts.seed ^ u64::from_be_bytes(alloy_primitives::keccak256(res.name.as_bytes())[..8].try_into().unwrap());
    let stop = std::sync::atomic::AtomicBool::new(false);
    let rejects = std::sync::atomic::AtomicU32::new(0);
    // each run gets its own deterministic stream; runs execute in parallel
    let outcomes: Vec<(u32, u64, Option<(String, String, Vec<String>, Vec<u8>)>, bool)> = (0..fuzz_runs)
        .into_par_iter()
        .filter_map(|i| {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let mut rng = Rng((base_seed ^ (i as u64).wrapping_mul(0x9E3779B97F4A7C15)) | 1);
            loop {
                let vals: Vec<DynSolValue> = types.iter().map(|t| fuzz_value(t, &mut rng)).collect();
                let data = f.abi_encode_input(&vals).unwrap();
                let (r, c, why) = exec_once(data.clone().into(), addr, db, env, cheats);
                if r.output.as_ref() == ASSUME_MAGIC {
                    if rejects.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 65536 {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        return Some((i, 0, Some(("too many vm.assume rejections".into(), String::new(), vec![], vec![])), false));
                    }
                    continue;
                }
                let skip = r.output.as_ref() == SKIP_MAGIC;
                let fail = why.map(|w| (w, vals.iter().map(decode::display).collect::<Vec<_>>().join(", "), c.console, data.clone()));
                if fail.is_some() || skip {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return Some((i, r.gas, fail, skip));
            }
        })
        .collect();
    let runs = outcomes.len() as u32;
    let gas_sum: u64 = outcomes.iter().map(|o| o.1).sum();
    if outcomes.iter().any(|o| o.3) {
        res.status = Status::Skip("skipped".into());
    } else if let Some((_, _, Some((why, args, logs, data)), _)) = outcomes.into_iter().filter(|o| o.2.is_some()).min_by_key(|o| o.0) {
        res.status = Status::Fail(why);
        res.args = Some(args).filter(|a| !a.is_empty());
        res.logs = logs;
        if !data.is_empty() && !f.name.starts_with("testFail") {
            let (lines, root) = trace(data.into(), addr, db, env, cheats);
            res.trace = lines;
            if let (Status::Fail(w), Some(root)) = (&mut res.status, root) {
                if w == "reverted without reason" {
                    *w = root;
                }
            }
        }
    }
    res.runs = Some(runs);
    res.gas = gas_sum / runs.max(1) as u64;
    res
}

/// Legacy `testFail*` (removed in forge 1.0): passes iff the body fails.
fn invert_fail(mut r: TestResult, f: &Function) -> TestResult {
    if f.name.starts_with("testFail") {
        r.status = match r.status {
            Status::Fail(_) => Status::Pass,
            Status::Pass => Status::Fail("testFail* test did not fail".into()),
            s => s,
        };
        r.args = None;
    }
    r
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn u256(&mut self) -> U256 {
        U256::from_limbs([self.next(), self.next(), self.next(), self.next()])
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Random value biased towards edge cases.
fn fuzz_value(ty: &DynSolType, r: &mut Rng) -> DynSolValue {
    match ty {
        DynSolType::Uint(bits) => {
            let max = if *bits == 256 { U256::MAX } else { (U256::from(1) << *bits) - U256::from(1) };
            let v = match r.next() % 10 {
                0 => U256::ZERO,
                1 => U256::from(1),
                2 => max,
                3 => max - U256::from(r.next() % 3).min(max),
                4 | 5 => U256::from(r.next() % 1000),
                6 => U256::from(1) << (r.next() as usize % *bits),
                _ => r.u256(),
            };
            DynSolValue::Uint(v & max, *bits)
        }
        DynSolType::Int(bits) => {
            let max = (I256::ONE << (*bits - 1)) - I256::ONE;
            let min = -max - I256::ONE;
            let v = match r.next() % 10 {
                0 => I256::ZERO,
                1 => I256::ONE,
                2 => I256::MINUS_ONE,
                3 => max,
                4 => min,
                5 | 6 => I256::try_from((r.next() % 2000) as i64 - 1000).unwrap(),
                _ => {
                    // sign-extend random bits into range
                    let raw = I256::from_raw(r.u256());
                    if *bits == 256 { raw } else { raw.asr(256 - *bits) }
                }
            };
            DynSolValue::Int(v, *bits)
        }
        DynSolType::Address => DynSolValue::Address(match r.next() % 8 {
            0 => Address::ZERO,
            1 => Address::with_last_byte((r.next() % 16) as u8 + 1),
            _ => Address::from_slice(&r.bytes(20)),
        }),
        DynSolType::Bool => DynSolValue::Bool(r.next() & 1 == 1),
        DynSolType::FixedBytes(n) => DynSolValue::FixedBytes(B256::right_padding_from(&r.bytes(*n)), *n),
        DynSolType::Bytes => {
            let n = (r.next() % 65) as usize;
            DynSolValue::Bytes(r.bytes(n))
        }
        DynSolType::String => {
            let n = (r.next() % 33) as usize;
            DynSolValue::String((0..n).map(|_| (b' ' + (r.next() % 95) as u8) as char).collect())
        }
        DynSolType::Array(t) => {
            let n = (r.next() % 5) as usize;
            DynSolValue::Array((0..n).map(|_| fuzz_value(t, r)).collect())
        }
        DynSolType::FixedArray(t, n) => DynSolValue::FixedArray((0..*n).map(|_| fuzz_value(t, r)).collect()),
        DynSolType::Tuple(ts) => DynSolValue::Tuple(ts.iter().map(|t| fuzz_value(t, r)).collect()),
        _ => DynSolValue::Bool(false),
    }
}

/// Re-run a failing call with tracing; keep the test's direct calls plus the path to each failure.
fn trace(data: Bytes, addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats) -> (Vec<String>, Option<String>) {
    let mut c = cheats.clone();
    c.trace = Some(vec![]);
    c.track_creates = true;
    let mut e = env.clone();
    let _ = evm::transact(evm::overlay(db), &mut e, &mut c, SENDER, TxKind::Call(addr), data, U256::ZERO, false);
    let nodes = c.trace.take().unwrap_or_default();
    let failed = |n: &evm::TraceNode| n.result.as_ref().is_some_and(|(r, _)| !r.is_ok());
    let mut parent = vec![None; nodes.len()];
    let mut stack: Vec<usize> = vec![];
    for (i, n) in nodes.iter().enumerate() {
        while stack.last().is_some_and(|&j| nodes[j].depth >= n.depth) {
            stack.pop();
        }
        parent[i] = stack.last().copied();
        stack.push(i);
    }
    // getCode calls come from rewritten `new` expressions (see rewrite.rs): noise in a trace
    let get_code = alloy_primitives::keccak256("getCode(string)");
    let mut keep: Vec<bool> = nodes.iter().map(|n| n.depth <= 1 && !(n.to == evm::CHEATS && n.input.starts_with(&get_code[..4]))).collect();
    for i in 0..nodes.len() {
        if failed(&nodes[i]) {
            let mut j = Some(i);
            while let Some(k) = j {
                keep[k] = true;
                j = parent[k];
            }
        }
    }
    let name = |a: &Address| -> String {
        if *a == evm::CHEATS {
            return "vm".into();
        }
        c.labels.get(a).or(c.names.get(a)).cloned().unwrap_or_else(|| a.to_checksum(None))
    };
    let short = |s: String| if s.len() > 66 { format!("{}…", &s[..64]) } else { s };
    let mut lines = vec![];
    for (i, n) in nodes.iter().enumerate().filter(|(i, _)| keep[*i]) {
        let pad = "  ".repeat(n.depth);
        let call = if n.create {
            format!("new {}", c.names.get(&n.to).cloned().unwrap_or_else(|| "<contract>".into()))
        } else {
            let sel: Option<[u8; 4]> = n.input.get(..4).map(|s| s.try_into().unwrap());
            let fname = sel.and_then(|s| {
                if n.to == evm::CHEATS {
                    let sig = evm::supported().get(&s).or_else(|| evm::vm_sol().get(&s))?;
                    let args = match sig.ty.abi_decode_params(&n.input[4..]) {
                        Ok(alloy_dyn_abi::DynSolValue::Tuple(v)) => v.iter().map(|x| short(decode::display(x))).collect::<Vec<_>>().join(", "),
                        _ => String::new(),
                    };
                    return Some(format!("{}({args})", sig.name));
                }
                let f = decode::tables()?.functions.get(&s)?;
                let args = f.abi_decode_input(&n.input[4..]).map(|v| v.iter().map(|x| short(decode::display(x))).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                Some(format!("{}({args})", f.name))
            });
            let fname = fname.unwrap_or_else(|| if n.input.is_empty() { "receive()".into() } else { format!("0x{}", hex::encode(&n.input[..4.min(n.input.len())])) });
            format!("{}.{fname}", name(&n.to))
        };
        let value = if n.value.is_zero() { String::new() } else { format!(" value: {}", n.value) };
        let res = match &n.result {
            Some((r, out)) if !r.is_ok() => {
                let why = if *r == revm::interpreter::InstructionResult::Revert { decode::reason(out) } else { format!("{r:?}") };
                format!("  ✗ {why}")
            }
            _ if i == 0 => String::new(),
            _ => String::new(),
        };
        let mut line = format!("{pad}{call}{value}{res}");
        for (a, l) in &c.labels {
            line = line.replace(&a.to_checksum(None), l);
        }
        lines.push(line);
    }
    if lines.len() > 60 {
        let cut = lines.len() - 60;
        lines.drain(..cut);
        lines.insert(0, format!("… {cut} earlier lines"));
    }
    // the innermost failure explains an empty top-level revert (e.g. OutOfFunds, OutOfGas)
    let has_failed_child: Vec<bool> = (0..nodes.len()).map(|i| (0..nodes.len()).any(|j| parent[j] == Some(i) && failed(&nodes[j]))).collect();
    let root = (0..nodes.len()).find(|&i| failed(&nodes[i]) && !has_failed_child[i] && nodes[i].to != evm::CHEATS).and_then(|i| {
        let (r, out) = nodes[i].result.as_ref()?;
        (*r != revm::interpreter::InstructionResult::Revert || !out.is_empty()).then(|| if *r == revm::interpreter::InstructionResult::Revert { decode::reason(out) } else { format!("{r:?}") })
    });
    (lines, root)
}

/// Static call on the test contract returning decoded addresses/selectors from StdInvariant getters.
fn view(addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats, sig: &str, ty: &str) -> Vec<DynSolValue> {
    let mut c = cheats.clone();
    let mut e = env.clone();
    let sel = alloy_primitives::keccak256(sig.as_bytes());
    let (_, r) = evm::transact(evm::overlay(db), &mut e, &mut c, SENDER, TxKind::Call(addr), Bytes::copy_from_slice(&sel[..4]), U256::ZERO, false);
    if !r.ok {
        return vec![];
    }
    match DynSolType::parse(ty).ok().and_then(|t| t.abi_decode(&r.output).ok()) {
        Some(DynSolValue::Array(v)) => v,
        _ => vec![],
    }
}

/// forge-style invariant testing: random call sequences against targets, invariants checked after every call.
fn run_invariants(invs: &[&Function], addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats, opts: &Opts) -> Vec<TestResult> {
    let (runs, depth, fail_on_revert) = opts.invariant;
    let abi_of = |a: &Address| -> Option<JsonAbi> {
        let name = cheats.names.get(a)?;
        let art = cheats.artifacts.iter().find(|x| &x.name == name)?;
        serde_json::from_value(art.abi.clone()).ok()
    };
    let addrs = |sig: &str| view(addr, db, env, cheats, sig, "address[]").into_iter().filter_map(|v| v.as_address()).collect::<Vec<_>>();
    let excluded = addrs("excludeContracts()");
    let mut targets: Vec<Address> = addrs("targetContracts()");
    if targets.is_empty() {
        targets = cheats.names.keys().copied().filter(|a| *a != addr).collect();
        targets.sort();
    }
    targets.retain(|a| !excluded.contains(a));
    let selected: Vec<(Address, Vec<[u8; 4]>)> = view(addr, db, env, cheats, "targetSelectors()", "(address,bytes4[])[]")
        .into_iter()
        .filter_map(|v| match v {
            DynSolValue::Tuple(t) => Some((t[0].as_address()?, t[1].as_array()?.iter().filter_map(|s| s.as_fixed_bytes().map(|(b, _)| b[..4].try_into().unwrap())).collect())),
            _ => None,
        })
        .collect();
    let mut calls: Vec<(Address, Function)> = vec![];
    for t in &targets {
        let Some(abi) = abi_of(t) else { continue };
        let only = selected.iter().find(|(a, _)| a == t).map(|(_, s)| s);
        for f in abi.functions() {
            let mutating = !matches!(f.state_mutability, alloy_json_abi::StateMutability::View | alloy_json_abi::StateMutability::Pure);
            if only.map_or(mutating, |s| s.contains(&f.selector().0)) {
                calls.push((*t, f.clone()));
            }
        }
    }
    let senders = addrs("targetSenders()");
    let excluded_senders = addrs("excludeSenders()");
    let skip = |why: &str| invs.iter().map(|f| TestResult { name: sig(f), status: Status::Skip(why.into()), gas: 0, runs: None, logs: vec![], args: None, trace: vec![] }).collect();
    if calls.is_empty() {
        return skip("no target contracts with known ABIs");
    }
    let types: Vec<Vec<DynSolType>> = calls.iter().map(|(_, f)| f.inputs.iter().filter_map(|p| p.resolve().ok()).collect()).collect();
    let stop = std::sync::atomic::AtomicBool::new(false);
    // each run: Some((invariant index, reason, calls made, the last calls)) on failure
    let failures: Vec<(u32, usize, String, usize, Vec<String>)> = (0..runs)
        .into_par_iter()
        .filter_map(|run| {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let mut rng = Rng((opts.seed ^ (run as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15)) | 1);
            let mut c = cheats.clone();
            let mut vm = evm::Vm::new(evm::overlay(db), env, &mut c);
            // (call index, args, sender, ok): formatted only if the run fails
            let mut seq: Vec<(usize, Vec<DynSolValue>, Address, bool)> = vec![];
            let tail = |seq: &[(usize, Vec<DynSolValue>, Address, bool)], c: &Cheats| -> (usize, Vec<String>) {
                let lines = seq[seq.len().saturating_sub(10)..]
                    .iter()
                    .map(|(i, vals, sender, ok)| {
                        let (target, f) = &calls[*i];
                        let name = c.names.get(target).cloned().unwrap_or_else(|| target.to_string());
                        format!("{name}.{}({}) from {sender}{}", f.name, vals.iter().map(decode::display).collect::<Vec<_>>().join(", "), if *ok { "" } else { " (reverted)" })
                    })
                    .collect();
                (seq.len(), lines)
            };
            for _ in 0..depth {
                let i = (rng.next() % calls.len() as u64) as usize;
                let (target, f) = &calls[i];
                let vals: Vec<DynSolValue> = types[i].iter().map(|t| fuzz_value(t, &mut rng)).collect();
                let Ok(data) = f.abi_encode_input(&vals) else { continue };
                let sender = if senders.is_empty() {
                    let mut s = Address::from_slice(&rng.bytes(20));
                    while excluded_senders.contains(&s) || s.0[..19] == [0u8; 19] {
                        s = Address::from_slice(&rng.bytes(20));
                    }
                    s
                } else {
                    senders[(rng.next() % senders.len() as u64) as usize]
                };
                let r = vm.call(sender, TxKind::Call(*target), data.into(), U256::ZERO, true);
                vm.cheats().finish();
                seq.push((i, vals, sender, r.ok));
                if fail_on_revert && !r.ok && r.output.as_ref() != ASSUME_MAGIC {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    let (n, lines) = tail(&seq, vm.cheats());
                    return Some((run, usize::MAX, format!("call reverted: {}", decode::reason(&r.output)), n, lines));
                }
                for (k, inv) in invs.iter().enumerate() {
                    vm.cheats().failed = false;
                    let r = vm.call(SENDER, TxKind::Call(addr), Bytes::copy_from_slice(&inv.selector()[..]), U256::ZERO, false);
                    if !r.ok || vm.cheats().failed {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        let why = if r.ok { "assertion failed".into() } else { decode::reason(&r.output) };
                        let (n, lines) = tail(&seq, vm.cheats());
                        return Some((run, k, why, n, lines));
                    }
                }
            }
            None
        })
        .collect();
    let first = failures.into_iter().min_by_key(|f| f.0);
    invs.iter()
        .enumerate()
        .map(|(k, f)| {
            let mut r = TestResult { name: sig(f), status: Status::Pass, gas: 0, runs: Some(runs), logs: vec![], args: None, trace: vec![] };
            if let Some((_, fk, why, n, tail)) = &first {
                if *fk == k || *fk == usize::MAX {
                    r.status = Status::Fail(format!("invariant broken: {why}"));
                    r.trace = std::iter::once(format!("after {n} calls (last {} shown):", tail.len())).chain(tail.iter().cloned()).collect();
                } else {
                    r.status = Status::Skip("not checked to completion (another invariant broke)".into());
                }
            }
            r
        })
        .collect()
}
