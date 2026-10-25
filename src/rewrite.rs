//! Test-only source rewrites that cut solc time without touching what gets deployed:
//! - `new X(...)` inside test contracts deploys X's compiled artifact (via the getCode cheatcode)
//!   instead of embedding X's bytecode, so X is optimized once, in its own job, not in every test.
//! - big test files compile as shards: shard k keeps its share of test functions public and turns
//!   the others `internal` (solc drops unused internal functions), so one file uses several cores.
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum K {
    Ident,
    Str,
    Num,
    Punct,
}

#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub k: K,
    pub s: usize,
    pub e: usize,
}

fn skip_str(b: &[u8], mut i: usize) -> usize {
    let q = b[i];
    i += 1;
    while i < b.len() && b[i] != q && b[i] != b'\n' {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(b.len())
}

/// Tokens of a Solidity source; comments dropped, `hex"..."`/`unicode"..."` are strings.
pub fn lex(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let s = i;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else if c == b'"' || c == b'\'' {
            i = skip_str(b, i);
            out.push(Tok { k: K::Str, s, e: i });
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || (b[i] == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()))) {
                i += 1;
            }
            out.push(Tok { k: K::Num, s, e: i });
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            if matches!(&src[s..i], "hex" | "unicode") && matches!(b.get(i), Some(b'"' | b'\'')) {
                i = skip_str(b, i);
                out.push(Tok { k: K::Str, s, e: i });
            } else {
                out.push(Tok { k: K::Ident, s, e: i });
            }
        } else {
            // one token per char; non-ASCII only appears in strings/comments in valid code
            i += src[i..].chars().next().map_or(1, char::len_utf8);
            out.push(Tok { k: K::Punct, s, e: i });
        }
    }
    out
}

pub struct Contract {
    pub name: String,
    /// "contract", "library" or "interface"
    pub kind: String,
    pub is_abstract: bool,
    pub bases: Vec<String>,
    /// token indices of the body's `{` and `}`
    pub body: (usize, usize),
    /// constructor parameter list (token indices of its parens) and whether it's payable
    pub ctor: Option<(usize, usize, bool)>,
}

pub struct Func {
    pub name: String,
    pub contract: usize,
    /// visibility keyword token
    pub vis: Option<usize>,
    pub payable: Option<usize>,
    pub overrides: bool,
    /// source bytes of the whole definition (shard balancing)
    pub size: usize,
    /// tokens of the whole definition
    pub toks: (usize, usize),
}

/// What the rewrites need to know about one file.
pub struct Unit {
    pub toks: Vec<Tok>,
    /// matching bracket for every ( ) [ ] { } token
    pub pair: Vec<usize>,
    pub contracts: Vec<Contract>,
    pub funcs: Vec<Func>,
    /// names introduced by `import ... as X`
    pub aliases: Vec<String>,
    /// has a library with public/external functions (needs linking)
    pub ext_lib: bool,
    /// token ranges of function/modifier/constructor bodies in contracts (not libraries): code that only
    /// reaches another contract's bytecode through inheritance or `new` (see `interface_hash`)
    pub bodies: Vec<(usize, usize)>,
    /// bodies of `pure` functions (can't call the view helper that replaces `type(X).creationCode`)
    pub pure_bodies: Vec<(usize, usize)>,
}

impl Unit {
    pub fn parse(src: &str) -> Option<Unit> {
        let toks = lex(src);
        let t = |i: usize| -> &str { toks.get(i).map_or("", |x| if x.k == K::Punct && x.e - x.s != 1 { "" } else { &src[x.s..x.e] }) };
        let mut pair = vec![usize::MAX; toks.len()];
        let mut stack = vec![];
        for i in 0..toks.len() {
            if toks[i].k != K::Punct {
                continue;
            }
            match t(i) {
                "(" | "[" | "{" => stack.push(i),
                c @ (")" | "]" | "}") => {
                    let o = stack.pop()?;
                    if !matches!((t(o), c), ("(", ")") | ("[", "]") | ("{", "}")) {
                        return None;
                    }
                    pair[o] = i;
                    pair[i] = o;
                }
                _ => {}
            }
        }
        if !stack.is_empty() {
            return None;
        }
        let mut u = Unit { toks: vec![], pair, contracts: vec![], funcs: vec![], aliases: vec![], ext_lib: false, bodies: vec![], pure_bodies: vec![] };
        let n = toks.len();
        let mut i = 0;
        while i < n {
            match t(i) {
                "import" => {
                    while i < n && t(i) != ";" {
                        if t(i) == "as" && toks.get(i + 1).is_some_and(|x| x.k == K::Ident) {
                            u.aliases.push(t(i + 1).to_string());
                        }
                        i += 1;
                    }
                }
                "contract" | "library" | "interface" if toks.get(i + 1).is_some_and(|x| x.k == K::Ident) => {
                    let kind = t(i).to_string();
                    let is_abstract = i > 0 && t(i - 1) == "abstract";
                    let name = t(i + 1).to_string();
                    let mut j = i + 2;
                    let mut bases = vec![];
                    if t(j) == "is" {
                        j += 1;
                        while j < n && t(j) != "{" {
                            if toks[j].k == K::Ident && (t(j - 1) == "is" || t(j - 1) == ",") {
                                bases.push(t(j).to_string());
                            }
                            j = if t(j) == "(" { u.pair[j] + 1 } else { j + 1 };
                        }
                    }
                    if t(j) != "{" {
                        return None;
                    }
                    let end = u.pair[j];
                    let ci = u.contracts.len();
                    let mut ctor = None;
                    let mut k = j + 1;
                    while k < end {
                        if t(k) == "constructor" && t(k + 1) == "(" {
                            let close = u.pair[k + 1];
                            let mut h = close + 1;
                            let mut payable = false;
                            while h < end && t(h) != "{" {
                                payable |= t(h) == "payable";
                                h = if t(h) == "(" { u.pair[h] + 1 } else { h + 1 };
                            }
                            ctor = Some((k + 1, close, payable));
                            if kind == "contract" && t(h) == "{" {
                                u.bodies.push((h, u.pair[h]));
                            }
                            k = h;
                        } else if t(k) == "function" && toks[k + 1].k == K::Ident && t(k + 2) == "(" {
                            let mut f = Func { name: t(k + 1).to_string(), contract: ci, vis: None, payable: None, overrides: false, size: 0, toks: (k, k) };
                            let mut h = u.pair[k + 2] + 1;
                            let mut pure = false;
                            while h < end && t(h) != "{" && t(h) != ";" {
                                pure |= t(h) == "pure";
                                match t(h) {
                                    "public" | "external" | "internal" | "private" => f.vis = Some(h),
                                    "payable" => f.payable = Some(h),
                                    "override" => f.overrides = true,
                                    _ => {}
                                }
                                h = if t(h) == "(" { u.pair[h] + 1 } else { h + 1 };
                            }
                            let last = if t(h) == "{" { u.pair[h] } else { h };
                            if kind == "contract" && t(h) == "{" {
                                u.bodies.push((h, last));
                                if pure {
                                    u.pure_bodies.push((h, last));
                                }
                            }
                            f.size = toks[last.min(n - 1)].e - toks[k].s;
                            f.toks = (k, last.min(n - 1));
                            if kind == "library" && f.vis.is_some_and(|v| matches!(t(v), "public" | "external")) {
                                u.ext_lib = true;
                            }
                            u.funcs.push(f);
                            k = last + 1;
                        } else if kind == "contract" && matches!(t(k), "modifier" | "fallback" | "receive") && (t(k) != "modifier" || toks.get(k + 1).is_some_and(|x| x.k == K::Ident)) {
                            let mut h = k + 1;
                            while h < end && t(h) != "{" && t(h) != ";" {
                                h = if t(h) == "(" { u.pair[h] + 1 } else { h + 1 };
                            }
                            if t(h) == "{" {
                                u.bodies.push((h, u.pair[h]));
                                k = u.pair[h] + 1;
                            } else {
                                k = h + 1;
                            }
                        } else if t(k) == "{" {
                            k = u.pair[k] + 1;
                        } else {
                            k += 1;
                        }
                    }
                    u.contracts.push(Contract { name, kind, is_abstract, bases, body: (j, end), ctor });
                    i = end;
                }
                "{" => i = u.pair[i],
                _ => {}
            }
            i += 1;
        }
        u.toks = toks;
        Some(u)
    }
}

impl Unit {
    /// Hash of what other files' compiled code can depend on without inheriting or creating this
    /// file's contracts: everything but contract function/modifier/constructor bodies (libraries, free
    /// functions and constants stay whole) and comments.
    pub fn interface_hash(&self, src: &str) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        let mut skip = self.bodies.iter().peekable();
        let mut i = 0;
        while i < self.toks.len() {
            if let Some(&&(a, b)) = skip.peek() {
                if i == a {
                    h.update(b"{}");
                    i = b + 1;
                    skip.next();
                    continue;
                }
            }
            h.update(&src.as_bytes()[self.toks[i].s..self.toks[i].e]);
            h.update(b" ");
            i += 1;
        }
        hex::encode(h.finalize())
    }

    /// Names whose code compiles into this file's contracts: bases, contracts created with `new`,
    /// and `type(X).creationCode`/`runtimeCode` targets.
    pub fn code_refs(&self, src: &str) -> Vec<String> {
        let mut out: Vec<String> = self.contracts.iter().flat_map(|c| c.bases.iter().cloned()).chain(self.created(src)).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Contracts whose creation code the unit uses: `new X`, `type(X).creationCode/runtimeCode`.
    pub fn created(&self, src: &str) -> Vec<String> {
        self.created_where(src, &|_| true)
    }

    /// Contracts whose creation code the unit's libraries, free functions and constants use: code that other
    /// files compile in whole, unlike contract function bodies (see `interface_hash`).
    pub fn loose_created(&self, src: &str) -> Vec<String> {
        self.created_where(src, &|i| !self.bodies.iter().any(|&(a, b)| i > a && i < b))
    }

    fn created_where(&self, src: &str, at: &dyn Fn(usize) -> bool) -> Vec<String> {
        let t = |i: usize| self.toks.get(i).map_or("", |x| &src[x.s..x.e]);
        let mut out = vec![];
        for i in (0..self.toks.len()).filter(|i| at(*i)) {
            if t(i) == "new" && self.toks.get(i + 1).is_some_and(|x| x.k == K::Ident) {
                out.push(t(i + 1).to_string());
            } else if t(i) == "type" && t(i + 1) == "(" && t(i + 3) == ")" && t(i + 4) == "." && matches!(t(i + 5), "creationCode" | "runtimeCode") {
                out.push(t(i + 2).to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// A replacement of src[s..e].
pub type Edit = (usize, usize, String);

pub fn apply(src: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(src.len() + 1024);
    let mut pos = 0;
    for (s, e, r) in edits {
        out.push_str(&src[pos..s]);
        out.push_str(&r);
        pos = e;
    }
    out.push_str(&src[pos..]);
    out
}

/// A contract tests may deploy from its artifact.
pub struct Target {
    /// artifact id, "path:Name"
    pub id: String,
    /// generated abstract contract whose `__plainCtor` mirrors the constructor (see `ctor_check`)
    pub check: String,
    /// source unit holding `check`
    pub path: String,
    pub payable: bool,
}

/// Source of an abstract contract inheriting `name` whose `__plainCtor` has its constructor's
/// parameters: `abi.encodeCall(check.__plainCtor, (args))` type-checks and encodes rewritten
/// `new name(args)` arguments exactly like the constructor call, without compiling `name` itself.
pub fn ctor_check(check: &str, import: &str, name: &str, params: &str) -> String {
    format!("// generated by plain for tests: checks and encodes `new {name}(...)` arguments\nimport \"{import}\";\n\nabstract contract {check} is {name} {{\n    function __plainCtor{params} external {{}}\n}}\n")
}

/// Rewrites `new X(...)` sites inside the contracts `in_test` accepts, for the contracts `target`
/// knows. Returns edits (helper function and imports appended at the end, so line numbers stay)
/// and, per contract index, the targets its sites deploy.
pub fn link_news<'t>(src: &str, u: &Unit, file: &str, in_test: &dyn Fn(usize) -> bool, target: &dyn Fn(&str) -> Option<&'t Target>, memory_safe: bool) -> (Vec<Edit>, Vec<(usize, &'t Target)>) {
    let helper = format!("__plainNew_{}", &hex::encode(alloy_primitives::keccak256(file.as_bytes()))[..8]);
    let used = std::cell::RefCell::new(vec![]);
    let record = |name: &str| {
        let t = target(name)?;
        used.borrow_mut().push(t);
        Some(t)
    };
    let mut edits = vec![];
    let mut ids: Vec<(usize, &Target)> = vec![];
    for (ci, c) in u.contracts.iter().enumerate() {
        if !in_test(ci) {
            continue;
        }
        let mut i = c.body.0;
        while i < c.body.1 {
            if let Some((end, text)) = site(src, u, i, &helper, &record) {
                edits.push((u.toks[i].s, u.toks[end].e, text));
                i = end + 1;
            } else {
                i += 1;
            }
        }
        ids.extend(used.borrow_mut().drain(..).map(|t| (ci, t)));
    }
    if !edits.is_empty() {
        let safe = if memory_safe { " (\"memory-safe\")" } else { "" };
        let mut tail = format!(
            "\n\n// added by plain for tests: deploy from the compiled artifact instead of embedding its bytecode\nfunction {helper}Code(string memory id) view returns (bytes memory) {{\n    (bool ok, bytes memory code) = address(0x7109709ECfa91a80626fF3989D68f67F5b1DD12D).staticcall(abi.encodeWithSignature(\"getCode(string)\", id));\n    require(ok, \"plain: getCode failed\");\n    return abi.decode(code, (bytes));\n}}\n\nfunction {helper}New(string memory id, bytes memory args) returns (address) {{\n    return {helper}(0, bytes32(0), false, id, args);\n}}\n\nfunction {helper}(uint256 value, bytes32 salt, bool useSalt, string memory id, bytes memory args) returns (address addr) {{\n    bytes memory code = {helper}Code(id);\n    assembly{safe} {{\n        // args comes from abi.encodeCall: drop its 4-byte selector\n        mstore(add(args, 4), sub(mload(args), 4))\n        args := add(args, 4)\n    }}\n    code = abi.encodePacked(code, args);\n    assembly{safe} {{\n        switch useSalt\n        case 0 {{ addr := create(value, add(code, 0x20), mload(code)) }}\n        default {{ addr := create2(value, add(code, 0x20), mload(code), salt) }}\n        if iszero(addr) {{\n            let p := mload(0x40)\n            returndatacopy(p, 0, returndatasize())\n            revert(p, returndatasize())\n        }}\n    }}\n}}\n"
        );
        let mut imports: Vec<&Target> = ids.iter().map(|(_, t)| *t).collect();
        imports.sort_by(|a, b| a.check.cmp(&b.check));
        imports.dedup_by(|a, b| a.check == b.check);
        for t in imports {
            tail += &format!("import {{{}}} from \"{}\";\n", t.check, t.path);
        }
        edits.push((src.len(), src.len(), tail));
    }
    (edits, ids)
}

/// `new X{opts}(args)` at token i, if rewritable: (index of the closing paren, replacement).
fn site<'t>(src: &str, u: &Unit, i: usize, helper: &str, target: &dyn Fn(&str) -> Option<&'t Target>) -> Option<(usize, String)> {
    let toks = &u.toks;
    let t = |i: usize| -> &str { toks.get(i).map_or("", |x| if x.k == K::Punct && x.e - x.s != 1 { "" } else { &src[x.s..x.e] }) };
    // type(X).creationCode in a function body (not a constant, which can't call functions)
    if t(i) == "type" && t(i + 1) == "(" && t(i + 3) == ")" && t(i + 4) == "." && t(i + 5) == "creationCode" && u.bodies.iter().any(|&(a, b)| a < i && i < b) && !u.pure_bodies.iter().any(|&(a, b)| a < i && i < b) {
        let tg = target(t(i + 2))?;
        return Some((i + 5, format!("{helper}Code(\"{}\")", tg.id)));
    }
    if t(i) != "new" || toks.get(i + 1)?.k != K::Ident {
        return None;
    }
    let name = t(i + 1);
    let mut j = i + 2;
    let (mut value, mut salt) = (None, None);
    if t(j) == "{" {
        for (a, b) in split_args(u, src, j + 1, u.pair[j]) {
            if b < a + 3 || t(a + 1) != ":" {
                return None;
            }
            match t(a) {
                "value" => value = Some((a + 2, b)),
                "salt" => salt = Some((a + 2, b)),
                _ => return None,
            }
        }
        j = u.pair[j] + 1;
    }
    if t(j) != "(" {
        return None;
    }
    let close = u.pair[j];
    if t(j + 1) == "{" && close == u.pair[j + 1] + 1 {
        return None; // named arguments: abi.encodeCall takes them positionally only
    }
    let tg = target(name)?;
    if value.is_some() && !tg.payable {
        return None; // keep solc's "constructor is not payable" error
    }
    let sub = |a: usize, b: usize| rewrite_range(src, u, a, b, helper, target);
    let value = value.map_or("0".to_string(), |(a, b)| sub(a, b));
    let (salt, use_salt) = salt.map_or(("bytes32(0)".to_string(), false), |(a, b)| (sub(a, b), true));
    let args = sub(j + 1, close);
    let ctor = format!("\"{}\", abi.encodeCall({}.__plainCtor, ({args}))", tg.id, tg.check);
    // the common `new X(args)` takes the two-argument helper: fewer stack slots at the call site, where via-IR's
    // stack can be one variable short of "too deep" in a big test function
    Some((close, match (value.as_str(), use_salt) {
        ("0", false) => format!("{name}(payable({helper}New({ctor})))"),
        _ => format!("{name}(payable({helper}({value}, {salt}, {use_salt}, {ctor})))"),
    }))
}

/// Source text of tokens [a, b) with nested `new` sites rewritten.
fn rewrite_range<'t>(src: &str, u: &Unit, a: usize, b: usize, helper: &str, target: &dyn Fn(&str) -> Option<&'t Target>) -> String {
    if a >= b {
        return String::new();
    }
    let (start, end) = (u.toks[a].s, u.toks[b - 1].e);
    let mut out = String::new();
    let mut pos = start;
    let mut i = a;
    while i < b {
        if let Some((close, text)) = site(src, u, i, helper, target) {
            out.push_str(&src[pos..u.toks[i].s]);
            out.push_str(&text);
            pos = u.toks[close].e;
            i = close + 1;
        } else {
            i += 1;
        }
    }
    out.push_str(&src[pos..end]);
    out
}

/// Comma-separated token ranges between a and b (exclusive), respecting nesting.
fn split_args(u: &Unit, src: &str, a: usize, b: usize) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut start = a;
    let mut i = a;
    while i < b {
        let x = u.toks[i];
        let c = if x.k == K::Punct { &src[x.s..x.e] } else { "" };
        if matches!(c, "(" | "[" | "{") {
            i = u.pair[i] + 1;
            continue;
        }
        if c == "," {
            out.push((start, i));
            start = i + 1;
        }
        i += 1;
    }
    if start < b {
        out.push((start, b));
    }
    out
}

/// Test functions (public/external `test*`/`invariant*`) in contracts `in_test` accepts, by name with total size.
/// None if the file can't be sharded safely (an `override`n test function).
/// Test functions as forge finds them: `test*`, and invariants (`invariant*`, `statefulFuzz*`).
pub fn is_test_name(n: &str) -> bool {
    n.starts_with("test") || is_invariant_name(n)
}

pub fn is_invariant_name(n: &str) -> bool {
    n.starts_with("invariant") || n.starts_with("statefulFuzz")
}

pub fn test_funcs(src: &str, u: &Unit, in_test: &dyn Fn(usize) -> bool) -> Option<Vec<(String, usize)>> {
    let mut out: Vec<(String, usize)> = vec![];
    for f in &u.funcs {
        let public = f.vis.is_some_and(|v| matches!(&src[u.toks[v].s..u.toks[v].e], "public" | "external"));
        if !public || !in_test(f.contract) || !is_test_name(&f.name) {
            continue;
        }
        if f.overrides {
            return None;
        }
        match out.iter_mut().find(|(n, _)| *n == f.name) {
            Some(e) => e.1 += f.size,
            None => out.push((f.name.clone(), f.size)),
        }
    }
    Some(out)
}

/// Test functions nothing else in the file names (no calls, no `this.test_x.selector`): a unit that doesn't
/// run them can leave them out entirely, so solc neither analyzes nor compiles them.
pub fn removable(src: &str, u: &Unit, tests: &[(String, usize)]) -> HashSet<String> {
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for t in u.toks.iter().filter(|t| t.k == K::Ident) {
        *uses.entry(&src[t.s..t.e]).or_default() += 1;
    }
    let mut defs: HashMap<&str, usize> = HashMap::new();
    for f in &u.funcs {
        *defs.entry(f.name.as_str()).or_default() += 1;
    }
    tests.iter().filter(|(n, _)| uses.get(n.as_str()) == defs.get(n.as_str())).map(|(n, _)| n.clone()).collect()
}

/// Hash of each test function's text (overloads together), to tell which tests an edit touched.
pub fn test_hashes(src: &str, u: &Unit, in_test: &dyn Fn(usize) -> bool) -> std::collections::BTreeMap<String, String> {
    use sha2::Digest;
    let mut h: std::collections::BTreeMap<String, sha2::Sha256> = Default::default();
    for f in u.funcs.iter().filter(|f| in_test(f.contract) && is_test_name(&f.name)) {
        h.entry(f.name.clone()).or_default().update(&src[u.toks[f.toks.0].s..u.toks[f.toks.1].e]);
    }
    h.into_iter().map(|(n, h)| (n, hex::encode(h.finalize()))).collect()
}

/// Edits for a unit running only the tests `keep` accepts: other removable tests are cut out with the comments
/// before them (whole, so the rest of the file reads the same whatever their size), the rest made internal
/// (and non-payable).
pub fn unit_edits(src: &str, u: &Unit, in_test: &dyn Fn(usize) -> bool, keep: &dyn Fn(&str) -> bool, removable: &HashSet<String>) -> Vec<Edit> {
    let mut edits = vec![];
    for f in &u.funcs {
        let Some(v) = f.vis else { continue };
        let vis = &src[u.toks[v].s..u.toks[v].e];
        if !matches!(vis, "public" | "external") || !in_test(f.contract) || !is_test_name(&f.name) || keep(&f.name) {
            continue;
        }
        if removable.contains(&f.name) {
            let (s, e) = (u.toks[f.toks.0 - 1].e, u.toks[f.toks.1].e);
            edits.push((s, e, String::new()));
            continue;
        }
        edits.push((u.toks[v].s, u.toks[v].e, "internal".into()));
        if let Some(p) = f.payable {
            edits.push((u.toks[p].s, u.toks[p].e, String::new()));
        }
    }
    edits
}

/// Split test functions (name, size) into k groups of similar total size; returns name -> shard.
pub fn assign(tests: &[(String, usize)], k: usize) -> HashMap<String, usize> {
    let mut sorted: Vec<&(String, usize)> = tests.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut load = vec![0usize; k];
    let mut out = HashMap::new();
    for (name, size) in sorted {
        // invariants share one campaign setup: keep them together in shard 0
        let s = if is_invariant_name(name) { 0 } else { (0..k).min_by_key(|&i| (load[i], i)).unwrap() };
        load[s] += size;
        out.insert(name.clone(), s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"pragma solidity ^0.8.20;
import {Test} from "forge-std/Test.sol";
import {Vault as V2} from "../src/Vault.sol";
import "../src/Token.sol";

contract Helper { function make() external returns (Token) { return new Token(1); } }

contract VaultTest is Test {
    Token t = new Token(5);
    function setUp() public {
        // new Token(9) in a comment
        string memory s = "new Token(7)";
        t = new Token{salt: bytes32(uint256(1)), value: 1 ether}(uint256(7) + 1, address(new Token(2)));
        bytes memory b = new bytes(4);
        new Token("lit");
        new Token({x: 1});
        new Local();
    }
    function test_A() public payable { new Token(0x1234); bytes memory c = type(Token).creationCode; }
    function testCode() public pure returns (bytes memory) { return type(Token).creationCode; }
    function testB(uint x) external view returns (uint) { return x; }
    function invariant_C() public {}
    function helper() internal {}
}
"#;

    #[test]
    fn parse_and_link() {
        let token = Target { id: "src/Token.sol:Token".into(), check: "__PlainCtor_t".into(), path: "__plain__/src/Token.sol/Token.sol".into(), payable: false };
        let target = |n: &str| (n == "Token").then_some(&token);
        let u = Unit::parse(SRC).unwrap();
        assert_eq!(u.contracts.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Helper", "VaultTest"]);
        assert_eq!(u.contracts[1].bases, ["Test"]);
        assert_eq!(u.aliases, ["V2"]);
        let in_test = |ci: usize| ci == 1;
        let (edits, ids) = link_news(SRC, &u, "test/V.t.sol", &in_test, &target, true);
        assert_eq!(ids.len(), 6, "state var, salted outer + inner, string literal, hex literal, creationCode");
        let out = apply(SRC, edits);
        let call = |args: &str| format!("\"src/Token.sol:Token\", abi.encodeCall(__PlainCtor_t.__plainCtor, ({args}))))");
        assert!(out.contains(&format!("Token t = Token(payable(__plainNew_")) && out.contains(&call("5")), "{out}");
        assert!(out.contains("new Token{salt: bytes32(uint256(1)), value: 1 ether}("), "value on a non-payable constructor stays for solc to reject: {out}");
        assert!(out.contains(&call("2")), "nested site inside the kept one is still rewritten: {out}");
        assert!(out.contains(&call("\"lit\"")) && out.contains(&call("0x1234")), "literals are converted by encodeCall: {out}");
        assert!(out.contains("new Token({x: 1})") && out.contains("new Local()") && out.contains("new bytes(4)"));
        assert!(out.contains("// new Token(9) in a comment") && out.contains("\"new Token(7)\""), "comments and strings untouched");
        assert!(out.contains("return new Token(1)"), "non-test contracts untouched");
        assert!(out.contains("assembly (\"memory-safe\")"));
        assert!(out.contains("bytes memory c = __plainNew_") && out.contains("Code(\"src/Token.sol:Token\");"), "{out}");
        assert!(out.contains("pure returns (bytes memory) { return type(Token).creationCode; }"), "pure functions keep creationCode");
        assert!(out.ends_with("import {__PlainCtor_t} from \"__plain__/src/Token.sol/Token.sol\";\n"));
        let line = |s: &str| s.lines().position(|l| l.contains("function test_A"));
        assert_eq!(line(&out), line(SRC), "no lines inserted before original code");

        let tests = test_funcs(SRC, &u, &in_test).unwrap();
        assert_eq!(tests.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["test_A", "testCode", "testB", "invariant_C"]);
        let a = assign(&tests, 2);
        assert_eq!(a["invariant_C"], 0);
        let keep = |n: &str| a[n] == 1;
        let rm = removable(SRC, &u, &tests);
        assert!(rm.contains("test_A") && rm.contains("invariant_C"));
        let none = HashSet::new();
        let sh = apply(SRC, unit_edits(SRC, &u, &in_test, &keep, &none));
        assert_eq!(sh.contains("function test_A() internal  {"), a["test_A"] == 0, "{sh}");
        assert!(sh.contains("function invariant_C() internal {}"));
        assert!(sh.contains("function setUp() public"));
        let cut = apply(SRC, unit_edits(SRC, &u, &in_test, &keep, &rm));
        assert_eq!(cut.contains("function test_A"), a["test_A"] == 1, "{cut}");
        assert!(!cut.contains("invariant_C") && cut.contains("function setUp() public") && cut.contains("function helper() internal {}"));
        let h = test_hashes(SRC, &u, &in_test);
        assert_eq!(h.len(), 4);
    }
}
