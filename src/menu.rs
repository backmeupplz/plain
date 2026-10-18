//! `plain` with no arguments, in a terminal: a menu over the commands. Each action prints the command it runs
//! and runs it as a child process, so a failure comes back here and the same command works on its own later.
use crate::project::Project;
use crate::ui::{self, bold, dim, green};
use serde_json::Value;

/// What the menu remembers between runs, in .plain/menu.json.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Last {
    network: Option<String>,
    account: Option<String>,
}

fn say(text: &str) {
    eprintln!("{}", dim(text));
}

fn items(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

/// Runs `plain <args>` after showing it. Returns whether it succeeded.
fn plain(args: &[String]) -> bool {
    let shown: Vec<String> = args.iter().map(|a| if a.is_empty() || a.contains(|c: char| c.is_whitespace() || "'\"$`\\".contains(c)) { format!("'{}'", a.replace('\'', "'\\''")) } else { a.clone() }).collect();
    eprintln!("{}", dim(&format!("$ plain {}", shown.join(" "))));
    let exe = std::env::current_exe().unwrap_or_else(|e| crate::die(&e.to_string()));
    let ok = std::process::Command::new(exe).args(args).status().is_ok_and(|s| s.success());
    eprintln!();
    ok
}

fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

pub fn run() {
    // ctrl-c stops the command that's running and comes back here
    let _ctrl_c = ui::Interrupt::catch();
    let mut p = Project::load();
    if p.project_files().is_empty() && !["foundry.toml", "plain.toml"].iter().any(|f| p.root.join(f).exists()) {
        if !start(&p) {
            return;
        }
        p = Project::load();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    eprintln!("{}{} {}", bold("plain"), green("."), dim(&format!("{} · {}", env!("CARGO_PKG_VERSION"), p.root.display().to_string().replacen(&home, "~", 1))));
    say("Contracts live in src/ and tests in test/; each step shows the command it runs.");
    let path = p.root.join(".plain/menu.json");
    let mut last: Last = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut at = 0;
    loop {
        let list = items(&[
            ("Build", "compile the contracts"),
            ("Test", "run the tests, once or on every save"),
            ("Deploy", "put a contract on a chain, local or live"),
            ("Verify", "publish the source code on block explorers"),
            ("Upgrade", "point a proxy at a new implementation"),
            ("Settings", "deployer accounts, Etherscan key, RPCs"),
            ("Quit", ""),
        ]);
        let pick = ui::select("What next?", &list, at);
        // the cursor stays where it was, for running the same step again
        at = pick.unwrap_or(at);
        match pick {
            Some(0) => {
                say("Compiles src/, test/ and script/ with the solc your pragmas ask for; unchanged files come from the cache.");
                plain(&strs(&["build"]));
            }
            Some(1) => test(),
            Some(2) => deploy(&p, &mut last),
            Some(3) => verify(&p),
            Some(4) => upgrade(&p, &mut last),
            Some(5) => settings(&p),
            _ => return,
        }
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, serde_json::to_vec(&last).unwrap()).ok();
    }
}

/// Outside a project: make one. Returns whether there's one now.
fn start(p: &Project) -> bool {
    eprintln!("{}{} {}", bold("plain"), green("."), dim(&format!("no Solidity project in {}", p.root.display())));
    let list = items(&[("Create a project", "a Counter contract, its test and a .gitignore (plain init)"), ("Show the commands", "plain help"), ("Quit", "")]);
    match ui::select("What next?", &list, 0) {
        Some(0) => {
            let empty = std::fs::read_dir(&p.root).map_or(true, |mut d| d.next().is_none());
            let Some(dir) = ui::input("Folder for it", Some(if empty { "." } else { "my-project" })) else { return false };
            std::fs::create_dir_all(&dir).and_then(|_| std::env::set_current_dir(&dir)).unwrap_or_else(|e| crate::die(&format!("{dir}: {e}")));
            plain(&strs(&["init"]))
        }
        Some(1) => {
            eprint!("{}", crate::USAGE);
            false
        }
        _ => false,
    }
}

fn test() {
    let list = items(&[
        ("Once", "compile what changed, then run every test"),
        ("On every save", "run them again whenever a file changes; ctrl-c stops"),
        ("Only some", "suites and tests whose name contains some text"),
    ]);
    match ui::select("Run the tests", &list, 0) {
        Some(0) => {
            plain(&strs(&["test"]));
        }
        Some(1) => {
            plain(&strs(&["test", "--watch"]));
        }
        Some(2) => {
            if let Some(f) = ui::input("Text in the suite or test name", None).filter(|f| !f.is_empty()) {
                plain(&strs(&["test", &f]));
            }
        }
        _ => {}
    }
}

/// Deployable contracts from the last build: name, file, ABI.
fn contracts(p: &Project) -> Vec<(String, String, Value)> {
    let mut out: Vec<(String, String, Value)> = std::fs::read_dir(p.root.join(".plain/out"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| serde_json::from_slice::<Value>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|a| a["bytecode"].as_str().is_some_and(|b| b.len() > 2) && a["sourceName"].as_str().is_some_and(|s| s.starts_with(&format!("{}/", p.dirs[0]))))
        .map(|a| (a["contractName"].as_str().unwrap_or_default().to_string(), a["sourceName"].as_str().unwrap_or_default().to_string(), a["abi"].clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn has(abi: &Value, name: &str) -> bool {
    abi.as_array().is_some_and(|f| f.iter().any(|x| x["name"] == name))
}

fn is_testnet(net: &str) -> bool {
    ["sepolia", "amoy", "hoodi", "testnet", "local"].iter().any(|t| net.contains(t))
}

/// The network to use; starts a local chain when that's picked and none is running.
fn network(last: &mut Last) -> Option<String> {
    let mut list = vec![("local".to_string(), "a chain on this computer, to try things out".to_string())];
    let mut seen = vec![];
    for testnets in [true, false] {
        for (name, id, _) in crate::rpc::NETWORKS.iter().filter(|n| is_testnet(n.0) == testnets) {
            if !seen.contains(id) {
                seen.push(*id);
                list.push((name.to_string(), format!("{} · chain {id}", if testnets { "testnet" } else { "mainnet, real money" })));
            }
        }
    }
    if let Some(n) = last.network.as_ref().filter(|n| !list.iter().any(|l| &l.0 == *n)) {
        list.insert(1, (n.clone(), "used last time".into()));
    }
    list.push(("Other…".into(), "any chain on chainlist.org by name or chain id, or an RPC URL".into()));
    let at = last.network.as_ref().and_then(|n| list.iter().position(|l| &l.0 == n)).unwrap_or(0);
    let i = ui::select("Which network?", &list, at)?;
    let net = match i == list.len() - 1 {
        true => ui::input("Network name, chain id or RPC URL", None).filter(|n| !n.is_empty())?,
        false => list[i].0.clone(),
    };
    if net == "local" && std::net::TcpStream::connect(("127.0.0.1", 8545)).is_err() {
        crate::node::start(None, 8545, true);
        say("Started a local chain on 127.0.0.1:8545 that lasts as long as this menu (`plain node` runs one on its own).");
    }
    last.network = Some(net.clone());
    Some(net)
}

/// Who signs: a keystore account, or None for $PRIVATE_KEY. Outer None: cancelled.
fn account(last: &mut Last) -> Option<Option<String>> {
    loop {
        let accounts = crate::account::all();
        let mut list: Vec<(String, String)> = accounts.iter().map(|(n, a, _)| (n.clone(), a.map_or("address shown once unlocked".into(), |a| a.to_checksum(None)))).collect();
        let env = std::env::var("PRIVATE_KEY").is_ok_and(|k| !k.is_empty());
        if env {
            list.push(("$PRIVATE_KEY".into(), "the key in your environment or .env".into()));
        }
        list.push(("New account…".into(), "a new key just for deploying, encrypted with a password".into()));
        list.push(("Import a key…".into(), "a private key you have, stored encrypted".into()));
        if accounts.is_empty() && !env {
            say("An account signs the deploy and pays for its gas. A new one just for deploying is safest:");
            say("it only ever holds what deploys cost, and your main wallet's key stays out of the terminal.");
        }
        let at = last.account.as_ref().and_then(|a| list.iter().position(|l| &l.0 == a)).unwrap_or(0);
        let i = ui::select("Deploy from which account?", &list, at)?;
        if i < accounts.len() {
            last.account = Some(list[i].0.clone());
            return Some(Some(list[i].0.clone()));
        }
        if env && i == accounts.len() {
            return Some(None);
        }
        let new = list[i].0.starts_with("New");
        let name = ui::input("Name for it", Some("deployer")).filter(|n| !n.is_empty())?;
        if plain(&strs(&["account", if new { "new" } else { "import" }, &name])) {
            last.account = Some(name);
        }
    }
}

/// Values for `params` (an ABI input list), asked one by one; addresses default to `me`.
fn args(params: &[Value], me: Option<&str>) -> Option<Vec<String>> {
    params
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let ty = x["type"].as_str().unwrap_or_default();
            let name = x["name"].as_str().filter(|n| !n.is_empty()).map_or(format!("argument {}", i + 1), String::from);
            ui::input(&format!("{name} ({ty})"), me.filter(|_| ty == "address"))
        })
        .collect()
}

/// Simulates `cmd` (a deploy or upgrade), then sends it once confirmed; on a local chain, sends right away.
fn send(mut cmd: Vec<String>, net: &str, what: &str) {
    if net != "local" {
        say("First a simulation against the network as it is now; nothing is sent yet.");
        if !plain(&cmd) {
            return;
        }
        if !is_testnet(net) && crate::rpc::NETWORKS.iter().any(|n| n.0 == net) {
            eprintln!("{}", bold(&format!("{net} is a mainnet: this spends real money.")));
        }
        if !ui::confirm(&format!("Send {what} to {net}?")) {
            return;
        }
    }
    cmd.push("--broadcast".into());
    plain(&cmd);
}

fn deploy(p: &Project, last: &mut Last) {
    say("A deploy is simulated first, sent only when you confirm, and then verified on block explorers.");
    if !plain(&strs(&["build"])) {
        return;
    }
    let found = contracts(p);
    let scripts: Vec<String> = p.project_files().into_iter().filter(|f| f.starts_with(&format!("{}/", p.dirs[2])) && f.ends_with(".s.sol")).collect();
    let mut list: Vec<(String, String)> = found.iter().map(|(n, f, _)| (n.clone(), f.clone())).collect();
    list.extend(scripts.iter().map(|f| (f.rsplit('/').next().unwrap_or(f).to_string(), format!("script: runs {f}"))));
    if list.is_empty() {
        say("Nothing to deploy yet: add a contract to src/.");
        return;
    }
    let Some(i) = ui::select("What to deploy?", &list, 0) else { return };
    let Some(net) = network(last) else { return };
    let mut cmd = strs(&["deploy"]);
    let signer = match net.as_str() {
        "local" => None,
        _ => match account(last) {
            Some(a) => a,
            None => return,
        },
    };
    let me = match &signer {
        Some(a) => crate::account::address(a).map(|a| a.to_checksum(None)),
        None if net == "local" => Some("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".into()),
        None => None,
    };
    if i >= found.len() {
        cmd.push(scripts[i - found.len()].clone());
    } else {
        let (name, _, abi) = &found[i];
        cmd.push(name.clone());
        let ctor: Vec<Value> = abi.as_array().into_iter().flatten().find(|x| x["type"] == "constructor").and_then(|c| c["inputs"].as_array().cloned()).unwrap_or_default();
        let mut params = ctor.clone();
        if has(abi, "initialize") && ctor.is_empty() {
            let uups = has(abi, "proxiableUUID");
            let how = if uups { "a UUPS proxy (ERC1967Proxy): the contract upgrades itself" } else { "a transparent proxy whose admin your account owns" };
            let list = items(&[("Yes, upgradeable", how), ("No", "deploy the contract as it is; initialize() isn't called")]);
            say(&format!("{name} has initialize(), so it's written to sit behind a proxy that you can upgrade later."));
            match ui::select("Deploy it behind a proxy?", &list, 0) {
                Some(0) => {
                    cmd.push("--proxy".into());
                    params = abi.as_array().into_iter().flatten().find(|x| x["name"] == "initialize").and_then(|f| f["inputs"].as_array().cloned()).unwrap_or_default();
                    if !params.is_empty() {
                        say("Arguments for initialize():");
                    }
                }
                Some(_) => {}
                None => return,
            }
        } else if !params.is_empty() {
            say(&format!("Arguments for {name}'s constructor:"));
        }
        let Some(values) = args(&params, me.as_deref()) else { return };
        cmd.extend(values);
    }
    cmd.extend(["--network".into(), net.clone()]);
    if let Some(a) = signer {
        cmd.extend(["--account".into(), a]);
    }
    let what = list[i].0.clone();
    send(cmd, &net, &what);
}

fn upgrade(p: &Project, last: &mut Last) {
    say("The new implementation has to keep the proxy's storage; then it's simulated and sent when you confirm.");
    // proxies plain deployed: names in deployments/<network>.json with a <name>_Implementation next to them
    let mut proxies: Vec<(String, String, String)> = vec![];
    for e in std::fs::read_dir(p.root.join("deployments")).into_iter().flatten().flatten() {
        let net = e.path().file_stem().unwrap_or_default().to_string_lossy().to_string();
        let book: std::collections::BTreeMap<String, String> = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for (name, addr) in &book {
            if book.contains_key(&format!("{name}_Implementation")) {
                proxies.push((net.clone(), name.clone(), addr.clone()));
            }
        }
    }
    if proxies.is_empty() {
        say("Nothing to upgrade yet: deploy a contract that has initialize() and pick \"Yes, upgradeable\".");
        return;
    }
    let list: Vec<(String, String)> = proxies.iter().map(|(n, name, a)| (format!("{name} on {n}"), a.clone())).collect();
    let Some(i) = ui::select("Which proxy?", &list, 0) else { return };
    let (net, name, _) = proxies[i].clone();
    if !plain(&strs(&["build"])) {
        return;
    }
    let found = contracts(p);
    let list: Vec<(String, String)> = found.iter().map(|(n, f, _)| (n.clone(), f.clone())).collect();
    let at = found.iter().position(|c| c.0 == name).unwrap_or(0);
    let Some(j) = ui::select("Upgrade it to which contract?", &list, at) else { return };
    if net == "local" && std::net::TcpStream::connect(("127.0.0.1", 8545)).is_err() {
        say("The local chain it's on isn't running anymore, and its state went with it.");
        return;
    }
    let mut cmd = strs(&["upgrade", &name, &found[j].0, "--network", &net]);
    if net != "local" {
        match account(last) {
            Some(Some(a)) => cmd.extend(["--account".into(), a]),
            Some(None) => {}
            None => return,
        }
    }
    send(cmd, &net, &format!("the upgrade of {name}"));
}

fn verify(p: &Project) {
    let chains: Vec<u64> = crate::verify::chains(p).into_iter().filter(|c| !matches!(c, 31337 | 1337)).collect();
    if chains.is_empty() {
        say("Nothing to verify yet. Deploys to live networks verify themselves; come here to retry, or to get the files to verify by hand.");
        return;
    }
    let list: Vec<(String, String)> = chains
        .iter()
        .map(|c| {
            let names: Vec<String> = crate::verify::load(p, *c).into_values().map(|r| r.name).collect();
            (crate::rpc::name_of(*c).map_or(c.to_string(), String::from), names.join(", "))
        })
        .collect();
    let Some(i) = ui::select("Verify what's deployed on", &list, 0) else { return };
    let how = items(&[
        ("On the explorers", "Sourcify, Blockscout, and Etherscan with a key; done ones are skipped"),
        ("Files to verify by hand", "standard JSON input and form values, for any explorer's verify page"),
    ]);
    let Some(h) = ui::select("How?", &how, 0) else { return };
    if h == 0 && crate::verify::etherscan_key(p).is_none() {
        say("No Etherscan API key, so Etherscan, Basescan, Arbiscan and the rest of that family are skipped (Settings adds one).");
    }
    let mut cmd = strs(&["verify", "--network", &list[i].0]);
    if h == 1 {
        cmd.push("--export".into());
    }
    plain(&cmd);
}

fn settings(p: &Project) {
    loop {
        let key = crate::verify::etherscan_key(p).is_some();
        let n = crate::account::all().len();
        let list = vec![
            ("Deployer accounts".to_string(), format!("{n} on this computer")),
            ("Etherscan API key".into(), if key { "set".into() } else { "not set: Etherscan-family explorers are skipped".into() }),
            ("Networks and RPCs".into(), "how plain reaches each chain".into()),
            ("Back".into(), String::new()),
        ];
        match ui::select("Settings", &list, 0) {
            Some(0) => {
                say("Deploys sign with an account's key, which stays encrypted with its password in ~/.plain/keystores.");
                plain(&strs(&["account"]));
                let list = items(&[("New account…", "a new key just for deploying"), ("Import a key…", "a private key you have"), ("Back", "")]);
                if let Some(k @ (0 | 1)) = ui::select("Accounts", &list, 2) {
                    if let Some(name) = ui::input("Name for it", Some("deployer")).filter(|n| !n.is_empty()) {
                        plain(&strs(&["account", if k == 0 { "new" } else { "import" }, &name]));
                    }
                }
            }
            Some(1) => {
                say("One free key from https://etherscan.io/myapikey covers every Etherscan-family explorer: Etherscan, Basescan,");
                say("Arbiscan, Polygonscan and 60 more chains. It's saved in ~/.plain/config.toml, readable only by you.");
                if let Some(k) = ui::secret("Etherscan API key").filter(|k| !k.is_empty()) {
                    crate::project::set_user_config("etherscan_api_key", &k);
                    say("Saved. ETHERSCAN_API_KEY in your environment or .env still takes priority over it.");
                }
            }
            Some(2) => rpcs(p),
            _ => return,
        }
    }
}

fn rpcs(p: &Project) {
    say("plain knows every chain on chainlist.org by name or chain id and finds a working public RPC by itself.");
    say("An RPC of your own (Alchemy, Infura, QuickNode...) wins when there is one: <NAME>_RPC_URL in the environment");
    say("or .env (BASE_SEPOLIA_RPC_URL for base-sepolia), or a [networks] entry in plain.toml.");
    let list = items(&[("Set an RPC URL", "saved in this project's .env, which stays out of git"), ("Back", "")]);
    if ui::select("Networks and RPCs", &list, 1) != Some(0) {
        return;
    }
    let Some(net) = ui::input("Network name", Some("base-sepolia")).filter(|n| !n.is_empty()) else { return };
    let Some(url) = ui::secret("RPC URL").filter(|u| !u.is_empty()) else { return };
    let var = format!("{}_RPC_URL", net.to_uppercase().replace('-', "_"));
    let env = p.root.join(".env");
    let mut lines: Vec<String> = std::fs::read_to_string(&env).unwrap_or_default().lines().filter(|l| !l.trim_start().starts_with(&format!("{var}="))).map(String::from).collect();
    lines.push(format!("{var}={url}"));
    std::fs::write(&env, lines.join("\n") + "\n").unwrap_or_else(|e| crate::die(&format!("{}: {e}", env.display())));
    let gi = p.root.join(".gitignore");
    let ignore = std::fs::read_to_string(&gi).unwrap_or_default();
    if !ignore.lines().any(|l| l.trim() == ".env") {
        std::fs::write(&gi, format!("{ignore}{}.env\n", if ignore.is_empty() || ignore.ends_with('\n') { "" } else { "\n" })).ok();
    }
    say(&format!("Saved {var} in .env."));
    if std::env::var_os(&var).is_some() {
        say("It was already set when this menu started, so the new value applies from the next start.");
    }
}
