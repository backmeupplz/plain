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
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    key: String,
    files: BTreeMap<String, CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    hash: String,
    contracts: Vec<Artifact>,
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

fn ensure(v: &str) -> PathBuf {
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
    let tmp = p.with_extension("tmp");
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
}

pub fn build(p: &Project, force: bool) -> Output {
    let files = p.project_files();
    if files.is_empty() {
        crate::die(&format!("no .sol files under {}", p.dirs.join(", ")));
    }
    let g = Graph::build(p, &files);
    let version = pick_version(p, &g);
    let settings = settings(p);
    let key = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(format!("{version}{settings}").as_bytes()))
    };
    let cache_path = p.root.join(".plain/cache.json");
    let mut cache: Cache = (!force)
        .then(|| std::fs::read(&cache_path).ok())
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|c: &Cache| c.key == key)
        .unwrap_or_default();
    cache.key = key;
    let hashes = g.hashes();
    cache.files.retain(|f, _| files.contains(f));
    let dirty: Vec<&String> = files.iter().filter(|f| cache.files.get(*f).is_none_or(|e| e.hash != hashes[*f])).collect();

    if !dirty.is_empty() {
        let solc = ensure(&version);
        let jobs = partition(&g, &dirty);
        let results: Vec<Result<(Vec<Artifact>, Vec<String>), String>> =
            jobs.par_iter().map(|job| {
                let t = std::time::Instant::now();
                let r = compile(&solc, &g, job, &settings, p);
                if std::env::var("PLAIN_DEBUG").is_ok() {
                    eprintln!("job {:.1?} {:?}", t.elapsed(), job);
                }
                r
            }).collect();
        let mut warnings = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        let mut fresh: HashMap<String, Vec<Artifact>> = dirty.iter().map(|f| (f.to_string(), vec![])).collect();
        for r in results {
            match r {
                Ok((arts, warns)) => {
                    warnings.extend(warns);
                    for a in arts {
                        fresh.entry(a.file.clone()).or_default().push(a);
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
        let out_dir = p.root.join(".plain/out");
        std::fs::create_dir_all(&out_dir).unwrap();
        for (f, contracts) in fresh {
            for a in &contracts {
                if !a.bytecode.is_empty() || a.abi.as_array().is_some_and(|x| !x.is_empty()) {
                    let j = json!({"contractName": a.name, "sourceName": a.file, "abi": a.abi, "bytecode": format!("0x{}", a.bytecode), "deployedBytecode": format!("0x{}", a.deployed)});
                    std::fs::write(out_dir.join(format!("{}.json", a.name)), j.to_string()).ok();
                }
            }
            cache.files.insert(f.clone(), CacheEntry { hash: hashes[&f].clone(), contracts });
        }
        std::fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();
        gitignore(p);
    }
    Output {
        compiled: dirty.len(),
        artifacts: cache.files.into_values().flat_map(|e| e.contracts).collect(),
        solc: version,
    }
}

fn gitignore(p: &Project) {
    let gi = p.root.join(".plain/.gitignore");
    if !gi.exists() {
        std::fs::write(gi, "*\n").ok();
    }
}

fn settings(p: &Project) -> Value {
    let mut s = json!({
        "remappings": p.remappings.iter().map(|(a, b)| format!("{a}={b}")).collect::<Vec<_>>(),
        "optimizer": {"enabled": p.optimizer, "runs": p.runs},
        "viaIR": p.via_ir,
    });
    if let Some(e) = &p.evm_version {
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

/// Split dirty files into ~ncpu jobs, balanced by closure size (longest-processing-time first).
fn partition(g: &Graph, dirty: &[&String]) -> Vec<Vec<String>> {
    let n = std::env::var("PLAIN_JOBS").ok().and_then(|j| j.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .min(dirty.len())
        .max(1);
    let mut weighted: Vec<(usize, &String)> = dirty
        .iter()
        .map(|f| (g.closure(f).iter().map(|u| g.files[u].content.len()).sum(), *f))
        .collect();
    weighted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut bins: Vec<(usize, Vec<String>)> = vec![(0, vec![]); n];
    for (w, f) in weighted {
        let b = bins.iter_mut().min_by_key(|b| b.0).unwrap();
        b.0 += w;
        b.1.push(f.clone());
    }
    bins.into_iter().map(|b| b.1).filter(|b| !b.is_empty()).collect()
}

fn compile(solc: &PathBuf, g: &Graph, job: &[String], settings: &Value, p: &Project) -> Result<(Vec<Artifact>, Vec<String>), String> {
    let mut sources = serde_json::Map::new();
    for f in job {
        for u in g.closure(f) {
            sources.entry(u.clone()).or_insert_with(|| json!({"content": g.files[&u].content}));
        }
    }
    let mut settings = settings.clone();
    let sel: serde_json::Map<String, Value> = job
        .iter()
        .map(|f| (f.clone(), json!({"*": ["abi", "evm.bytecode.object", "evm.bytecode.linkReferences", "evm.deployedBytecode.object"]})))
        .collect();
    settings["outputSelection"] = Value::Object(sel);
    let input = json!({"language": "Solidity", "sources": sources, "settings": settings});
    let mut child = Command::new(solc)
        .args(["--standard-json", "--base-path"])
        .arg(&p.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn solc: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let body = serde_json::to_vec(&input).unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&body));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    writer.join().ok();
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("solc output: {e}: {}", String::from_utf8_lossy(&out.stderr)))?;
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
