//! Project discovery: config (foundry.toml / plain.toml, both optional), sources, imports, remappings.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

pub struct Project {
    pub root: PathBuf,
    pub dirs: Vec<String>, // src, test, script: files we build bytecode for
    pub remappings: Vec<(String, String)>,
    pub solc: Option<String>,
    pub optimizer: bool,
    pub runs: u64,
    pub via_ir: bool,
    pub evm_version: Option<String>,
    pub bytecode_hash: Option<String>,
    pub cbor_metadata: Option<bool>,
    pub fuzz_runs: u32,
    pub allow_internal_revert: bool,
    pub invariant: (u32, u32, bool),
    pub networks: BTreeMap<String, String>,
}

type Table = toml::Table;

fn read_toml(p: &Path) -> Option<Table> {
    let s = std::fs::read_to_string(p).ok()?;
    match s.parse::<Table>() {
        Ok(t) => Some(t),
        Err(e) => crate::die(&format!("{}: {e}", p.display())),
    }
}

/// `${VAR}` interpolation for urls.
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else { break };
        out.push_str(&std::env::var(&rest[i + 2..i + j]).unwrap_or_default());
        rest = &rest[i + j + 1..];
    }
    out + rest
}

impl Project {
    pub fn load() -> Project {
        let cwd = std::env::current_dir().unwrap();
        let root = cwd
            .ancestors()
            .find(|d| d.join("plain.toml").exists() || d.join("foundry.toml").exists())
            .unwrap_or(&cwd)
            .to_path_buf();
        // foundry.toml [profile.default] first, plain.toml (same keys, top level) overrides.
        let mut cfg = Table::new();
        let mut networks = BTreeMap::new();
        if let Some(f) = read_toml(&root.join("foundry.toml")) {
            if let Some(p) = f.get("profile").and_then(|p| p.get("default")).and_then(|p| p.as_table()) {
                cfg.extend(p.clone());
            }
            for k in ["fuzz", "invariant"] {
                if let Some(t) = f.get(k).and_then(|p| p.as_table()) {
                    cfg.entry(k.to_string()).or_insert_with(|| t.clone().into());
                }
            }
            if let Some(r) = f.get("rpc_endpoints").and_then(|p| p.as_table()) {
                for (k, v) in r {
                    networks.insert(k.replace('_', "-"), v.as_str().unwrap_or_default().to_string());
                }
            }
        }
        if let Some(p) = read_toml(&root.join("plain.toml")) {
            for (k, v) in p {
                if k == "networks" {
                    for (n, u) in v.as_table().into_iter().flatten() {
                        networks.insert(n.clone(), u.as_str().unwrap_or_default().to_string());
                    }
                } else {
                    cfg.insert(k, v);
                }
            }
        }
        let s = |k: &str| cfg.get(k).and_then(|v| v.as_str()).map(String::from);
        let b = |k: &str| cfg.get(k).and_then(|v| v.as_bool());
        let src = s("src").unwrap_or_else(|| if root.join("contracts").is_dir() && !root.join("src").is_dir() { "contracts".into() } else { "src".into() });
        let test = s("test").unwrap_or("test".into());
        let script = s("script").unwrap_or("script".into());
        let libs: Vec<String> = cfg
            .get("libs")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or(vec!["lib".into()]);
        let fuzz_runs = cfg
            .get("fuzz")
            .and_then(|f| f.get("runs"))
            .and_then(|v| v.as_integer())
            .unwrap_or(256) as u32;

        let mut remappings = Vec::new();
        let mut add = |line: &str, base: &str| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return;
            }
            // drop optional `context:` prefix
            let line = match line.split_once(':') {
                Some((_, r)) if r.contains('=') => r,
                _ => line,
            };
            if let Some((a, b)) = line.split_once('=') {
                if !remappings.iter().any(|(p, _): &(String, String)| p == a) {
                    remappings.push((a.to_string(), format!("{base}{b}")));
                }
            }
        };
        for r in cfg.get("remappings").and_then(|v| v.as_array()).into_iter().flatten() {
            add(r.as_str().unwrap_or_default(), "");
        }
        if let Ok(t) = std::fs::read_to_string(root.join("remappings.txt")) {
            t.lines().for_each(|l| add(l, ""));
        }
        // auto-detect lib/<name> like forge: name/ => lib/name/src/ (or lib/name/), plus nested remappings.txt
        for lib in &libs {
            let Ok(rd) = std::fs::read_dir(root.join(lib)) else { continue };
            let mut entries: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().to_string();
                let base = format!("{lib}/{name}/");
                if let Ok(t) = std::fs::read_to_string(e.path().join("remappings.txt")) {
                    t.lines().for_each(|l| add(l, &base));
                }
                let target = if e.path().join("src").is_dir() { format!("{base}src/") } else if e.path().join("contracts").is_dir() { format!("{base}contracts/") } else { base.clone() };
                add(&format!("{name}/={target}"), "");
            }
        }
        // ponytail: nested-lib remappings take the lib dir as base; relative-to-lib targets like "lib/x" inside libs are normalized below
        for (_, t) in remappings.iter_mut() {
            *t = normalize(t);
            if !t.ends_with('/') && root.join(&*t).is_dir() {
                t.push('/');
            }
        }

        Project {
            dirs: vec![src, test, script],
            remappings,
            solc: s("solc_version").or(s("solc")).filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())),
            // forge: off unless optimizer_runs is set; without foundry.toml: on
            optimizer: b("optimizer").unwrap_or(!root.join("foundry.toml").exists() || cfg.contains_key("optimizer_runs")),
            runs: cfg.get("optimizer_runs").and_then(|v| v.as_integer()).unwrap_or(200) as u64,
            via_ir: b("via_ir").unwrap_or(false),
            evm_version: s("evm_version"),
            bytecode_hash: s("bytecode_hash"),
            cbor_metadata: b("cbor_metadata"),
            fuzz_runs,
            allow_internal_revert: b("allow_internal_expect_revert").unwrap_or(false),
            invariant: {
                let i = |k: &str| cfg.get("invariant").and_then(|t| t.get(k));
                (
                    i("runs").and_then(|v| v.as_integer()).unwrap_or(256) as u32,
                    i("depth").and_then(|v| v.as_integer()).unwrap_or(500) as u32,
                    i("fail_on_revert").and_then(|v| v.as_bool()).unwrap_or(false),
                )
            },
            networks,
            root,
        }
    }

    /// All .sol files under src/test/script, relative to root.
    pub fn project_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        for d in &self.dirs {
            walk(&self.root, &self.root.join(d), &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Resolve an import the way solc does (remappings, then relative), returning the source unit name.
    pub fn resolve_import(&self, from: &str, path: &str) -> String {
        if path.starts_with("./") || path.starts_with("../") {
            let dir = from.rsplit_once('/').map(|x| x.0).unwrap_or("");
            return normalize(&format!("{dir}/{path}"));
        }
        let best = self
            .remappings
            .iter()
            .filter(|(p, _)| path.starts_with(p.as_str()))
            .max_by_key(|(p, _)| p.len());
        match best {
            Some((p, t)) => normalize(&format!("{t}{}", &path[p.len()..])),
            None => normalize(path),
        }
    }

    /// Find a unit on disk: as-is, then node_modules/, then lib dirs.
    pub fn locate(&self, unit: &str) -> Option<PathBuf> {
        ["", "node_modules/", "lib/"]
            .iter()
            .map(|p| self.root.join(format!("{p}{unit}")))
            .find(|p| p.is_file())
    }

    /// Source text of a unit: from disk, else the embedded forge-std.
    pub fn read(&self, unit: &str) -> Option<String> {
        match self.locate(unit) {
            Some(p) => Some(std::fs::read_to_string(&p).unwrap_or_else(|e| crate::die(&format!("{}: {e}", p.display())))),
            None => crate::stdlib::FORGE_STD.iter().find(|(n, _)| *n == unit).map(|(_, c)| c.to_string()),
        }
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if p.extension().is_some_and(|x| x == "sol") {
            out.push(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}

pub fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut s = parts.join("/");
    if p.ends_with('/') && !s.is_empty() {
        s.push('/');
    }
    s
}

/// Strip comments and string-free scan for `import ... "path";` and `pragma solidity ...;`.
pub fn scan(src: &str) -> (Vec<String>, Option<String>) {
    let b = src.as_bytes();
    let mut clean = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if b[i] == b'"' || b[i] == b'\'' {
            // keep strings intact (import paths live in them)
            let q = b[i];
            let start = i;
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            clean.push_str(&src[start..i.min(b.len())]);
        } else {
            clean.push(b[i] as char);
            i += 1;
        }
    }
    let mut imports = Vec::new();
    let mut pragma = None;
    for stmt in clean.split(';') {
        let t = stmt.trim_start();
        if let Some(rest) = t.strip_prefix("import") {
            if rest.starts_with(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '{' || c == '*') {
                if let Some(q) = rest.find(['"', '\'']) {
                    let qc = rest.as_bytes()[q] as char;
                    if let Some(e) = rest[q + 1..].find(qc) {
                        imports.push(rest[q + 1..q + 1 + e].to_string());
                    }
                }
            }
        } else if let Some(rest) = t.strip_prefix("pragma") {
            if let Some(v) = rest.trim_start().strip_prefix("solidity") {
                pragma = Some(v.trim().to_string());
            }
        }
    }
    (imports, pragma)
}

pub struct Graph {
    pub files: BTreeMap<String, FileNode>,
}

pub struct FileNode {
    pub content: String,
    pub imports: Vec<String>,
    pub pragma: Option<String>,
}

impl Graph {
    pub fn build(p: &Project, roots: &[String]) -> Graph {
        let mut files = BTreeMap::new();
        let mut stack: Vec<String> = roots.to_vec();
        while let Some(u) = stack.pop() {
            if files.contains_key(&u) {
                continue;
            }
            let Some(content) = p.read(&u) else {
                crate::die(&format!("source not found: {u}"));
            };
            let (raw, pragma) = scan(&content);
            let imports: Vec<String> = raw.iter().map(|i| p.resolve_import(&u, i)).collect();
            stack.extend(imports.iter().cloned());
            files.insert(u, FileNode { content, imports, pragma });
        }
        Graph { files }
    }

    pub fn closure(&self, root: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![root.to_string()];
        while let Some(u) = stack.pop() {
            if seen.insert(u.clone()) {
                if let Some(n) = self.files.get(&u) {
                    stack.extend(n.imports.iter().cloned());
                }
            }
        }
        seen
    }

    /// sha256 over the file and its transitive imports (sorted), per file.
    pub fn hashes(&self) -> HashMap<String, String> {
        use sha2::{Digest, Sha256};
        let own: HashMap<&str, [u8; 32]> =
            self.files.iter().map(|(k, v)| (k.as_str(), Sha256::digest(v.content.as_bytes()).into())).collect();
        self.files
            .keys()
            .map(|k| {
                let mut h = Sha256::new();
                for f in self.closure(k) {
                    h.update(f.as_bytes());
                    h.update(own.get(f.as_str()).copied().unwrap_or_default());
                }
                (k.clone(), hex::encode(h.finalize()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_imports() {
        let s = r#"// import "nope.sol";
pragma solidity ^0.8.20;
/* import "no.sol"; */
import "a.sol";
import {X, Y} from './b.sol';
import * as Z from "../c.sol";
string constant s = "import \"x.sol\";";
"#;
        let (i, p) = scan(s);
        assert_eq!(i, vec!["a.sol", "./b.sol", "../c.sol"]);
        assert_eq!(p.as_deref(), Some("^0.8.20"));
        assert_eq!(normalize("src/a/../b/./c.sol"), "src/b/c.sol");
    }
}
