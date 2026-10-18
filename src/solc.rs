//! solc management + parallel, cached compilation.
use crate::project::{Graph, Project};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Serialize, Deserialize, Clone)]
pub struct Artifact {
    pub name: String,
    pub file: String,
    pub abi: Value,
    pub bytecode: String,
    pub deployed: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub links: Value,
    /// test contracts of a sharded file: one artifact per unit (see `UnitMeta`), each running its own tests
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shard: u32,
    /// tests this unit runs (sharded files); tests every unit has (inherited) run on unit 0
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<String>,
    /// artifacts this test contract deploys via the getCode cheatcode (see rewrite.rs)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deploys: Vec<String>,
    /// a test or script contract compiled from rewritten sources (not solc's output for the file as written)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewritten: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    key: String,
    files: BTreeMap<String, CacheEntry>,
    /// content hash -> interface hash (see Unit::interface_hash)
    #[serde(default)]
    iface: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    hash: String,
    contracts: Vec<Artifact>,
    /// solc CPU time last build (ms), to schedule and shard the next one
    #[serde(default)]
    ms: u64,
    /// compiled from rewritten test sources, which embed no other contract's code: `hash` covers the
    /// full sources of `code` (the file, what it inherits or embeds) and only the interfaces of the
    /// rest of its imports, so editing a function body in src/ doesn't recompile the tests
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    code: Vec<String>,
    /// a big test file compiled as units: hash of everything but its test functions (and of its imports)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    shared: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    units: Vec<UnitMeta>,
}

/// One unit of a sharded test file: the file with only some of its test functions. While nothing but test
/// bodies changes, a unit stays valid for the tests it owns whose text is unchanged; edited and new tests
/// compile into a small "hot" unit, so editing one test recompiles one test.
#[derive(Serialize, Deserialize, Clone)]
struct UnitMeta {
    owns: Vec<String>,
    /// hash of each test function in the unit's source, as compiled
    tests: BTreeMap<String, String>,
    hot: bool,
    ms: u64,
}

impl CacheEntry {
    /// shards the file was compiled as last time (1 if not sharded)
    fn shards(&self) -> u32 {
        self.contracts.iter().map(|a| a.shard + 1).max().unwrap_or(1)
    }
}

/// Files compiled from rewritten sources: tests and scripts. A script's own bytecode never goes on
/// chain, only the transactions it broadcasts, and deploying from artifacts sends the same ones
/// (veydrift's Deploy.s.sol: 40 byte-identical transactions).
fn rewritten(p: &Project, f: &str) -> bool {
    f.starts_with(&format!("{}/", p.dirs[1])) || f.ends_with(".t.sol") || f.starts_with(&format!("{}/", p.dirs[2]))
}

/// Artifacts shared by every checkout of the same sources (git worktrees, fresh clones), by content
/// hash under ~/.plain/cache: a new checkout skips compiling what another one already compiled.
struct Store(PathBuf);

impl Store {
    fn new(key: &str) -> Store {
        Store(home().join("cache").join(&key[..16]))
    }
    fn path(&self, hash: &str) -> PathBuf {
        self.0.join(format!("{hash}.json"))
    }
    fn get(&self, hash: &str) -> Option<CacheEntry> {
        let path = self.path(hash);
        let e = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        // recently used entries survive gc
        std::fs::File::options().append(true).open(&path).and_then(|f| f.set_modified(std::time::SystemTime::now())).ok();
        Some(e)
    }
    fn put(&self, hash: &str, e: &CacheEntry) {
        std::fs::create_dir_all(&self.0).ok();
        let tmp = self.0.join(format!(".{hash}.{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec(e).unwrap()).is_ok() {
            std::fs::rename(&tmp, self.path(hash)).ok();
        }
        // once a day, drop entries unused for two weeks
        let mark = home().join("cache/.gc");
        let stale = |m: &std::fs::Metadata, days: u64| m.modified().ok().and_then(|t| t.elapsed().ok()).is_none_or(|d| d.as_secs() > days * 86400);
        if std::fs::metadata(&mark).is_ok_and(|m| !stale(&m, 1)) {
            return;
        }
        std::fs::write(&mark, "").ok();
        for dir in std::fs::read_dir(home().join("cache")).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
                if f.metadata().is_ok_and(|m| m.is_file() && stale(&m, 14)) {
                    std::fs::remove_file(f.path()).ok();
                }
            }
        }
    }
}

/// Closure hash of a test build: full source hashes for `code`, interface hashes for the rest.
fn dep_hash(g: &Graph, f: &str, code: &[String], own: &HashMap<String, String>, iface: &mut BTreeMap<String, String>) -> String {
    g.closure_hash(f, &mut |u| {
        let o = &own[u];
        if code.iter().any(|c| c == u) {
            return o.clone();
        }
        iface
            .entry(o.clone())
            .or_insert_with(|| crate::rewrite::Unit::parse(&g.files[u].content).map_or_else(|| o.clone(), |x| x.interface_hash(&g.files[u].content)))
            .clone()
    })
}

/// What `build` compiles: everything, what tests need (scripts only when a test imports them), or what
/// deploying one contract or script needs (with what its rewritten code deploys from artifacts).
#[derive(PartialEq)]
pub enum Scope {
    All,
    /// what `plain test [filter]` runs: with a filter, only test files with a matching suite or test
    Tests(Option<String>),
    Deploy(String),
}

/// A random value of this machine's own in the build cache's key, so that a `.plain/cache.json` committed to a repo
/// (or copied from anywhere else) can never pass for this machine's compile of the sources it describes.
fn salt() -> String {
    let path = home().join("salt");
    let read = || std::fs::read_to_string(&path).ok().filter(|s| s.len() == 64);
    if let Some(s) = read() {
        return s;
    }
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).unwrap_or_else(|e| crate::die(&format!("no randomness from the OS: {e}")));
    std::fs::create_dir_all(home()).ok();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, hex::encode(b)).is_ok() {
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600)).ok();
        std::fs::rename(&tmp, &path).ok();
    }
    read().unwrap_or_else(|| hex::encode(b))
}

pub fn home() -> PathBuf {
    let h = std::env::var("PLAIN_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or(".".into())).join(".plain"));
    h
}

fn platform() -> &'static str {
    // ponytail: no official linux-arm64 solc builds; add a mirror if someone needs it
    match std::env::consts::OS {
        "macos" => "macosx-amd64",
        "windows" => "windows-amd64",
        _ => "linux-amd64",
    }
}

/// A solc version as a path component can't be anything but digits and dots: versions come from config files and
/// deploy records, which a cloned repo controls.
fn valid(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 4 && p.bytes().all(|b| b.is_ascii_digit()))
}

fn solc_path(v: &str) -> PathBuf {
    // reuse foundry's svm cache when present
    let h = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    [h.join("Library/Application Support/svm"), h.join(".svm"), h.join(".local/share/svm")]
        .into_iter()
        .map(|d| d.join(v).join(format!("solc-{v}")))
        .find(|p| p.exists())
        .unwrap_or_else(|| home().join("solc").join(format!("solc-{v}")))
}

fn installed() -> Vec<semver::Version> {
    let mut out: Vec<_> = std::fs::read_dir(home().join("solc"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("solc-")?.parse().ok())
        .collect();
    out.sort();
    out
}

fn list() -> Value {
    let cached = home().join("solc").join("list.json");
    let fresh = std::fs::metadata(&cached)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default().as_secs() < 86400);
    if !fresh {
        let url = format!("https://binaries.soliditylang.org/{}/list.json", platform());
        if let Ok(mut r) = ureq::get(&url).call() {
            if let Ok(s) = r.body_mut().read_to_string() {
                std::fs::create_dir_all(cached.parent().unwrap()).ok();
                std::fs::write(&cached, s).ok();
            }
        }
    }
    std::fs::read_to_string(&cached)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| crate::die("cannot fetch solc release list (offline?)"))
}

/// "0.8.28+commit.7893614a", as explorers want it.
pub fn long_version(v: &str) -> String {
    let out = Command::new(ensure(v)).arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let full = out.lines().find_map(|l| l.strip_prefix("Version: ")).unwrap_or(v);
    // drop the platform suffix after the commit hash
    match full.find("+commit.") {
        Some(i) => full[..(i + 16).min(full.len())].to_string(),
        None => full.to_string(),
    }
}

fn ensure(v: &str) -> PathBuf {
    if !valid(v) {
        crate::die(&format!("{v:?} is not a solc version (like 0.8.28)"));
    }
    let p = solc_path(v);
    if p.exists() {
        return p;
    }
    let l = list();
    let build = l["builds"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["version"] == v && b["prerelease"].is_null()))
        .unwrap_or_else(|| crate::die(&format!("solc {v} not available for {}", platform())));
    let url = format!("https://binaries.soliditylang.org/{}/{}", platform(), build["path"].as_str().unwrap());
    eprintln!("downloading solc {v}");
    let bytes = ureq::get(&url)
        .call()
        .and_then(|mut r| r.body_mut().with_config().limit(500 << 20).read_to_vec())
        .unwrap_or_else(|e| crate::die(&format!("download {url}: {e}")));
    use sha2::Digest;
    let sum = format!("0x{}", hex::encode(sha2::Sha256::digest(&bytes)));
    if build["sha256"].as_str() != Some(sum.as_str()) {
        crate::die(&format!("solc {v}: checksum mismatch"));
    }
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // a name of its own: two plain processes may download the same version at once
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::rename(&tmp, &p).unwrap();
    p
}

/// Solidity pragma -> semver requirement(s); `||` alternatives allowed.
fn pragma_matches(pragma: &str, v: &semver::Version) -> bool {
    pragma.split("||").any(|alt| {
        let mut parts: Vec<String> = Vec::new();
        let mut pending = String::new();
        for tok in alt.split_whitespace() {
            pending.push_str(tok);
            if pending.chars().last().is_some_and(|c| c.is_ascii_digit() || c == '*' || c == 'x') {
                let t = std::mem::take(&mut pending);
                parts.push(if t.starts_with(|c: char| c.is_ascii_digit()) { format!("={t}") } else { t });
            }
        }
        semver::VersionReq::parse(&parts.join(",")).is_ok_and(|r| r.matches(v))
    })
}

/// Highest installed version satisfying every pragma, else highest released one.
fn pick_version(p: &Project, g: &Graph) -> String {
    if let Some(v) = &p.solc {
        return v.clone();
    }
    let pragmas: Vec<&str> = g.files.values().filter_map(|f| f.pragma.as_deref()).collect();
    let ok = |v: &semver::Version| pragmas.iter().all(|pr| pragma_matches(pr, v));
    if let Some(v) = installed().into_iter().rev().find(ok) {
        return v.to_string();
    }
    let l = list();
    let mut all: Vec<semver::Version> =
        l["releases"].as_object().into_iter().flatten().filter_map(|(k, _)| k.parse().ok()).collect();
    all.sort();
    all.into_iter()
        .rev()
        .find(ok)
        .map(|v| v.to_string())
        .unwrap_or_else(|| crate::die("no single solc version satisfies all pragmas; set `solc = \"x.y.z\"` in plain.toml"))
}

pub struct Output {
    pub artifacts: Vec<Artifact>,
    pub compiled: usize,
    pub solc: String,
    /// the EVM version the code targets (and tests run under)
    pub evm: String,
}

/// EVM versions as solc names them, oldest first.
const EVMS: [&str; 14] = ["homestead", "tangerineWhistle", "spuriousDragon", "byzantium", "constantinople", "petersburg", "istanbul", "berlin", "london", "paris", "shanghai", "cancun", "prague", "osaka"];

/// The EVM version builds target, set once the compiler is picked (see `target_evm`).
static EVM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The EVM version to compile for, as forge picks it: the configured evm_version (forge's default: osaka), lowered
/// to the newest this solc supports, so the bytecode is what forge would build. Tests run under the configured rules.
fn target_evm(p: &Project, version: &str) -> String {
    let want = p.evm_version.clone().unwrap_or("osaka".into());
    let path = home().join("solc").join(format!("solc-{version}.evms"));
    let known = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        let help = Command::new(ensure(version)).arg("--help").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let section: String = help.split("--evm-version").nth(1).unwrap_or_default().lines().take_while(|l| !l.trim_start().starts_with("--") || l.contains("Select")).collect::<Vec<_>>().join(" ");
        let words: Vec<&str> = section.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| EVMS.contains(w)).collect();
        let list = words.join(" ");
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, &list).ok();
        list
    });
    let newest = EVMS.iter().rev().find(|e| known.split(' ').any(|k| k == **e)).copied().unwrap_or("cancun");
    let rank = |e: &str| EVMS.iter().position(|x| *x == e);
    match (rank(&want), rank(newest)) {
        (Some(w), Some(n)) if w > n => newest.to_string(),
        _ => want,
    }
}

pub fn build(p: &Project, force: bool, scope: Scope) -> Output {
    let files = p.project_files();
    if files.is_empty() {
        crate::die(&format!("no .sol files under {}", p.dirs.join(", ")));
    }
    let t0 = std::time::Instant::now();
    let phase = |name: &str| {
        if std::env::var_os("PLAIN_DEBUG").is_some() {
            eprintln!("build: {name} at {:.2?}", t0.elapsed());
        }
    };
    let g = Graph::build(p, &files);
    let version = pick_version(p, &g);
    let evm = EVM.get_or_init(|| target_evm(p, &version)).clone();
    let settings = settings(p);
    let key = {
        use sha2::Digest;
        // bump the suffix when rewrites change what gets compiled
        hex::encode(sha2::Sha256::digest(format!("{}{version}{settings}{:?}rw5", salt(), p.dirs).as_bytes()))
    };
    let cache_path = p.root.join(".plain/cache.json");
    let mut cache: Cache = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|c: &Cache| c.key == key)
        .unwrap_or_default();
    cache.key = key;
    let own = g.own_hashes();
    let full: HashMap<String, String> = g.files.keys().map(|f| (f.clone(), g.closure_hash(f, &mut |u| own[u].clone()))).collect();
    cache.files.retain(|f, _| g.files.contains_key(f));
    cache.iface.retain(|h, _| own.values().any(|o| o == h));
    let script = format!("{}/", p.dirs[2]);
    let wanted: Vec<&String> = match scope {
        Scope::All => files.iter().collect(),
        Scope::Tests(filter) => {
            // a test file matches by suite id ("file:Contract") or by the name of a test it has, its own or
            // inherited from test files it imports
            let tests_in = |u: &str| -> Vec<String> {
                let src = &g.files[u].content;
                crate::rewrite::Unit::parse(src).map(|x| x.funcs.iter().filter(|f| crate::rewrite::is_test_name(&f.name)).map(|f| f.name.clone()).collect()).unwrap_or_default()
            };
            let matches = |f: &String, flt: &str| {
                let own = crate::rewrite::Unit::parse(&g.files[f].content).map(|u| u.contracts.iter().any(|c| format!("{f}:{}", c.name).contains(flt))).unwrap_or(true);
                own || g.closure(f).iter().filter(|u| rewritten(p, u)).any(|u| tests_in(u).iter().any(|t| t.contains(flt)))
            };
            let mut need: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for f in files.iter().filter(|f| !f.starts_with(&script)) {
                // every src file too, filter or not: a test can deploy any contract by name (getCode, deployCode)
                if !rewritten(p, f) || filter.as_deref().is_none_or(|flt| matches(f, flt)) {
                    need.extend(g.closure(f));
                }
            }
            files.iter().filter(|f| need.contains(*f)).collect()
        }
        Scope::Deploy(t) => {
            let units: HashMap<&String, crate::rewrite::Unit> = g.files.par_iter().filter_map(|(f, n)| Some((f, crate::rewrite::Unit::parse(&n.content)?))).collect();
            let defines = |f: &String, name: &str| units.get(f).is_some_and(|u| u.contracts.iter().any(|c| c.name == name));
            let (tf, tn) = t.rsplit_once(':').map_or((t.as_str(), None), |(f, n)| (f, Some(n)));
            let mut stack: Vec<String> = files
                .iter()
                .filter(|f| match tn {
                    Some(n) => *f == tf && defines(f, n),
                    None if t.ends_with(".sol") => f.as_str() == t.as_str() || f.ends_with(&format!("/{t}")),
                    None => defines(f, t.as_str()),
                })
                .cloned()
                .collect();
            // the target, and every contract it can create: rewritten code deploys project contracts from
            // artifacts, and whatever goes on chain (OpenZeppelin proxies too) needs an artifact to verify
            let mut need = std::collections::BTreeSet::new();
            let mut roots = std::collections::BTreeSet::new();
            while let Some(f) = stack.pop() {
                if !roots.insert(f.clone()) {
                    continue;
                }
                for u in g.closure(&f) {
                    if need.insert(u.clone()) {
                        let src = &g.files[&u].content;
                        for name in units.get(&u).map(|x| x.created(src)).unwrap_or_default() {
                            stack.extend(g.files.keys().filter(|d| defines(d, &name)).cloned());
                        }
                    }
                }
            }
            // and every src file: a script can deploy any contract by name (getCode, deployCode)
            for f in files.iter().filter(|f| !rewritten(p, f)) {
                need.extend(g.closure(f));
            }
            let mut w: Vec<&String> = files.iter().filter(|f| need.contains(*f)).collect();
            w.extend(g.files.keys().filter(|f| roots.contains(*f) && !files.contains(f)));
            w
        }
    };
    let mut iface = std::mem::take(&mut cache.iface);
    let mut changed = |f: &String| {
        cache.files.get(f).is_none_or(|e| if e.code.is_empty() { e.hash != full[f] } else { !e.code.iter().all(|c| own.contains_key(c)) || e.hash != dep_hash(&g, f, &e.code, &own, &mut iface) })
    };
    // what this scope leaves uncompiled although its sources changed: its old artifacts are never handed out
    let stale: std::collections::HashSet<String> = cache.files.keys().filter(|f| !wanted.contains(f)).cloned().collect::<Vec<_>>().into_iter().filter(|f| changed(f)).collect();
    let dirty: Vec<&String> = wanted.into_iter().filter(|f| force || changed(f)).collect();

    let store = Store::new(&cache.key);
    let out_dir = p.root.join(".plain/out");
    // .plain/out holds solc's output as written: test and script contracts (rewritten) stay out
    let export = |arts: &[Artifact]| {
        std::fs::create_dir_all(&out_dir).unwrap();
        for a in arts.iter().filter(|a| a.shard == 0 && !a.rewritten) {
            if !a.bytecode.is_empty() || a.abi.as_array().is_some_and(|x| !x.is_empty()) {
                let j = json!({"contractName": a.name, "sourceName": a.file, "abi": a.abi, "bytecode": format!("0x{}", a.bytecode), "deployedBytecode": format!("0x{}", a.deployed)});
                std::fs::write(out_dir.join(format!("{}.json", a.name)), j.to_string()).ok();
            }
        }
    };
    let compiled = dirty.len();
    let dirty: Vec<&String> = dirty
        .into_iter()
        .filter(|f| {
            let hit = (!force)
                .then(|| store.get(&full[*f]))
                .flatten()
                .filter(|e| e.code.iter().all(|c| own.contains_key(c)))
                // never trade units for an unsharded build of the same source: the next edit would recompile them all
                .filter(|e| !e.units.is_empty() || cache.files.get(*f).is_none_or(|c| c.units.is_empty()));
            let Some(e) = hit else { return true };
            export(&e.contracts);
            cache.files.insert(f.to_string(), e);
            false
        })
        .collect();
    let reused = compiled - dirty.len();

    if !dirty.is_empty() {
        let solc = ensure(&version);
        let v: semver::Version = version.parse().unwrap_or(semver::Version::new(0, 8, 0));
        phase("plan");
        let plan = Plan::new(p, &g, &files, &dirty, &v, &cache, &own, force);
        phase("jobs");
        let mut results = run_jobs(&solc, &g, &plan.jobs, &settings, p, &plan.sources, &plan.gen_deps);
        // unoptimized code can run out of stack where optimized code doesn't: those compile as configured
        let retry: Vec<Job> = results.iter().filter(|r| r.0.fast && r.1.is_err()).map(|r| Job { fast: false, files: r.0.files.clone(), only: r.0.only.clone(), ..r.0 }).collect();
        if !retry.is_empty() {
            results.retain(|r| !(r.0.fast && r.1.is_err()));
            results.extend(run_jobs(&solc, &g, &retry, &settings, p, &plan.sources, &plan.gen_deps));
        }
        phase("collect");
        // a rewrite that broke a test file: compile it again from the original sources (also reports real errors)
        let broken: Vec<String> = {
            let mut b: Vec<String> = results.iter().filter(|r| r.1.is_err() && r.0.test).flat_map(|r| r.0.files.clone()).collect();
            b.sort();
            b.dedup();
            b
        };
        if !broken.is_empty() {
            if std::env::var_os("PLAIN_DEBUG").is_some() {
                eprintln!("rewrite fallback: {broken:?}");
            }
            results.retain(|r| !r.0.files.iter().any(|f| broken.contains(f)));
            let jobs: Vec<Job> = broken.iter().map(|f| Job { files: vec![f.clone()], shard: None, test: false, est: 0, fast: false, only: vec![] }).collect();
            results.extend(run_jobs(&solc, &g, &jobs, &settings, p, &HashMap::new(), &HashMap::new()));
        }
        let mut warnings = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        // (artifacts, ms, compiled from rewritten sources)
        let mut fresh: HashMap<String, (Vec<Artifact>, u64, bool)> = dirty.iter().map(|f| (f.to_string(), (vec![], 0, plan.units.contains_key(*f) && !broken.contains(f)))).collect();
        let mut unit_ms: HashMap<(String, usize), u64> = HashMap::new();
        for (job, r, ms) in results {
            if let Some((k, _)) = job.shard {
                unit_ms.insert((job.files[0].clone(), k), ms);
            }
            let total: u64 = job.files.iter().map(|f| plan.est.get(f).copied().unwrap_or(1)).sum::<u64>().max(1);
            for f in &job.files {
                if let Some(e) = fresh.get_mut(f) {
                    e.1 += ms * plan.est.get(f).copied().unwrap_or(1) / total;
                    e.2 = job.test;
                }
            }
            match r {
                Ok((arts, warns)) => {
                    warnings.extend(warns);
                    for mut a in arts {
                        if job.test {
                            a.deploys = plan.deploys.get(&(a.file.clone(), a.name.clone())).cloned().unwrap_or_default();
                            a.rewritten = plan.rewritten.contains(&(a.file.clone(), a.name.clone()));
                        }
                        if let Some((k, _)) = job.shard {
                            // units differ only in which tests they have: keep unit 0 whole, other units' test contracts
                            if k > 0 && !crate::test::is_test_contract(&a) {
                                continue;
                            }
                            a.shard = k as u32;
                            a.tests = plan.units.get(&job.files[0]).map(|u| u.1[k].0.owns.clone()).unwrap_or_default();
                        }
                        fresh.entry(a.file.clone()).or_default().0.push(a);
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        for w in &warnings {
            eprintln!("{w}");
        }
        if !errors.is_empty() {
            let mut seen = std::collections::BTreeSet::new();
            for e in errors {
                for line in e.split("\u{0}") {
                    if seen.insert(line.to_string()) {
                        eprintln!("{line}");
                    }
                }
            }
            crate::die("compilation failed");
        }
        for (f, (mut contracts, mut ms, test)) in fresh {
            let (shared, units) = match plan.units.get(&f).filter(|_| !broken.contains(&f)) {
                Some((shared, units)) => {
                    let mut metas = vec![];
                    for (i, (u, reuse)) in units.iter().enumerate() {
                        let mut m = u.clone();
                        match reuse {
                            // a unit from the previous build: its artifacts, now owning only its unchanged tests
                            Some(old) => contracts.extend(cache.files[&f].contracts.iter().filter(|a| a.shard == *old).map(|a| Artifact { shard: i as u32, tests: if crate::test::is_test_contract(a) { m.owns.clone() } else { vec![] }, ..a.clone() })),
                            // (a file compiled whole has no unit timing: it's the file's)
                            None => m.ms = unit_ms.get(&(f.clone(), i)).copied().unwrap_or(ms),
                        }
                        metas.push(m);
                    }
                    ms = metas.iter().map(|m| m.ms).sum();
                    (shared.clone(), metas)
                }
                None => (String::new(), vec![]),
            };
            contracts.sort_by_key(|a| a.shard);
            export(&contracts);
            let (hash, code) = match plan.code.get(&f).filter(|_| test) {
                Some(code) => (dep_hash(&g, &f, code, &own, &mut iface), code.clone()),
                None => (full[&f].clone(), vec![]),
            };
            let e = CacheEntry { hash, contracts, ms, code, shared, units };
            store.put(&full[&f], &e);
            cache.files.insert(f.clone(), e);
        }
    }
    if compiled > 0 {
        if reused > 0 && std::env::var_os("PLAIN_DEBUG").is_some() {
            eprintln!("reused {reused} files from {}", store.0.display());
        }
        cache.iface = iface;
        std::fs::create_dir_all(p.root.join(".plain")).ok();
        phase("write");
        std::fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();
        phase("done");
        gitignore(p);
    }
    Output {
        compiled: compiled - reused,
        artifacts: cache.files.into_iter().filter(|(f, _)| !stale.contains(f)).flat_map(|(_, e)| e.contracts).collect(),
        solc: version,
        // tests run under the configured rules, forge's default osaka when there are none
        evm: p.evm_version.clone().unwrap_or(if evm == "osaka" { evm } else { "osaka".into() }),
    }
}

/// One solc run: `files` selected for output; a single sharded file when `shard` is (k, of).
struct Job {
    files: Vec<String>,
    shard: Option<(usize, usize)>,
    /// compiled from rewritten test sources
    test: bool,
    est: u64,
    /// without the optimizer: a hot unit (a few edited tests) of test code that doesn't look at its own gas.
    /// solc takes a third of the time; only gas spent in the test contract itself differs
    fast: bool,
    /// contracts to compile, when not every contract in `files` (units after the first: the test contracts)
    only: Vec<String>,
}

struct Plan {
    jobs: Vec<Job>,
    /// source overrides: rewritten test files, and "<file>#<k>" for shard k of a sharded file
    sources: HashMap<String, String>,
    est: HashMap<String, u64>,
    /// (file, contract) -> artifact ids its rewritten code deploys
    deploys: HashMap<(String, String), Vec<String>>,
    /// rewritten file -> generated constructor-check units it imports (in `sources`)
    gen_deps: HashMap<String, Vec<String>>,
    /// test-job file -> files whose code compiles into it (itself, bases, embedded contracts)
    code: HashMap<String, Vec<String>>,
    /// (file, contract) of test and script contracts, compiled from rewritten sources
    rewritten: std::collections::HashSet<(String, String)>,
    /// sharded test files: shared hash and units, each reusing the previous build's unit (by index) or compiled
    units: HashMap<String, (String, Vec<(UnitMeta, Option<u32>)>)>,
}

impl Plan {
    fn new(p: &Project, g: &Graph, files: &[String], dirty: &[&String], v: &semver::Version, cache: &Cache, own_hash: &HashMap<String, String>, force: bool) -> Plan {
        use crate::rewrite::{self, Unit};
        let workers = std::env::var("PLAIN_JOBS").ok().and_then(|j| j.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)).max(1);
        let is_test = |f: &str| rewritten(p, f);
        let units: HashMap<&str, Unit> = g.files.par_iter().filter_map(|(f, n)| Some((f.as_str(), Unit::parse(&n.content)?))).collect();
        let mut defs: HashMap<&str, Vec<(&str, usize)>> = HashMap::new();
        for (f, u) in &units {
            for (ci, c) in u.contracts.iter().enumerate() {
                defs.entry(c.name.as_str()).or_default().push((f, ci));
            }
        }
        // contracts deriving from forge-std Test/Script or ds-test DSTest: the only ones rewritten
        let mut testish: HashMap<(&str, usize), bool> = HashMap::new();
        fn derives(f: &str, ci: usize, units: &HashMap<&str, Unit>, defs: &HashMap<&str, Vec<(&str, usize)>>, memo: &mut HashMap<(String, usize), bool>, depth: usize) -> bool {
            if let Some(v) = memo.get(&(f.to_string(), ci)) {
                return *v;
            }
            let c = &units[f].contracts[ci];
            let v = depth < 32
                && c.bases.iter().any(|b| matches!(b.as_str(), "Test" | "DSTest" | "Script") || defs.get(b.as_str()).into_iter().flatten().any(|(bf, bci)| derives(bf, *bci, units, defs, memo, depth + 1)));
            memo.insert((f.to_string(), ci), v);
            v
        }
        let mut memo = HashMap::new();
        for (f, u) in &units {
            for ci in 0..u.contracts.len() {
                testish.insert((f, ci), is_test(f) && derives(f, ci, &units, &defs, &mut memo, 0));
            }
        }
        let project: std::collections::HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
        // linking only works for libraries we compile (project files)
        let needs_link: HashMap<&str, bool> = files.iter().map(|f| (f.as_str(), g.closure(f).iter().any(|c| !project.contains(c.as_str()) && units.get(c.as_str()).is_some_and(|u| u.ext_lib)))).collect();
        let memory_safe = *v >= semver::Version::new(0, 8, 13);
        let mut sources = HashMap::new();
        let mut gen_deps: HashMap<String, Vec<String>> = HashMap::new();
        let mut uses: HashMap<(String, String), std::collections::BTreeSet<String>> = HashMap::new();
        // contracts tests may deploy from artifacts, with the generated constructor checks for them
        let mut targets: HashMap<(&str, usize), rewrite::Target> = HashMap::new();
        for f in files {
            let Some(u) = units.get(f.as_str()) else { continue };
            if needs_link.get(f.as_str()).copied().unwrap_or(true) {
                continue;
            }
            for (ci, c) in u.contracts.iter().enumerate() {
                if c.kind != "contract" || c.is_abstract {
                    continue;
                }
                let id = format!("{f}:{}", c.name);
                let path = format!("__plain__/{f}/{}.sol", c.name);
                // the generated file imports the target's file; the import must resolve to exactly that unit
                let rel = format!("{}{f}", "../".repeat(path.matches('/').count()));
                let Some(import) = [f.clone(), rel].into_iter().find(|i| p.resolve_import(&path, i) == *f) else { continue };
                let check = format!("__PlainCtor_{}", &hex::encode(alloy_primitives::keccak256(id.as_bytes()))[..8]);
                let (params, payable) = c.ctor.map_or(("()".to_string(), false), |(a, b, pay)| (g.files[f].content[u.toks[a].s..u.toks[b].e].to_string(), pay));
                sources.insert(path.clone(), rewrite::ctor_check(&check, &import, &c.name, &params));
                targets.insert((f.as_str(), ci), rewrite::Target { id, check, path, payable });
            }
        }
        if *v >= semver::Version::new(0, 8, 11) {
            // every test file that some job will read (a dirty file's whole closure), not just dirty ones
            let mut seen = std::collections::BTreeSet::new();
            for d in dirty {
                for f in g.closure(d) {
                    if !is_test(&f) || !seen.insert(f.clone()) {
                        continue;
                    }
                    let (Some(u), Some(node)) = (units.get(f.as_str()), g.files.get(&f)) else { continue };
                    let closure = g.closure(&f);
                    // a name any imported file aliases may not be the contract defined under it (`import {Real as Token}`
                    // re-exported through a wildcard import): those keep solc's own `new`
                    let aliased: std::collections::HashSet<&str> = closure.iter().filter_map(|c| units.get(c.as_str())).flat_map(|x| x.aliases.iter().map(String::as_str)).collect();
                    let target = |name: &str| -> Option<&rewrite::Target> {
                        let cands: Vec<&(&str, usize)> = defs.get(name)?.iter().filter(|(df, _)| closure.contains(*df)).collect();
                        let [(df, ci)] = cands.as_slice() else { return None };
                        (*df != f && !aliased.contains(name)).then(|| targets.get(&(*df, *ci))).flatten()
                    };
                    let in_test = |ci: usize| testish.get(&(f.as_str(), ci)).copied().unwrap_or(false);
                    // contracts a script file defines may go on chain: their bytecode (metadata hash) must be
                    // solc's for the file as written, or explorers can't verify them
                    if f.starts_with(&format!("{}/", p.dirs[2])) && u.contracts.iter().enumerate().any(|(ci, c)| c.kind == "contract" && !c.is_abstract && !in_test(ci)) {
                        continue;
                    }
                    let (edits, ids) = rewrite::link_news(&node.content, u, &f, &in_test, &target, memory_safe);
                    for (ci, t) in ids {
                        uses.entry((f.clone(), u.contracts[ci].name.clone())).or_default().insert(t.id.clone());
                        gen_deps.entry(f.clone()).or_default().push(t.path.clone());
                    }
                    if !edits.is_empty() {
                        sources.insert(f.clone(), rewrite::apply(&node.content, edits));
                    }
                }
            }
        }
        // artifacts each test contract may deploy through its own or inherited rewritten code (their libraries get deployed up front)
        let mut deploys: HashMap<(String, String), Vec<String>> = HashMap::new();
        for (f, u) in &units {
            for (ci, c) in u.contracts.iter().enumerate() {
                if !testish.get(&(*f, ci)).copied().unwrap_or(false) {
                    continue;
                }
                let mut ids = std::collections::BTreeSet::new();
                let mut stack = vec![(f.to_string(), c.name.clone())];
                let mut seen = std::collections::HashSet::new();
                while let Some((bf, bn)) = stack.pop() {
                    if !seen.insert((bf.clone(), bn.clone())) {
                        continue;
                    }
                    ids.extend(uses.get(&(bf.clone(), bn.clone())).into_iter().flatten().cloned());
                    let Some(bci) = units.get(bf.as_str()).and_then(|bu| bu.contracts.iter().position(|x| x.name == bn)) else { continue };
                    for b in &units[bf.as_str()].contracts[bci].bases {
                        for (df, _) in defs.get(b.as_str()).into_iter().flatten() {
                            stack.push((df.to_string(), b.clone()));
                        }
                    }
                }
                if !ids.is_empty() {
                    deploys.insert((f.to_string(), c.name.clone()), ids.into_iter().collect());
                }
            }
        }
        // estimated cost: last build's solc time, else the bytes of source compiled into the file's
        // deployable contracts (scaled by this project's ms per byte when some history exists)
        let rewritten: HashMap<&str, Unit> = sources.par_iter().filter_map(|(f, s)| Some((f.as_str(), Unit::parse(s)?))).collect();
        let test_file = |f: &str| f.starts_with(&format!("{}/", p.dirs[1])) || f.ends_with(".t.sol");
        let mut model = Cost { units: HashMap::new(), memo: HashMap::new(), defs: &defs, is_test: &test_file };
        for (f, n) in &g.files {
            if let Some(u) = rewritten.get(f.as_str()).or(units.get(f.as_str())) {
                model.units.insert(f.as_str(), (u, sources.get(f).map_or(n.content.as_str(), |s| s.as_str())));
            }
        }
        let (ms, bytes) = cache.files.iter().filter(|(f, e)| e.ms > 0 && g.files.contains_key(*f)).fold((0.0, 0.0), |(m, b), (f, e)| (m + e.ms as f64, b + model.file(f) as f64));
        let per_byte = if ms > 0.0 && bytes > 0.0 { ms / bytes } else { 1.0 };
        // (estimate, share of it in test functions: what sharding splits, bytes of test functions when there's no history)
        let est_own: HashMap<String, (u64, f64, usize)> = dirty
            .iter()
            .map(|f| {
                let code = model.file(f).max(1);
                let hist = cache.files.get(*f).map_or(0, |e| e.ms);
                let e = if hist > 0 { hist } else { (code as f64 * per_byte) as u64 };
                let tests: u64 = model.units.get(f.as_str()).map_or(0, |(u, _)| u.funcs.iter().filter(|x| crate::rewrite::is_test_name(&x.name)).map(|x| x.size as u64).sum());
                let weighted = if test_file(f) { tests * 3 } else { tests };
                // timings of a sharded build are mostly test functions already
                let own = if hist > 0 && cache.files.get(*f).is_some_and(|e| e.shards() > 1) { 0.95 } else { (weighted as f64 / code as f64).min(1.0) };
                (f.to_string(), (e.max(1), own, if hist > 0 { 0 } else { tests as usize }))
            })
            .collect();
        let est: HashMap<String, u64> = est_own.iter().map(|(f, (e, ..))| (f.clone(), *e)).collect();
        if std::env::var("PLAIN_DEBUG").is_ok_and(|v| v == "est") {
            for f in dirty {
                eprintln!("est {} {} tests {:.2}", est[f.as_str()], f, est_own[f.as_str()].1);
            }
        }
        let total: u64 = est.values().sum();
        let slot = (total / workers as u64).max(1);
        let mut jobs = vec![];
        let mut small: [Vec<(String, u64)>; 2] = [vec![], vec![]];
        let mut unit_plans = HashMap::new();
        for f in dirty {
            let (e, own, test_bytes) = est_own[f.as_str()];
            let test = is_test(f) && *v >= semver::Version::new(0, 8, 0);
            // a file whose own code is over half a core's share is split into units by test function;
            // every unit still compiles what the file embeds, so only the own share counts
            let split = (e as f64 * own) as u64;
            // via-IR: also cap a unit at ~48 KB of test code (several seconds of solc) when there's no history.
            // Each unit re-pays the file's fixed cost (analysis, setUp, helpers), so units aim at about one
            // core's share of the build, not less.
            let floor = if p.via_ir { 48 * 1024 } else { usize::MAX };
            let old = cache.files.get(*f).filter(|o| !o.units.is_empty() && !force);
            let big = old.is_some_and(|o| o.units.len() > 1) || split > slot || test_bytes > 2 * floor;
            if test && workers > 1 {
                // contract indices survive the `new` rewrite (it only edits bodies and appends a free function)
                let in_test = |ci: usize| testish.get(&(f.as_str(), ci)).copied().unwrap_or(false);
                let content = sources.get(*f).cloned().unwrap_or_else(|| g.files[*f].content.clone());
                if let Some((cu, tests)) = Unit::parse(&content).and_then(|cu| Some((rewrite::test_funcs(&content, &cu, &in_test)?, cu)).map(|(t, cu)| (cu, t))) {
                    let rm = rewrite::removable(&content, &cu, &tests);
                    let hashes = rewrite::test_hashes(&content, &cu, &in_test);
                    let shared = {
                        use sha2::Digest;
                        let text = rewrite::apply(&content, rewrite::unit_edits(&content, &cu, &in_test, &|_| false, &rm));
                        let deps = g.closure_hash(f, &mut |u| if u == f.as_str() { String::new() } else { own_hash[u].clone() });
                        if std::env::var("PLAIN_DEBUG").is_ok_and(|v| v == "units") {
                            eprintln!("units {f}: text {} deps {deps}", hex::encode(sha2::Sha256::digest(text.as_bytes())));
                            std::fs::write(p.root.join(format!(".plain/shared-{}.sol", f.replace('/', "_"))), &text).ok();
                        }
                        hex::encode(sha2::Sha256::digest(format!("{text}{deps}").as_bytes()))
                    };
                    let size: HashMap<&str, usize> = tests.iter().map(|(n, s)| (n.as_str(), *s)).collect();
                    let sz = |v: &[String]| v.iter().map(|n| size.get(n.as_str()).copied().unwrap_or(0)).sum::<usize>();
                    let total = tests.iter().map(|t| t.1).sum::<usize>().max(1);
                    let k = (split.div_ceil(slot) as usize).max(test_bytes.div_ceil(floor)).min(workers).min(tests.len() / 2);
                    let meta = |owns: Vec<String>, hot: bool| UnitMeta { tests: owns.iter().map(|n| (n.clone(), hashes[n].clone())).collect(), owns, hot, ms: 0 };
                    let mut units: Vec<(UnitMeta, Option<u32>)> = vec![];
                    if let Some(o) = old.filter(|o| o.shared == shared) {
                        // only test bodies changed: units keep the tests they compiled unchanged (unit 0 stays
                        // regardless: it brings the file's other contracts and inherited tests)
                        for (i, u) in o.units.iter().enumerate() {
                            let owns: Vec<String> = u.owns.iter().filter(|n| hashes.get(*n).is_some() && hashes.get(*n) == u.tests.get(*n)).cloned().collect();
                            if i == 0 || !owns.is_empty() {
                                units.push((UnitMeta { owns, ..u.clone() }, Some(i as u32)));
                            }
                        }
                        let covered: std::collections::HashSet<&String> = units.iter().flat_map(|u| u.0.owns.iter()).collect();
                        let mut pending: Vec<String> = tests.iter().map(|t| t.0.clone()).filter(|n| !covered.contains(n)).collect();
                        if !pending.is_empty() {
                            // one hot unit for edited and new tests, absorbing earlier hot units while it stays small
                            let hot: Vec<String> = units.iter().skip(1).filter(|u| u.0.hot).flat_map(|u| u.0.owns.clone()).collect();
                            if sz(&pending) + sz(&hot) <= total / k.max(2) {
                                let mut i = 0;
                                units.retain(|u| {
                                    i += 1;
                                    i == 1 || !u.0.hot
                                });
                                pending.extend(hot);
                            } else {
                                units.iter_mut().for_each(|u| u.0.hot = false);
                            }
                            pending.sort();
                            units.push((meta(pending, true), None));
                        }
                        if units.len() > 2 * k.max(2) + 2 {
                            units.clear(); // fragmented: shard afresh
                        }
                    }
                    if units.is_empty() && k >= 2 && big {
                        let a = rewrite::assign(&tests, k);
                        for s in 0..k {
                            let mut owns: Vec<String> = a.iter().filter(|(_, x)| **x == s).map(|(n, _)| n.clone()).collect();
                            owns.sort();
                            units.push((meta(owns, false), None));
                        }
                    }
                    if !units.is_empty() {
                        let n = units.len();
                        for (i, (u, reuse)) in units.iter().enumerate() {
                            if reuse.is_some() {
                                continue;
                            }
                            let keep = |x: &str| u.owns.iter().any(|o| o == x);
                            let src = rewrite::apply(&content, rewrite::unit_edits(&content, &cu, &in_test, &keep, &rm));
                            let fast = u.hot && !observes_gas(&src) && !g.closure(f).iter().any(|c| c != *f && is_test(c) && observes_gas(&g.files[c].content));
                            sources.insert(format!("{f}#{i}"), src);
                            // unit 0 also compiles the file's other contracts (mocks, harnesses); the rest only the tests
                            let only = if i == 0 { vec![] } else { cu.contracts.iter().enumerate().filter(|(ci, c)| in_test(*ci) && !c.is_abstract && c.kind == "contract").map(|(_, c)| c.name.clone()).collect() };
                            jobs.push(Job { files: vec![f.to_string()], shard: Some((i, n)), test: true, est: e - split + split * sz(&u.owns) as u64 / total as u64, fast, only });
                        }
                        unit_plans.insert(f.to_string(), (shared, units));
                        continue;
                    }
                    // compiled whole, but recorded as one unit: the next edit compiles only the tests it changed
                    let mut all: Vec<String> = tests.iter().map(|t| t.0.clone()).collect();
                    all.sort();
                    unit_plans.insert(f.to_string(), (shared, vec![(meta(all, false), None)]));
                }
            }
            if e > slot / 4 {
                jobs.push(Job { files: vec![f.to_string()], shard: None, test, est: e, fast: false, only: vec![] });
            } else {
                small[test as usize].push((f.to_string(), e));
            }
        }
        // pack small files into jobs of about a quarter share each (fewer processes, shared analysis)
        for (test, mut list) in small.into_iter().enumerate() {
            list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let mut cur = Job { files: vec![], shard: None, test: test == 1, est: 0, fast: false, only: vec![] };
            for (f, e) in list {
                if cur.est + e > slot / 4 && !cur.files.is_empty() {
                    jobs.push(std::mem::replace(&mut cur, Job { files: vec![], shard: None, test: test == 1, est: 0, fast: false, only: vec![] }));
                }
                cur.files.push(f);
                cur.est += e;
            }
            if !cur.files.is_empty() {
                jobs.push(cur);
            }
        }
        jobs.sort_by(|a, b| b.est.cmp(&a.est));
        if std::env::var_os("PLAIN_DEBUG").is_some() {
            let dir = p.root.join(".plain/rewritten");
            std::fs::create_dir_all(&dir).ok();
            for (f, s) in &sources {
                std::fs::write(dir.join(f.replace('/', "_")), s).ok();
            }
        }
        for d in gen_deps.values_mut() {
            d.sort();
            d.dedup();
        }
        // code closures of test-job files, on the sources they compile from
        let mut code = HashMap::new();
        for f in dirty.iter().filter(|f| is_test(f)) {
            let closure = g.closure(f);
            let mut out = vec![f.to_string()];
            // whatever library, free-function or constant code anywhere in the imports creates is compiled into the
            // test wherever that code is used (its own changes already change its file's interface hash)
            let mut whole = false;
            let mut names: Vec<String> = vec![];
            for c in &closure {
                let src = sources.get(c).unwrap_or(&g.files[c].content);
                match Unit::parse(src) {
                    Some(u) => {
                        let loose = u.loose_created(src);
                        // an alias could name anything: no telling which file its code is in
                        whole |= !loose.is_empty() && !u.aliases.is_empty();
                        names.extend(loose);
                    }
                    None => whole = true,
                }
            }
            let push = |out: &mut Vec<String>, names: &[String]| {
                for name in names {
                    for (df, _) in defs.get(name.as_str()).into_iter().flatten() {
                        if closure.contains(*df) && !out.iter().any(|o| o == df) {
                            out.push(df.to_string());
                        }
                    }
                }
            };
            push(&mut out, &names);
            let mut i = 0;
            while i < out.len() && !whole {
                let x = out[i].clone();
                i += 1;
                let src = sources.get(&x).unwrap_or(&g.files[&x].content);
                let Some(u) = Unit::parse(src) else {
                    whole = true;
                    break;
                };
                // bases or creations under an alias or a namespace (`import * as M`, `is M.Base`) can't be told apart
                // by name: then every file in the imports counts in full
                whole |= !u.aliases.is_empty();
                push(&mut out, &u.code_refs(src));
            }
            if whole {
                out = closure.iter().cloned().collect();
            }
            out.sort();
            code.insert(f.to_string(), out);
        }
        let rewritten = testish.iter().filter(|(_, t)| **t).map(|((f, ci), _)| (f.to_string(), units[*f].contracts[*ci].name.clone())).collect();
        Plan { jobs, sources, est, deploys, gen_deps, code, rewritten, units: unit_plans }
    }
}

/// Test code that measures its own gas (`gasleft()`, gas snapshots between cheatcode calls) must be compiled
/// as configured; `snapshotGasLastCall` measures the callee only.
fn observes_gas(src: &str) -> bool {
    src.contains("gasleft") || src.contains("startSnapshotGas") || src.contains("stopSnapshotGas")
}

/// Cold-build cost model: bytes of source compiled into a contract's bytecode, i.e. its body, its bases'
/// bodies and (in full) what it creates with `new`. Libraries are left out: solc compiles only the
/// functions used. Calibrated on v4-core, solc CPU time tracks output bytecode size, which this follows.
struct Cost<'a> {
    units: HashMap<&'a str, (&'a crate::rewrite::Unit, &'a str)>,
    /// test code yields ~3x the bytecode per source byte (asserts, abi encoding); fitted on v4-core + veydrift
    is_test: &'a dyn Fn(&str) -> bool,
    defs: &'a HashMap<&'a str, Vec<(&'a str, usize)>>,
    memo: HashMap<(&'a str, usize), u64>,
}

impl<'a> Cost<'a> {
    fn file(&mut self, f: &str) -> u64 {
        let Some((key, (u, _))) = self.units.get_key_value(f).map(|(k, v)| (*k, *v)) else { return 0 };
        1024 + (0..u.contracts.len()).filter(|&ci| u.contracts[ci].kind == "contract" && !u.contracts[ci].is_abstract).map(|ci| self.contract(key, ci, 0)).sum::<u64>()
    }

    fn contract(&mut self, f: &'a str, ci: usize, depth: usize) -> u64 {
        if let Some(v) = self.memo.get(&(f, ci)) {
            return *v;
        }
        self.memo.insert((f, ci), 0); // cycle guard
        // the contract and its bases (each once), skipping test framework code
        let mut parts: Vec<(&'a str, usize)> = vec![];
        let mut stack = vec![(f, ci)];
        while let Some((bf, bci)) = stack.pop() {
            if parts.contains(&(bf, bci)) || bf.contains("forge-std/") || bf.contains("ds-test/") {
                continue;
            }
            parts.push((bf, bci));
            let Some((u, _)) = self.units.get(bf) else { continue };
            for b in &u.contracts[bci].bases {
                stack.extend(self.defs.get(b.as_str()).into_iter().flatten().copied());
            }
        }
        let mut total = 0;
        let mut created = vec![];
        // identifiers the contract's code uses: they pick the library functions compiled into it
        let mut idents: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
        for (bf, bci) in parts {
            let Some((u, src)) = self.units.get(bf).copied() else { continue };
            let c = &u.contracts[bci];
            if c.kind == "interface" {
                continue;
            }
            total += (u.toks[c.body.1].e - u.toks[c.body.0].s) as u64 * if (self.is_test)(bf) { 3 } else { 1 };
            for i in c.body.0..c.body.1 {
                if u.toks[i].k != crate::rewrite::K::Ident {
                    continue;
                }
                let n = &src[u.toks[i].s..u.toks[i].e];
                if i > 0 && &src[u.toks[i - 1].s..u.toks[i - 1].e] == "new" {
                    created.extend(self.defs.get(n).into_iter().flatten().copied());
                }
                idents.insert(n);
            }
        }
        // library functions called (by name, transitively through their own bodies); not test framework code
        let mut libs: Vec<(&'a str, usize)> = vec![];
        let mut counted: std::collections::HashSet<(&'a str, usize)> = std::collections::HashSet::new();
        loop {
            let before = (libs.len(), counted.len());
            for n in idents.iter() {
                for &(lf, lci) in self.defs.get(n).into_iter().flatten() {
                    if !libs.contains(&(lf, lci)) && !lf.contains("forge-std/") && self.units.get(lf).is_some_and(|(u, _)| u.contracts[lci].kind == "library") {
                        libs.push((lf, lci));
                    }
                }
            }
            let mut found = vec![];
            for &(lf, lci) in &libs {
                let Some((u, src)) = self.units.get(lf).copied() else { continue };
                for (fi, f) in u.funcs.iter().enumerate() {
                    if f.contract == lci && idents.contains(f.name.as_str()) && counted.insert((lf, fi)) {
                        total += f.size as u64;
                        found.extend(u.toks[f.toks.0..f.toks.1].iter().filter(|t| t.k == crate::rewrite::K::Ident).map(|t| &src[t.s..t.e]));
                    }
                }
            }
            idents.extend(found);
            if (libs.len(), counted.len()) == before {
                break;
            }
        }
        created.sort();
        created.dedup();
        if depth < 16 {
            for (cf, cci) in created {
                total += self.contract(cf, cci, depth + 1);
            }
        }
        self.memo.insert((f, ci), total);
        total
    }
}

/// Runs jobs on a pool of workers, biggest first; results with solc wall time (ms).
fn run_jobs(solc: &PathBuf, g: &Graph, jobs: &[Job], settings: &Value, p: &Project, sources: &HashMap<String, String>, gen_deps: &HashMap<String, Vec<String>>) -> Vec<(Job, Result<(Vec<Artifact>, Vec<String>), String>, u64)> {
    let workers = std::env::var("PLAIN_JOBS").ok().and_then(|j| j.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out = std::sync::Mutex::new(Vec::new());
    let t0 = std::time::Instant::now();
    std::thread::scope(|s| {
        for _ in 0..workers.min(jobs.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(job) = jobs.get(i) else { break };
                let t = std::time::Instant::now();
                let r = compile(solc, g, job, settings, p, sources, gen_deps);
                let ms = t.elapsed().as_millis() as u64;
                if std::env::var_os("PLAIN_DEBUG").is_some() {
                    eprintln!("job {:.1}s at {:.1}s est {} {:?}{}{}", ms as f64 / 1000.0, (t0.elapsed().as_millis() as u64 - ms) as f64 / 1000.0, job.est, job.files, job.shard.map(|(k, n)| format!(" shard {k}/{n}")).unwrap_or_default(), if job.fast { " fast" } else { "" });
                }
                out.lock().unwrap().push((i, r, ms));
            });
        }
    });
    let mut res = out.into_inner().unwrap();
    res.sort_by_key(|r| r.0);
    res.into_iter().map(|(i, r, ms)| (Job { files: jobs[i].files.clone(), shard: jobs[i].shard, test: jobs[i].test, est: jobs[i].est, fast: jobs[i].fast, only: jobs[i].only.clone() }, r, ms)).collect()
}

fn gitignore(p: &Project) {
    let gi = p.root.join(".plain/.gitignore");
    if !gi.exists() {
        std::fs::write(gi, "*\n").ok();
    }
}

pub fn settings(p: &Project) -> Value {
    let mut s = json!({
        "remappings": p.remappings.iter().map(|(a, b)| format!("{a}={b}")).collect::<Vec<_>>(),
        "optimizer": {"enabled": p.optimizer, "runs": p.runs},
        "viaIR": p.via_ir,
    });
    if let Some(e) = EVM.get().or(p.evm_version.as_ref()) {
        s["evmVersion"] = json!(e);
    }
    if let Some(h) = &p.bytecode_hash {
        s["metadata"]["bytecodeHash"] = json!(h);
    }
    if let Some(c) = p.cbor_metadata {
        s["metadata"]["appendCBOR"] = json!(c);
    }
    s
}

/// solc's standard JSON output for `input`.
fn run(solc: &PathBuf, root: &std::path::Path, input: &Value) -> Result<Value, String> {
    let mut child = Command::new(solc)
        .args(["--standard-json", "--base-path"])
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn solc: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let body = serde_json::to_vec(input).unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&body));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    writer.join().ok();
    serde_json::from_slice(&out.stdout).map_err(|e| format!("solc output: {e}: {}", String::from_utf8_lossy(&out.stderr)))
}

/// Compiles a standard JSON input with solc `version` ("0.8.28", or as explorers write it); errors as solc prints them.
pub fn standard_json(version: &str, input: &Value, root: &std::path::Path) -> Result<Value, String> {
    let v = run(&ensure(version.split('+').next().unwrap_or(version)), root, input)?;
    let errs: Vec<&str> = v["errors"].as_array().into_iter().flatten().filter(|e| e["severity"] == "error").filter_map(|e| e["formattedMessage"].as_str()).collect();
    match errs.is_empty() {
        true => Ok(v),
        false => Err(errs.join("\n")),
    }
}

/// Each contract in `file` as solc compiles the standard JSON input explorers verify against (the file's import
/// closure and the project's settings, nothing rewritten or cached): name -> (bytecode, deployed bytecode).
pub fn canonical(p: &Project, version: &str, file: &str) -> Result<HashMap<String, (String, String)>, String> {
    let mut input = crate::verify::input(p, file);
    input["settings"]["outputSelection"] = json!({});
    input["settings"]["outputSelection"][file]["*"] = json!(["evm.bytecode.object", "evm.deployedBytecode.object"]);
    // remembered by what was compiled (sources, settings, compiler), so a deploy pays for this once per change
    let key = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(format!("{version}{input}").as_bytes()))
    };
    let path = home().join("cache/canonical").join(format!("{key}.json"));
    if let Some(hit) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok(hit);
    }
    let out = standard_json(version, &input, &p.root)?;
    let code = |c: &Value, k: &str| c["evm"][k]["object"].as_str().unwrap_or_default().to_string();
    let found: HashMap<String, (String, String)> = out["contracts"][file].as_object().into_iter().flatten().map(|(n, c)| (n.clone(), (code(c, "bytecode"), code(c, "deployedBytecode")))).collect();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(&found).unwrap()).is_ok() {
        std::fs::rename(&tmp, &path).ok();
    }
    Ok(found)
}

/// Artifacts whose bytecode differs from `canonical`, compiling each file again (in parallel). Test and script
/// contracts compile from rewritten sources and never go on chain, so they're left out.
pub fn check(p: &Project, out: &Output) -> Vec<String> {
    let mut by_file: BTreeMap<&str, Vec<&Artifact>> = BTreeMap::new();
    for a in out.artifacts.iter().filter(|a| a.shard == 0 && !a.rewritten && !a.bytecode.is_empty() && !rewritten(p, &a.file)) {
        by_file.entry(a.file.as_str()).or_default().push(a);
    }
    by_file
        .par_iter()
        .flat_map(|(f, arts)| match canonical(p, &out.solc, f) {
            Err(e) => vec![format!("{f}: {e}")],
            Ok(c) => arts
                .iter()
                .filter_map(|a| match c.get(&a.name) {
                    Some((b, d)) if *b == a.bytecode && *d == a.deployed => None,
                    Some(_) => Some(format!("{}:{} differs from solc's output for its sources", a.file, a.name)),
                    None => Some(format!("{}:{} isn't in solc's output for its file", a.file, a.name)),
                })
                .collect(),
        })
        .collect()
}

fn compile(solc: &PathBuf, g: &Graph, job: &Job, settings: &Value, p: &Project, overrides: &HashMap<String, String>, gen_deps: &HashMap<String, Vec<String>>) -> Result<(Vec<Artifact>, Vec<String>), String> {
    let mut sources = serde_json::Map::new();
    for f in &job.files {
        for u in g.closure(f) {
            if sources.contains_key(&u) {
                continue;
            }
            let shard = job.shard.filter(|_| &u == f).and_then(|(k, _)| overrides.get(&format!("{u}#{k}")));
            let rewritten = shard.or_else(|| if job.test { overrides.get(&u) } else { None });
            sources.insert(u.clone(), json!({"content": rewritten.unwrap_or(&g.files[&u].content)}));
            if rewritten.is_some() {
                for d in gen_deps.get(&u).into_iter().flatten() {
                    sources.insert(d.clone(), json!({"content": overrides[d]}));
                }
            }
        }
    }
    let mut settings = settings.clone();
    if job.fast {
        settings["optimizer"]["enabled"] = json!(false);
    }
    let out = json!(["abi", "evm.bytecode.object", "evm.bytecode.linkReferences", "evm.deployedBytecode.object"]);
    let sel: serde_json::Map<String, Value> = job
        .files
        .iter()
        .map(|f| (f.clone(), if job.only.is_empty() { json!({"*": out}) } else { Value::Object(job.only.iter().map(|c| (c.clone(), out.clone())).collect()) }))
        .collect();
    settings["outputSelection"] = Value::Object(sel);
    let input = json!({"language": "Solidity", "sources": sources, "settings": settings});
    if std::env::var("PLAIN_DEBUG").is_ok_and(|v| v == "2") {
        let dir = p.root.join(".plain/rewritten");
        std::fs::create_dir_all(&dir).ok();
        let name = format!("{}{}.json", job.files[0].replace('/', "_"), job.shard.map(|(k, _)| format!("#{k}")).unwrap_or_default());
        std::fs::write(dir.join(name), input.to_string()).ok();
    }
    let v = run(solc, &p.root, &input)?;
    let mut errs = Vec::new();
    let mut warns = Vec::new();
    for e in v["errors"].as_array().into_iter().flatten() {
        let msg = e["formattedMessage"].as_str().unwrap_or_default().trim_end().to_string();
        if e["severity"] == "error" {
            errs.push(msg);
        } else if e["sourceLocation"]["file"].as_str().is_some_and(|f| f.starts_with(&format!("{}/", p.dirs[0])) && !f.ends_with(".t.sol") && !f.contains("/test/")) {
            warns.push(msg);
        }
    }
    if !errs.is_empty() {
        return Err(errs.join("\u{0}"));
    }
    let mut arts = Vec::new();
    for (file, cs) in v["contracts"].as_object().into_iter().flatten() {
        for (name, c) in cs.as_object().into_iter().flatten() {
            let links = &c["evm"]["bytecode"]["linkReferences"];
            arts.push(Artifact {
                name: name.clone(),
                file: file.clone(),
                abi: c["abi"].clone(),
                bytecode: c["evm"]["bytecode"]["object"].as_str().unwrap_or_default().to_string(),
                deployed: c["evm"]["deployedBytecode"]["object"].as_str().unwrap_or_default().to_string(),
                links: if links.as_object().is_some_and(|o| !o.is_empty()) { links.clone() } else { Value::Null },
                shard: 0,
                tests: vec![],
                deploys: vec![],
                rewritten: false,
            });
        }
    }
    Ok((arts, warns))
}

#[cfg(test)]
mod tests {
    #[test]
    fn pragmas() {
        let v = |s: &str| s.parse::<semver::Version>().unwrap();
        assert!(super::pragma_matches("^0.8.20", &v("0.8.28")));
        assert!(!super::pragma_matches("^0.8.20", &v("0.9.0")));
        assert!(super::pragma_matches(">=0.6.2 <0.9.0", &v("0.8.28")));
        assert!(super::pragma_matches(">= 0.8.0", &v("0.8.28")));
        assert!(super::pragma_matches("0.8.28", &v("0.8.28")));
        assert!(!super::pragma_matches("0.8.26", &v("0.8.28")));
        assert!(super::pragma_matches("^0.7.0 || ^0.8.0", &v("0.8.1")));
    }
}
