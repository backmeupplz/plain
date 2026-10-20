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

/// Which tests run: forge's --match-path (a glob over the file's path), --match-contract and --match-test (taken
/// as substrings: what a regex without special characters matches), and plain's own filter, a substring of the
/// suite id ("test/Vault.t.sol:VaultTest") or of the test's name. A test runs when it passes all that are given.
#[derive(Clone, Default, PartialEq)]
pub struct Filter {
    pub any: Option<String>,
    pub path: Option<String>,
    pub contract: Option<String>,
    pub test: Option<String>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.any.is_none() && self.path.is_none() && self.contract.is_none() && self.test.is_none()
    }

    pub fn file(&self, f: &str) -> bool {
        self.path.as_deref().is_none_or(|p| path_matches(p, f))
    }

    /// The suite `file:Name` and its test `name` (None: any of its tests).
    pub fn wants(&self, id: &str, name: Option<&str>) -> bool {
        let (file, contract) = id.rsplit_once(':').unwrap_or((id, ""));
        let named = |want: &Option<String>, n: Option<&str>| want.as_deref().is_none_or(|w| n.is_none_or(|n| n.contains(w)));
        self.file(file)
            && self.contract.as_deref().is_none_or(|c| contract.contains(c))
            && named(&self.test, name)
            && self.any.as_deref().is_none_or(|a| id.contains(a) || name.is_none_or(|n| n.contains(a)))
    }
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts = [("", &self.any), ("--match-path ", &self.path), ("--match-contract ", &self.contract), ("--match-test ", &self.test)];
        let s: Vec<String> = parts.iter().filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}{v:?}"))).collect();
        f.write_str(&s.join(" "))
    }
}

/// forge's --match-path: a glob over the path from the project root (`*` crosses `/`, `?`, `[abc]`, `{a,b}`), or a
/// file name that contains it.
pub fn path_matches(pat: &str, path: &str) -> bool {
    let (pat, path) = (pat.strip_prefix("./").unwrap_or(pat), path.strip_prefix("./").unwrap_or(path));
    path.rsplit('/').next().is_some_and(|name| name.contains(pat)) || alternatives(pat).iter().any(|p| glob(p.as_bytes(), path.as_bytes()))
}

/// A glob with each `{a,b}` spelled out.
fn alternatives(p: &str) -> Vec<String> {
    let Some(i) = p.find('{') else { return vec![p.to_string()] };
    let (mut depth, mut cuts, mut end) = (0, vec![i], None);
    for (j, c) in p.char_indices().skip(i) {
        match c {
            '{' => depth += 1,
            '}' if depth == 1 => {
                end = Some(j);
                break;
            }
            '}' => depth -= 1,
            ',' if depth == 1 => cuts.push(j),
            _ => {}
        }
    }
    let Some(end) = end else { return vec![p.to_string()] };
    cuts.push(end);
    cuts.windows(2).flat_map(|w| alternatives(&format!("{}{}{}", &p[..i], &p[w[0] + 1..w[1]], &p[end + 1..]))).collect()
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => {
            let rest = &p[p.iter().take_while(|c| **c == b'*').count()..];
            (0..=s.len()).any(|k| glob(rest, &s[k..]))
        }
        Some(b'?') => !s.is_empty() && glob(&p[1..], &s[1..]),
        Some(b'[') => match (p.iter().position(|c| *c == b']'), s.first()) {
            (Some(e), Some(c)) => {
                let (neg, set) = match &p[1..e] {
                    [b'!' | b'^', rest @ ..] => (true, rest),
                    set => (false, set),
                };
                let hit = set.iter().enumerate().any(|(k, x)| x == c || (set.get(k + 1) == Some(&b'-') && set.get(k + 2).is_some_and(|hi| x <= c && c <= hi)));
                hit != neg && glob(&p[e + 1..], &s[1..])
            }
            _ => false,
        },
        Some(c) => s.first() == Some(c) && glob(&p[1..], &s[1..]),
    }
}

pub struct Opts {
    pub filter: Filter,
    pub verbose: bool,
    pub fuzz_runs: u32,
    pub fork: Option<String>,
    pub seed: u64,
    pub allow_internal_revert: bool,
    /// (runs, depth, fail_on_revert)
    pub invariant: (u32, u32, bool),
    pub quiet_if_empty: bool,
    /// reuse results of suites whose bytecode, settings and inputs haven't changed (not with --force or --fork)
    pub cache: bool,
    /// what tests may reach outside the EVM
    pub access: Arc<crate::project::Access>,
    /// the EVM rules the code was compiled for
    pub spec: revm::primitives::hardfork::SpecId,
    /// the solc version, which artifact ids may name
    pub solc: String,
    /// forge's --isolate (or `isolate = true`): each call a test makes is its own transaction
    pub isolate: bool,
    /// gas for each test (and the block), forge's gas_limit / --gas-limit
    pub gas_limit: u64,
}

/// `/// forge-config:` settings above a test function.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Inline {
    fuzz_runs: Option<u32>,
    inv_runs: Option<u32>,
    inv_depth: Option<u32>,
    fail_on_revert: Option<bool>,
    isolate: Option<bool>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Status {
    Pass,
    Fail(String),
    Skip(String),
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    /// results reused from an earlier run
    cached: usize,
}

/// A suite's results by test, with what it read besides its bytecode (see evm::Reads).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Entry {
    deps: std::collections::BTreeMap<String, String>,
    results: Vec<TestResult>,
}

/// Results are deterministic given the test contract's bytecode, the settings, this binary and what the
/// suite read (artifacts it deployed, files, env vars): the key covers the first three, `Entry::deps` the rest.
fn cache_key(art: &Artifact, opts: &Opts, inline: &HashMap<String, Inline>) -> String {
    use sha2::Digest;
    static EXE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let exe = EXE.get_or_init(|| {
        let m = std::env::current_exe().and_then(std::fs::metadata);
        m.map(|m| format!("{}{:?}", m.len(), m.modified().ok())).unwrap_or_default()
    });
    let mut inline: Vec<_> = inline.iter().collect();
    inline.sort();
    let s = format!("{exe}|{}|{}|{}|{}|{}|{:?}|{}|{}|{inline:?}|{:?}|{:?}|{}|{}", art.file, art.name, art.bytecode, art.abi, opts.fuzz_runs, opts.invariant, opts.seed, opts.allow_internal_revert, opts.access, opts.spec, opts.isolate, opts.gas_limit);
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

fn entry_path(key: &str) -> std::path::PathBuf {
    crate::solc::home().join("cache/tests").join(format!("{key}.json"))
}

/// Deploy `art` (linking libraries first) from SENDER; returns the CREATE result.
pub fn deploy(db: Db, env: &mut Env, cheats: &mut Cheats, arts: &[Artifact], art: &Artifact, args: &[u8], libs: &mut HashMap<String, Address>) -> (Db, Exec) {
    let db = deploy_libs(db, env, cheats, arts, art, libs, SENDER, &mut None);
    let mut data = evm::link_code(&art.bytecode, &art.links, libs).unwrap_or_else(|e| crate::die(&format!("{}: {e}", art.name)));
    data.extend_from_slice(args);
    evm::transact(db, env, cheats, SENDER, TxKind::Create, data.into(), U256::ZERO, true)
}

/// Deploy from `from` every library that `art`, and the contracts it deploys from artifacts, link against
/// (each library's own libraries first; the default profile's build of each, as forge's linker picks), adding them
/// to `libs`. With `record`, the deployments are also transactions to broadcast (`plain deploy`).
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
                let named = |x: &&Artifact| x.shard == 0 && format!("{}:{}", x.file, x.name) == *id;
                todo.extend(arts.iter().filter(named).find(|x| x.profile == art.profile).or_else(|| arts.iter().find(named)));
            }
        }
    }
    for (file, name) in needed {
        let key = format!("{file}:{name}");
        if libs.contains_key(&key) {
            continue;
        }
        let Some(lib) = arts.iter().find(|a| a.file == file && a.name == name && a.profile == 0) else {
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

/// A suite's starting state, as forge sets it up: every library any artifact links against, deployed from
/// LIBRARY_DEPLOYER in forge's order (its artifact ids sorted: out/<File.sol>/<Name>.json), so at the addresses
/// forge's are, then the test contract from SENDER at nonce 1 (so at TEST_CONTRACT), with 2^96-1 wei from before its
/// constructor runs; SENDER and LIBRARY_DEPLOYER are left with 2^96-1 wei too. Each library is the default profile's
/// build, which forge's linker picks for every profile.
fn setup(db: Db, env: &mut Env, cheats: &mut Cheats, arts: &[Artifact], art: &Artifact) -> (Db, Exec) {
    let initial = U256::from((1u128 << 96) - 1);
    let base = |f: &str| f.rsplit('/').next().unwrap_or(f).to_string();
    let mut needed: std::collections::BTreeMap<(String, String), &Artifact> = Default::default();
    for a in arts.iter().filter(|a| a.shard == 0) {
        for (file, names) in a.links.as_object().into_iter().chain(a.deployed_links.as_object()).flatten() {
            for name in names.as_object().into_iter().flatten().map(|(n, _)| n) {
                if let Some(lib) = arts.iter().find(|l| l.file == *file && l.name == *name && l.profile == 0) {
                    needed.insert((base(file), format!("{name}.json")), lib);
                }
            }
        }
    }
    // where each lands, so libraries linking libraries link wherever their order puts them
    let mut db = db;
    let start = db.basic_ref(evm::LIBRARY_DEPLOYER).ok().flatten().map_or(0, |i| i.nonce);
    let libs: HashMap<String, Address> = needed.values().enumerate().map(|(i, l)| (format!("{}:{}", l.file, l.name), evm::LIBRARY_DEPLOYER.create(start + i as u64))).collect();
    for lib in needed.values() {
        let code = evm::link_code(&lib.bytecode, &lib.links, &libs).unwrap_or_else(|e| crate::die(&format!("{}: {e}", lib.name)));
        let r;
        (db, r) = evm::transact(db, env, cheats, evm::LIBRARY_DEPLOYER, TxKind::Create, code.into(), U256::ZERO, true);
        let a = r.address.filter(|_| r.ok).unwrap_or_else(|| crate::die(&format!("deploying library {} failed: {}", lib.name, decode::reason(&r.output))));
        cheats.names.insert(a, lib.name.clone());
    }
    for l in libs.keys() {
        cheats.read(format!("art:{l}"));
    }
    let set = |db: &mut Db, a: Address, f: &dyn Fn(&mut revm::state::AccountInfo)| {
        let mut info = db.basic_ref(a).ok().flatten().unwrap_or_default();
        f(&mut info);
        db.insert_account_info(a, info);
    };
    set(&mut db, SENDER, &|i| i.nonce = 1);
    set(&mut db, SENDER.create(1), &|i| i.balance = initial);
    let code = evm::link_code(&art.bytecode, &art.links, &libs).unwrap_or_else(|e| crate::die(&format!("{}: {e}", art.name)));
    // what forge deploys before setUp keeps its state when a test selects a fork
    cheats.persistent = libs.values().copied().chain([SENDER.create(1)]).collect();
    cheats.libs = libs;
    let (mut db, r) = evm::transact(db, env, cheats, SENDER, TxKind::Create, code.into(), U256::ZERO, true);
    for a in [SENDER, evm::LIBRARY_DEPLOYER] {
        set(&mut db, a, &|i| i.balance = initial);
    }
    (db, r)
}

/// Whether a filter selects anything in a test contract: its id or one of its tests.
pub fn matches(a: &Artifact, filter: &Filter) -> bool {
    let id = format!("{}:{}", a.file, a.name);
    a.abi.as_array().is_some_and(|f| f.iter().any(|x| x["name"].as_str().is_some_and(|n| crate::rewrite::is_test_name(n) && filter.wants(&id, Some(n)))))
}

pub fn is_test_contract(a: &Artifact) -> bool {
    !a.bytecode.is_empty() && a.abi.as_array().is_some_and(|f| f.iter().any(|x| x["type"] == "function" && x["name"].as_str().is_some_and(crate::rewrite::is_test_name)))
}

pub fn run(arts: Vec<Artifact>, root: &std::path::Path, opts: &Opts) -> bool {
    let t0 = Instant::now();
    decode::init(&arts);
    let arts = Arc::new(arts);
    let fork = opts.fork.as_ref().map(|u| Arc::new(evm::Fork::new(u, None)));
    // on a fork, tests see the forked block: its chain id, number, timestamp and fees, as forge does
    let mut env = match &fork {
        Some(f) => f.env().unwrap_or_else(|e| crate::die(&format!("fork: {e}"))),
        None => Env::new(31337),
    };
    env.spec = opts.spec;
    let remote = evm::Remote(fork, None);
    let suites: Vec<&Artifact> = arts.iter().filter(|a| is_test_contract(a)).collect();
    // a sharded test contract has one artifact per unit: a test runs on the unit that owns it; tests no unit
    // owns but every unit has (inherited from other files) run on unit 0
    let selectors = |a: &Artifact| -> Vec<([u8; 4], String)> {
        serde_json::from_value::<JsonAbi>(a.abi.clone()).map(|abi| abi.functions().map(|f| (f.selector().0, f.name.clone())).collect()).unwrap_or_default()
    };
    let skips: Vec<std::collections::HashSet<[u8; 4]>> = suites
        .iter()
        .map(|a| {
            let sibs: Vec<&&Artifact> = suites.iter().filter(|b| b.file == a.file && b.name == a.name).collect();
            if sibs.len() < 2 {
                return Default::default();
            }
            let sels: Vec<Vec<([u8; 4], String)>> = sibs.iter().map(|b| selectors(b)).collect();
            let first = sibs.iter().all(|b| b.shard >= a.shard);
            selectors(a)
                .into_iter()
                .filter(|(sel, name)| {
                    let owned = sibs.iter().any(|b| b.tests.contains(name));
                    let run = if owned { a.tests.contains(name) } else { first && sels.iter().all(|s| s.iter().any(|x| x.0 == *sel)) };
                    !run
                })
                .map(|x| x.0)
                .collect()
        })
        .collect();
    let filter = &opts.filter;
    let mut results: Vec<Suite> = suites
        .par_iter()
        .zip(skips.par_iter())
        .filter_map(|(art, skip)| run_suite(art, &arts, root, opts, &remote, &env, filter, skip))
        .collect();
    results.sort_by(|a, b| a.id.cmp(&b.id));
    results.dedup_by(|b, a| {
        let same = a.id == b.id;
        if same {
            a.results.append(&mut b.results);
            a.results.sort_by(|x, y| x.name.cmp(&y.name));
            a.elapsed = a.elapsed.max(b.elapsed);
            a.cached += b.cached;
        }
        same
    });
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    let cached: usize = results.iter().map(|s| s.cached).sum();
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
        if s.cached == s.results.len() {
            println!("{line} (cached)");
        } else {
            println!("{line} ({:.0?})", s.elapsed);
        }
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
        println!("no tests found{}", if filter.is_empty() { String::new() } else { format!(" matching {filter}") });
    }
    let reused = if cached > 0 { format!(", {cached} results cached") } else { String::new() };
    println!("{pass} passed, {fail} failed, {skip} skipped ({} suites, {:.2?}{reused})", results.len(), t0.elapsed());
    fail == 0
}

#[allow(clippy::too_many_arguments)]
fn run_suite(art: &Artifact, arts: &Arc<Vec<Artifact>>, root: &std::path::Path, opts: &Opts, remote: &evm::Remote, env: &Env, filter: &Filter, skip: &std::collections::HashSet<[u8; 4]>) -> Option<Suite> {
    let id = format!("{}:{}", art.file, art.name);
    let abi: JsonAbi = serde_json::from_value(art.abi.clone()).ok()?;
    let mut tests: Vec<&Function> = abi
        .functions()
        .filter(|f| crate::rewrite::is_test_name(&f.name))
        .filter(|f| !skip.contains(&f.selector().0))
        .filter(|f| filter.wants(&id, Some(&f.name)))
        .collect();
    if tests.is_empty() {
        return None;
    }
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    let t0 = Instant::now();
    let inline = inline_config(&root.join(&art.file));
    // results of an unchanged suite come from its last run
    let key = opts.cache.then(|| cache_key(art, opts, &inline));
    let old: Option<Entry> = key
        .as_ref()
        .and_then(|k| serde_json::from_slice::<Entry>(&std::fs::read(entry_path(k)).ok()?).ok())
        .filter(|e| e.deps.iter().all(|(k, v)| evm::read_now(k, arts, root).as_ref() == Some(v)));
    let mut cached = vec![];
    if let Some(e) = &old {
        tests.retain(|f| match e.results.iter().find(|r| r.name == sig(f)) {
            Some(r) => {
                cached.push(r.clone());
                false
            }
            None => true,
        });
    }
    let n = cached.len();
    if tests.is_empty() {
        return Some(Suite { id, results: cached, elapsed: t0.elapsed(), cached: n });
    }
    let reads = key.as_ref().map(|_| Arc::new(std::sync::Mutex::new(evm::Reads::default())));
    let mut results = execute(art, arts, root, opts, remote, env, &id, &abi, tests, &inline, reads.clone());
    if let (Some(k), Some(r)) = (key, reads) {
        let r = r.lock().unwrap();
        if !r.impure {
            let mut e = old.unwrap_or_default();
            e.deps.extend(r.deps.clone());
            e.results.retain(|x| !results.iter().any(|y| y.name == x.name));
            e.results.extend(results.iter().cloned());
            let path = entry_path(&k);
            std::fs::create_dir_all(path.parent().unwrap()).ok();
            let tmp = path.with_extension(format!("{}", std::process::id()));
            if std::fs::write(&tmp, serde_json::to_vec(&e).unwrap()).is_ok() {
                std::fs::rename(&tmp, &path).ok();
            }
        }
    }
    results.extend(cached);
    results.sort_by(|a, b| a.name.cmp(&b.name));
    Some(Suite { id, results, elapsed: t0.elapsed(), cached: n })
}

/// Deploys the suite, runs setUp, then `tests` on copies of the post-setUp state.
#[allow(clippy::too_many_arguments)]
fn execute(art: &Artifact, arts: &Arc<Vec<Artifact>>, root: &std::path::Path, opts: &Opts, remote: &evm::Remote, env: &Env, id: &str, abi: &JsonAbi, tests: Vec<&Function>, inline: &HashMap<String, Inline>, reads: Option<Arc<std::sync::Mutex<evm::Reads>>>) -> Vec<TestResult> {
    let mut cheats = Cheats::new(arts.clone(), root.to_path_buf(), opts.access.clone());
    cheats.reads = reads;
    // --fork is fork 0
    if let Some(f) = &remote.0 {
        cheats.forks.push(f.clone());
        cheats.active_fork = Some(0);
    }
    cheats.track_creates = true;
    cheats.allow_internal_revert = opts.allow_internal_revert;
    cheats.solc = opts.solc.clone();
    cheats.isolate = opts.isolate;
    cheats.profile = art.profile;
    let mut env = env.clone();
    env.block.gas_limit = opts.gas_limit;
    let (mut db, r) = setup(evm::base_db(remote.clone()), &mut env, &mut cheats, arts, art);
    let fail_all = |why: String, logs: Vec<String>| -> Vec<TestResult> {
        tests.iter().map(|f| TestResult { name: sig(f), status: Status::Fail(why.clone()), gas: 0, runs: None, logs: logs.clone(), args: None, trace: vec![] }).collect()
    };
    let Some(addr) = r.address.filter(|_| r.ok) else {
        return fail_all(format!("constructor reverted: {}", decode::reason(&r.output)), cheats.console);
    };
    if abi.functions().any(|f| f.name == "setUp" && f.inputs.is_empty()) {
        let r;
        (db, r) = evm::transact(db, &mut env, &mut cheats, SENDER, TxKind::Call(addr), Bytes::from_static(&[0x0a, 0x92, 0x54, 0xe4]), U256::ZERO, true);
        let pending = cheats.finish();
        if r.output.as_ref() == SKIP_MAGIC {
            return tests.iter().map(|f| TestResult { name: sig(f), status: Status::Skip("skipped in setUp".into()), gas: 0, runs: None, logs: vec![], args: None, trace: vec![] }).collect();
        }
        if !r.ok || pending.is_some() {
            let why = if r.ok { pending.unwrap() } else { decode::reason(&r.output) };
            return fail_all(format!("setUp() failed: {why}"), cheats.console);
        }
    }
    cheats.console.clear();
    cheats.track_creates = false;
    cheats.names.insert(addr, art.name.clone());
    cheats.ids.insert(addr, format!("{}:{}", art.file, art.name));
    // the post-setUp state, shared read-only: each test/run executes on an overlay of it
    let db = Arc::new(db);
    // the fuzzer's dictionary: constants in the code of the contracts setUp made, and of the test itself
    let dict = Dict { targets: constants(&db, cheats.names.keys().copied().filter(|a| *a != addr)), own: constants(&db, std::iter::once(addr)) };
    let (invariants, tests): (Vec<&Function>, Vec<&Function>) = tests.into_iter().partition(|f| crate::rewrite::is_invariant_name(&f.name));
    let mut results: Vec<TestResult> = tests
        .par_iter()
        .map(|f| {
            let t = Instant::now();
            let runs = inline.get(&f.name).and_then(|i| i.fuzz_runs).unwrap_or(opts.fuzz_runs);
            // `forge-config: default.isolate`: this test's calls are transactions of their own (setUp's weren't)
            let mut own;
            let cheats = match inline.get(&f.name).and_then(|i| i.isolate) {
                Some(isolate) => {
                    own = cheats.clone();
                    own.isolate = isolate;
                    &own
                }
                None => &cheats,
            };
            let r = legacy_fail(run_test(f, addr, &db, &env, cheats, runs, opts, &dict), f);
            if std::env::var_os("PLAIN_DEBUG").is_some() {
                eprintln!("test {:.1?} {}:{}", t.elapsed(), id, r.name);
            }
            r
        })
        .collect();
    if !invariants.is_empty() {
        // invariants with the same settings share their call sequences
        let mut groups: Vec<((u32, u32, bool), Vec<&Function>)> = vec![];
        for f in invariants {
            let i = inline.get(&f.name).copied().unwrap_or_default();
            let (runs, depth, revert) = opts.invariant;
            let key = (i.inv_runs.unwrap_or(runs), i.inv_depth.unwrap_or(depth), i.fail_on_revert.unwrap_or(revert));
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => g.1.push(f),
                None => groups.push((key, vec![f])),
            }
        }
        for (key, invs) in groups {
            results.extend(run_invariants(&invs, key, addr, &db, &env, &cheats, opts, abi, &dict));
        }
        results.sort_by(|a, b| a.name.cmp(&b.name));
    }
    results
}

/// Values for the fuzzer to try, from bytecode: the code under test, and the test's own (forge's dictionary).
#[derive(Default)]
struct Dict {
    targets: Vec<U256>,
    own: Vec<U256>,
}

/// The values PUSHed by the code at `addrs`, less what every contract pushes: function selectors, event topics,
/// the cheatcode and console addresses.
fn constants(db: &Db, addrs: impl Iterator<Item = Address>) -> Vec<U256> {
    let noise: std::collections::HashSet<U256> = decode::tables()
        .map(|t| t.functions.keys().chain(t.errors.keys()).map(|s| U256::from_be_slice(s)).chain(t.events.keys().map(|e| U256::from_be_bytes(e.0))).collect())
        .unwrap_or_default();
    let skip = |v: &U256| noise.contains(v) || [evm::CHEATS, evm::CONSOLE, evm::CREATE2_DEPLOYER].iter().any(|a| U256::from_be_slice(a.as_slice()) == *v);
    let mut out = std::collections::BTreeSet::new();
    for a in addrs {
        let Some(code) = db.cache.accounts.get(&a).and_then(|acc| acc.info.code.as_ref()).map(|c| c.original_bytes()) else { continue };
        let mut i = 0;
        while i < code.len() {
            let op = code[i];
            if (0x60..=0x7f).contains(&op) {
                let n = (op - 0x5f) as usize;
                if let Some(b) = code.get(i + 1..i + 1 + n) {
                    let v = U256::from_be_slice(b);
                    // most pushes are jump targets: offsets of a JUMPDEST in this very code, not constants
                    let target = n <= 2 && v < U256::from(code.len()) && code[v.to::<usize>()] == 0x5b;
                    if !skip(&v) && !target {
                        out.insert(v);
                    }
                }
                i += n;
            }
            i += 1;
            if out.len() >= 8192 {
                break;
            }
        }
    }
    out.into_iter().collect()
}

/// `/// forge-config: default.fuzz.runs = N` (and `invariant.runs`, `invariant.depth`, `invariant.fail-on-revert`)
/// natspec above a test function.
fn inline_config(path: &std::path::Path) -> HashMap<String, Inline> {
    let mut out = HashMap::new();
    let Ok(src) = std::fs::read_to_string(path) else { return out };
    let mut pending: Option<Inline> = None;
    for line in src.lines() {
        let t = line.trim();
        if let Some(cfg) = t.split("forge-config:").nth(1) {
            if let Some((k, v)) = cfg.split_once('=') {
                let (k, v) = (k.trim().trim_start_matches("default."), v.trim());
                let p = pending.get_or_insert_default();
                match k {
                    "fuzz.runs" => p.fuzz_runs = v.parse().ok(),
                    "invariant.runs" => p.inv_runs = v.parse().ok(),
                    "invariant.depth" => p.inv_depth = v.parse().ok(),
                    "invariant.fail-on-revert" | "invariant.fail_on_revert" => p.fail_on_revert = v.parse().ok(),
                    "isolate" => p.isolate = v.parse().ok(),
                    _ => {}
                }
            }
        } else if let Some(rest) = t.strip_prefix("function ") {
            if let Some(i) = pending.take() {
                out.insert(rest.split('(').next().unwrap_or_default().trim().to_string(), i);
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
    } else if failed_slot(&db) || c.failed {
        why = Some("assertion failed".into());
    }
    (r, c, why)
}

#[allow(clippy::too_many_arguments)]
fn run_test(f: &Function, addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats, fuzz_runs: u32, opts: &Opts, dict: &Dict) -> TestResult {
    let name = sig(f);
    let mut res = TestResult { name, status: Status::Pass, gas: 0, runs: None, logs: vec![], args: None, trace: vec![] };
    let sel = f.selector();
    if f.inputs.is_empty() {
        let data = Bytes::copy_from_slice(sel.as_slice());
        let (r, c, why) = exec_once(data.clone(), addr, db, env, cheats);
        res.gas = r.gas;
        res.logs = c.console;
        res.status = match why {
            _ if r.output.as_ref() == SKIP_MAGIC => Status::Skip("skipped".into()),
            _ if r.output.as_ref() == ASSUME_MAGIC => Status::Fail("vm.assume rejected the inputs of a test that takes none".into()),
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
            // runs seeded side by side start out correlated: a few steps in, they aren't
            (0..4).for_each(|_| _ = rng.next());
            loop {
                let vals: Vec<DynSolValue> = types.iter().map(|t| fuzz_value(t, &mut rng, dict)).collect();
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
    let skipped = outcomes.iter().any(|o| o.3);
    // a run that failed fails the test, whatever other runs did
    if let Some((_, _, Some((why, args, logs, data)), _)) = outcomes.into_iter().filter(|o| o.2.is_some()).min_by_key(|o| o.0) {
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
    } else if skipped {
        res.status = Status::Skip("skipped".into());
    }
    res.runs = Some(runs);
    res.gas = gas_sum / runs.max(1) as u64;
    res
}

/// `testFail*`: forge 1.0 dropped it, because a test that passes whenever its body fails also passes when it
/// fails for the wrong reason. It fails, as in forge, with what to use instead.
fn legacy_fail(mut r: TestResult, f: &Function) -> TestResult {
    if f.name.starts_with("testFail") {
        r.status = Status::Fail("testFail* isn't supported (forge 1.0 removed it, since it passes whenever the body fails, for any reason): name it test_… and use vm.expectRevert".into());
        r.args = None;
        r.trace.clear();
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

/// Random value biased towards edge cases and, half the time, a constant from the code (mostly the code under test).
fn fuzz_value(ty: &DynSolType, r: &mut Rng, dict: &Dict) -> DynSolValue {
    if (!dict.targets.is_empty() || !dict.own.is_empty()) && r.next() % 2 == 0 {
        let pool = if !dict.targets.is_empty() && (dict.own.is_empty() || r.next() % 3 != 0) { &dict.targets } else { &dict.own };
        let v = pool[(r.next() % pool.len() as u64) as usize];
        let v = match r.next() % 8 {
            0 => v.wrapping_add(U256::from(1)),
            1 => v.wrapping_sub(U256::from(1)),
            _ => v,
        };
        match ty {
            DynSolType::Uint(bits) => return DynSolValue::Uint(if *bits == 256 { v } else { v & ((U256::from(1) << *bits) - U256::from(1)) }, *bits),
            DynSolType::Int(bits) => {
                let raw = I256::from_raw(v << (256 - *bits));
                return DynSolValue::Int(if *bits == 256 { raw } else { raw.asr(256 - *bits) }, *bits);
            }
            DynSolType::Address => return DynSolValue::Address(Address::from_word(v.into())),
            DynSolType::FixedBytes(32) => return DynSolValue::FixedBytes(v.into(), 32),
            _ => {}
        }
    }
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
            DynSolValue::Array((0..n).map(|_| fuzz_value(t, r, dict)).collect())
        }
        DynSolType::FixedArray(t, n) => DynSolValue::FixedArray((0..*n).map(|_| fuzz_value(t, r, dict)).collect()),
        DynSolType::Tuple(ts) => DynSolValue::Tuple(ts.iter().map(|t| fuzz_value(t, r, dict)).collect()),
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

/// forge-style invariant testing: random call sequences against targets, invariants checked before the first call
/// and after every call. What plain can't target as forge would (artifact- or interface-based targeting, targets
/// without a compiled ABI) fails the invariant rather than silently fuzzing less.
#[allow(clippy::too_many_arguments)]
fn run_invariants(invs: &[&Function], (runs, depth, fail_on_revert): (u32, u32, bool), addr: Address, db: &Arc<Db>, env: &Env, cheats: &Cheats, opts: &Opts, abi: &JsonAbi, dict: &Dict) -> Vec<TestResult> {
    let result = |status: Status, trace: Vec<String>| -> Vec<TestResult> {
        invs.iter().map(|f| TestResult { name: sig(f), status: status.clone(), gas: 0, runs: Some(runs), logs: vec![], args: None, trace: trace.clone() }).collect()
    };
    let fail = |why: String| result(Status::Fail(why), vec![]);
    // the ABI of what's at an address: the artifact it was deployed from, never one that merely shares its name
    let abi_of = |a: &Address| -> Option<JsonAbi> {
        let id = cheats.ids.get(a)?;
        let art = cheats.artifacts.iter().find(|x| x.shard == 0 && format!("{}:{}", x.file, x.name) == *id)?;
        serde_json::from_value(art.abi.clone()).ok()
    };
    let has = |name: &str| abi.functions().any(|f| f.name == name && f.inputs.is_empty());
    for (getter, ty) in [("targetArtifacts", "string[]"), ("targetArtifactSelectors", "(string,bytes4[])[]"), ("targetInterfaces", "(address,string[])[]"), ("excludeArtifacts", "string[]")] {
        if has(getter) && !view(addr, db, env, cheats, &format!("{getter}()"), ty).is_empty() {
            return fail(format!("{getter}() isn't supported yet, so this invariant can't be fuzzed as written"));
        }
    }
    let addrs = |sig: &str| view(addr, db, env, cheats, sig, "address[]").into_iter().filter_map(|v| v.as_address()).collect::<Vec<_>>();
    let excluded = addrs("excludeContracts()");
    let explicit = addrs("targetContracts()");
    let mut targets = explicit.clone();
    if targets.is_empty() {
        targets = cheats.names.keys().copied().filter(|a| *a != addr).collect();
        targets.sort();
    }
    targets.retain(|a| !excluded.contains(a));
    let selectors = |sig: &str| -> Vec<(Address, Vec<[u8; 4]>)> {
        view(addr, db, env, cheats, sig, "(address,bytes4[])[]")
            .into_iter()
            .filter_map(|v| match v {
                DynSolValue::Tuple(t) => Some((t[0].as_address()?, t[1].as_array()?.iter().filter_map(|s| s.as_fixed_bytes().map(|(b, _)| b[..4].try_into().unwrap())).collect())),
                _ => None,
            })
            .collect()
    };
    let selected = selectors("targetSelectors()");
    let unselected = selectors("excludeSelectors()");
    let mut calls: Vec<(Address, Function)> = vec![];
    for t in &targets {
        let Some(abi) = abi_of(t) else {
            if explicit.contains(t) {
                return fail(format!("target {t} has no compiled ABI here (only src/, test/ and script/ are compiled to artifacts)"));
            }
            continue;
        };
        let only = selected.iter().find(|(a, _)| a == t).map(|(_, s)| s);
        let not = unselected.iter().find(|(a, _)| a == t).map(|(_, s)| s);
        for f in abi.functions() {
            let mutating = !matches!(f.state_mutability, alloy_json_abi::StateMutability::View | alloy_json_abi::StateMutability::Pure);
            let sel = f.selector().0;
            if only.map_or(mutating, |s| s.contains(&sel)) && not.is_none_or(|s| !s.contains(&sel)) {
                calls.push((*t, f.clone()));
            }
        }
    }
    let senders = addrs("targetSenders()");
    let excluded_senders = addrs("excludeSenders()");
    if calls.is_empty() {
        return fail("nothing to fuzz: no target contract with functions to call (forge: \"No contracts to fuzz\")".into());
    }
    // the state setUp left has to satisfy the invariants too
    for f in invs {
        let (r, c, why) = exec_once(Bytes::copy_from_slice(&f.selector()[..]), addr, db, env, cheats);
        if let Some(why) = why.filter(|_| r.output.as_ref() != SKIP_MAGIC) {
            let mut out = fail(String::new());
            for (res, g) in out.iter_mut().zip(invs) {
                res.status = if g.name == f.name { Status::Fail(format!("invariant broken before any call: {why}")) } else { Status::Skip("not checked (another invariant broke)".into()) };
                res.logs = c.console.clone();
            }
            return out;
        }
    }
    let after = has("afterInvariant");
    let types: Vec<Vec<DynSolType>> = calls.iter().map(|(_, f)| f.inputs.iter().filter_map(|p| p.resolve().ok()).collect()).collect();
    let stop = std::sync::atomic::AtomicBool::new(false);
    // each run: Some((run, invariant index or MAX for all, reason, calls made, the last calls)) on failure
    let failures: Vec<(u32, usize, String, usize, Vec<String>)> = (0..runs)
        .into_par_iter()
        .filter_map(|run| {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let mut rng = Rng((opts.seed ^ (run as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15)) | 1);
            (0..4).for_each(|_| _ = rng.next());
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
                let vals: Vec<DynSolValue> = types[i].iter().map(|t| fuzz_value(t, &mut rng, dict)).collect();
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
                vm.cheats().failed = false;
                let r = vm.call(sender, TxKind::Call(*target), data.into(), U256::ZERO, true);
                vm.cheats().finish();
                seq.push((i, vals, sender, r.ok));
                // a handler that calls fail() (the global failure flag) breaks the run as an assertion would
                if vm.cheats().failed {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    let (n, lines) = tail(&seq, vm.cheats());
                    return Some((run, usize::MAX, "a call failed an assertion (fail())".into(), n, lines));
                }
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
            // forge's afterInvariant(): checked at the end of every run
            if after {
                vm.cheats().failed = false;
                let r = vm.call(SENDER, TxKind::Call(addr), Bytes::copy_from_slice(&alloy_primitives::keccak256("afterInvariant()")[..4]), U256::ZERO, false);
                if !r.ok || vm.cheats().failed {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    let why = if r.ok { "assertion failed".into() } else { decode::reason(&r.output) };
                    let (n, lines) = tail(&seq, vm.cheats());
                    return Some((run, usize::MAX, format!("afterInvariant(): {why}"), n, lines));
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

#[cfg(test)]
mod tests {
    #[test]
    fn match_path() {
        use super::path_matches as m;
        // forge's globs: `*` crosses `/`, `{a,b}` alternatives (veydrift passes 50 files as one), a file name
        // containing the pattern, "./" ignored
        assert!(m("test/{A.t.sol,B.t.sol}", "test/B.t.sol") && !m("test/{A.t.sol,B.t.sol}", "test/C.t.sol"));
        assert!(m("test/*.t.sol", "test/sub/A.t.sol") && m("**/A.t.sol", "test/sub/A.t.sol"));
        assert!(m("A.t.sol", "test/A.t.sol") && m("./test/A.t.sol", "test/A.t.sol"));
        assert!(m("test/[AB].t.sol", "test/B.t.sol") && !m("test/[!AB].t.sol", "test/B.t.sol") && m("test/A?t.sol", "test/A.t.sol"));
        let f = super::Filter { path: Some("test/{A,B}.t.sol".into()), test: Some("Fuzz".into()), ..Default::default() };
        assert!(f.wants("test/A.t.sol:ATest", Some("testFuzzX")) && f.wants("test/A.t.sol:ATest", None));
        assert!(!f.wants("test/A.t.sol:ATest", Some("testX")) && !f.wants("test/C.t.sol:CTest", Some("testFuzzX")));
    }
}
