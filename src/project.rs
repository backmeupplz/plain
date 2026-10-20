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
    /// foundry.toml [etherscan]: v2 keys work on every chain, so any entry's key will do
    pub etherscan_key: Option<String>,
    /// what tests may do outside the EVM (ffi, fs_permissions), as configured
    pub access: Access,
    /// run test/**/*.test.ts too (`typescript = true`): they're programs, so only when the project says so
    pub typescript: bool,
    /// forge's `additional_compiler_profiles`: settings a file compiles with instead, when `restrictions` rule
    /// the default ones out for it (each unset setting is the default's)
    pub profiles: Vec<Profile>,
    /// forge's `compilation_restrictions`
    pub restrictions: Vec<Restriction>,
    /// forge's `isolate`: each call a test makes is a transaction of its own
    pub isolate: bool,
    /// forge's `gas_limit` (or `block_gas_limit`): the gas a test gets, and the block's limit
    pub gas_limit: u64,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub optimizer: bool,
    pub runs: u64,
    pub via_ir: bool,
    pub evm_version: Option<String>,
}

/// What a file (and every file importing it) may compile with: via-IR or not, a range of optimizer runs.
#[derive(Clone, Debug)]
pub struct Restriction {
    /// a glob, as forge's --match-path
    pub paths: String,
    pub via_ir: Option<bool>,
    pub min_runs: Option<u64>,
    pub max_runs: Option<u64>,
}

impl Restriction {
    pub fn allows(&self, p: &Profile) -> bool {
        self.via_ir.is_none_or(|v| v == p.via_ir)
            && self.min_runs.is_none_or(|m| p.runs >= m && (m == 0 || p.optimizer))
            && self.max_runs.is_none_or(|m| p.runs <= m)
    }
}

/// What test and script code may reach outside the EVM. Nothing unless the config opens it, like forge:
/// `ffi = true` for vm.ffi, `fs_permissions` for the file cheatcodes.
#[derive(Clone, Default, Debug)]
pub struct Access {
    pub ffi: bool,
    /// (path, read, write), resolved
    pub fs: Vec<(PathBuf, bool, bool)>,
    /// never writable, whatever fs_permissions says: plain's state, git's, the config (writing foundry.toml
    /// could turn ffi on), .env
    pub protected: Vec<PathBuf>,
}

/// `p` with `.`, `..` and symlinks resolved; for a path that doesn't exist yet, its nearest existing ancestor's.
pub fn resolve(p: &Path) -> PathBuf {
    let mut norm = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                norm.pop();
            }
            std::path::Component::CurDir => {}
            c => norm.push(c),
        }
    }
    let (mut base, mut rest) = (norm, vec![]);
    while !base.exists() {
        match base.file_name() {
            Some(n) => rest.push(n.to_owned()),
            None => break,
        }
        base.pop();
    }
    let mut out = std::fs::canonicalize(&base).unwrap_or(base);
    out.extend(rest.iter().rev());
    out
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
        out.push_str(&var(&rest[i + 2..i + j]).unwrap_or_default());
        rest = &rest[i + j + 1..];
    }
    out + rest
}

static DOTENV: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();

/// An environment variable: the process's own, else the project's .env.
pub fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().or_else(|| DOTENV.get()?.get(k).cloned())
}

/// The project's .env, for code that is the project's own anyway (TypeScript tests, vm.ffi).
pub fn dotenv() -> Vec<(String, String)> {
    DOTENV.get().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()
}

/// The project root: the nearest folder up with plain.toml or foundry.toml, else the current one.
pub fn root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    cwd.ancestors().find(|d| d.join("plain.toml").exists() || d.join("foundry.toml").exists()).unwrap_or(&cwd).to_path_buf()
}

/// Reads the project's .env, as forge does, but never exports it: a cloned repo's .env can't change how plain or
/// the programs it starts behave (PLAIN_HOME, PATH, DYLD_INSERT_LIBRARIES, NODE_OPTIONS...). Its values answer
/// `var`: keys, RPC URLs, `${VAR}` in the config, the env cheatcodes.
pub fn load_env() {
    let mut vars = HashMap::new();
    for line in std::fs::read_to_string(root().join(".env")).unwrap_or_default().lines() {
        let line = line.trim();
        let Some((k, v)) = line.strip_prefix("export ").unwrap_or(line).split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let v = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or_default(),
            _ => v.split(" #").next().unwrap_or_default().trim_end(),
        };
        if !k.is_empty() && !k.starts_with('#') {
            vars.insert(k.to_string(), v.to_string());
        }
    }
    DOTENV.set(vars).ok();
}

/// ~/.plain/config.toml: settings for every project, like the Etherscan key the menu saves.
pub fn user_config() -> Table {
    read_toml(&crate::solc::home().join("config.toml")).unwrap_or_default()
}

/// Saves one setting in ~/.plain/config.toml, readable only by you (it holds API keys).
pub fn set_user_config(key: &str, value: &str) {
    let path = crate::solc::home().join("config.toml");
    let mut t = user_config();
    t.insert(key.into(), value.into());
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(&path, toml::to_string(&t).unwrap()).unwrap_or_else(|e| crate::die(&format!("{}: {e}", path.display())));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    }
}

impl Project {
    pub fn load() -> Project {
        let root = root();
        // foundry.toml [profile.default] first, plain.toml (same keys, top level) overrides.
        let mut cfg = Table::new();
        let mut networks = BTreeMap::new();
        let mut etherscan_key = None;
        if let Some(f) = read_toml(&root.join("foundry.toml")) {
            for (_, e) in f.get("etherscan").and_then(|e| e.as_table()).into_iter().flatten() {
                etherscan_key = etherscan_key.or(e.get("key").and_then(|k| k.as_str()).map(expand_env).filter(|k| !k.is_empty()));
            }
            if let Some(p) = f.get("profile").and_then(|p| p.get("default")).and_then(|p| p.as_table()) {
                cfg.extend(p.clone());
            }
            // FOUNDRY_PROFILE=ci picks [profile.ci] over the default, as forge does (its fuzz and invariant tables too)
            if let Some(name) = var("FOUNDRY_PROFILE").filter(|n| !n.is_empty() && n != "default") {
                match f.get("profile").and_then(|p| p.get(&name)).and_then(|p| p.as_table()) {
                    Some(p) => {
                        for (k, v) in p {
                            match (cfg.get_mut(k).and_then(|c| c.as_table_mut()), v.as_table()) {
                                (Some(c), Some(t)) => c.extend(t.clone()),
                                _ => {
                                    cfg.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                    None => crate::die(&format!("FOUNDRY_PROFILE={name}, but foundry.toml has no [profile.{name}]")),
                }
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
        // a remapping into a submodule that isn't checked out can't resolve anything, and it would shadow
        // node_modules/, lib/ and the built-in forge-std (OpenZeppelin's own maps forge-std to its lib/)
        remappings.retain(|(_, t)| root.join(t).exists());

        // forge: off unless optimizer_runs is set; without foundry.toml: on
        let optimizer = b("optimizer").unwrap_or(!root.join("foundry.toml").exists() || cfg.contains_key("optimizer_runs"));
        let runs = cfg.get("optimizer_runs").and_then(|v| v.as_integer()).unwrap_or(200) as u64;
        let via_ir = b("via_ir").unwrap_or(false);
        Project {
            dirs: vec![src, test, script],
            remappings,
            solc: s("solc_version").or(s("solc")).filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())),
            optimizer,
            runs,
            via_ir,
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
            etherscan_key,
            access: Access {
                ffi: b("ffi").unwrap_or(false),
                // forge's fs_permissions: [{ access = "read" | "write" | "read-write" | true | false, path = "./x" }]
                fs: cfg
                    .get("fs_permissions")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|e| {
                        let path = resolve(&root.join(e.get("path")?.as_str()?));
                        let (read, write) = match e.get("access")? {
                            toml::Value::Boolean(b) => (*b, *b),
                            a => match a.as_str()? {
                                "read" => (true, false),
                                "write" => (false, true),
                                "read-write" => (true, true),
                                _ => (false, false),
                            },
                        };
                        Some((path, read, write))
                    })
                    .collect(),
                protected: [".git", ".plain", "foundry.toml", "plain.toml", ".env"].iter().map(|f| resolve(&root.join(f))).collect(),
            },
            typescript: b("typescript").unwrap_or(false),
            profiles: cfg.get("additional_compiler_profiles").and_then(|v| v.as_array()).into_iter().flatten().map(|t| {
                let t = t.as_table().unwrap_or_else(|| crate::die("additional_compiler_profiles: each entry is a table"));
                if let Some(k) = t.keys().find(|k| !["name", "optimizer", "optimizer_runs", "via_ir", "evm_version"].contains(&k.as_str())) {
                    crate::die(&format!("additional_compiler_profiles: plain doesn't support {k:?} yet"));
                }
                Profile {
                    name: t.get("name").and_then(|v| v.as_str()).unwrap_or_else(|| crate::die("additional_compiler_profiles: a profile needs a name")).into(),
                    optimizer: t.get("optimizer").and_then(|v| v.as_bool()).unwrap_or(optimizer),
                    runs: t.get("optimizer_runs").and_then(|v| v.as_integer()).map_or(runs, |n| n as u64),
                    via_ir: t.get("via_ir").and_then(|v| v.as_bool()).unwrap_or(via_ir),
                    evm_version: t.get("evm_version").and_then(|v| v.as_str()).map(String::from).or_else(|| s("evm_version")),
                }
            }).collect(),
            restrictions: cfg.get("compilation_restrictions").and_then(|v| v.as_array()).into_iter().flatten().map(|t| {
                let t = t.as_table().unwrap_or_else(|| crate::die("compilation_restrictions: each entry is a table"));
                if let Some(k) = t.keys().find(|k| !["paths", "via_ir", "optimizer_runs", "min_optimizer_runs", "max_optimizer_runs"].contains(&k.as_str())) {
                    crate::die(&format!("compilation_restrictions: plain doesn't support {k:?} yet"));
                }
                let n = |k: &str| t.get(k).and_then(|v| v.as_integer()).map(|n| n as u64);
                Restriction {
                    paths: t.get("paths").and_then(|v| v.as_str()).unwrap_or_else(|| crate::die("compilation_restrictions: each needs paths")).into(),
                    via_ir: t.get("via_ir").and_then(|v| v.as_bool()),
                    min_runs: n("min_optimizer_runs").or(n("optimizer_runs")),
                    max_runs: n("max_optimizer_runs").or(n("optimizer_runs")),
                }
            }).collect(),
            isolate: b("isolate").unwrap_or(false),
            // a number, or a string holding one (toml has no u64)
            gas_limit: ["block_gas_limit", "gas_limit"]
                .iter()
                .find_map(|k| match cfg.get(*k)? {
                    toml::Value::Integer(n) => u64::try_from(*n).ok(),
                    toml::Value::String(s) if s == "max" => Some(i64::MAX as u64),
                    toml::Value::String(s) => s.parse().ok(),
                    _ => None,
                })
                .unwrap_or(crate::evm::GAS_LIMIT),
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

    /// sha256 of each file's content.
    pub fn own_hashes(&self) -> HashMap<String, String> {
        use sha2::{Digest, Sha256};
        self.files.iter().map(|(k, v)| (k.clone(), hex::encode(Sha256::digest(v.content.as_bytes())))).collect()
    }

    /// sha256 over the file itself and its transitive imports (sorted) with the hash `part` gives each of them. The
    /// file is named first: files that import each other have the same closure, not the same artifacts.
    pub fn closure_hash(&self, root: &str, part: &mut dyn FnMut(&str) -> String) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(root.as_bytes());
        h.update(b"\0");
        for f in self.closure(root) {
            h.update(f.as_bytes());
            h.update(part(&f).as_bytes());
        }
        hex::encode(h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrictions() {
        // veydrift's: an optimized default (runs 1) and an unoptimized "tests" profile; max_optimizer_runs = 0 picks the latter
        let default = Profile { name: "default".into(), optimizer: true, runs: 1, via_ir: true, evm_version: None };
        let tests = Profile { optimizer: false, runs: 0, ..default.clone() };
        let r = |min, max, via_ir| Restriction { paths: "test/X.t.sol".into(), via_ir, min_runs: min, max_runs: max };
        assert!(!r(None, Some(0), None).allows(&default) && r(None, Some(0), None).allows(&tests));
        assert!(r(Some(1), None, None).allows(&default) && !r(Some(1), None, None).allows(&tests), "min runs needs the optimizer on");
        assert!(r(Some(0), None, None).allows(&tests) && !r(None, None, Some(false)).allows(&tests));
    }
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
        // files that import each other: one closure, two artifacts' worth of hashes
        let node = |imports: &[&str]| FileNode { content: String::new(), imports: imports.iter().map(|s| s.to_string()).collect(), pragma: None };
        let g = Graph { files: [("a.sol".to_string(), node(&["b.sol"])), ("b.sol".to_string(), node(&["a.sol"]))].into() };
        assert_eq!(g.closure("a.sol"), g.closure("b.sol"));
        assert_ne!(g.closure_hash("a.sol", &mut |_| String::new()), g.closure_hash("b.sol", &mut |_| String::new()));
        assert_eq!(p.as_deref(), Some("^0.8.20"));
        assert_eq!(normalize("src/a/../b/./c.sol"), "src/b/c.sol");
    }
}
