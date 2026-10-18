//! Deployer accounts: private keys kept encrypted with a password as Web3 Secret Storage (v3) keystores, the
//! format geth and Foundry's `cast wallet` use. plain writes ~/.plain/keystores and reads Foundry's too.
use crate::{die, ui};
use aes::cipher::{KeyIvInit, StreamCipher};
use alloy_primitives::{Address, B256, keccak256};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Where new keystores go, and Foundry's (`cast wallet import`), which plain reads too.
fn dirs() -> [PathBuf; 2] {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    [crate::solc::home().join("keystores"), home.join(".foundry/keystores")]
}

/// The keystore for `name`: a path to one, or a name in ~/.plain/keystores or ~/.foundry/keystores.
fn file(name: &str) -> Option<PathBuf> {
    let [plain, foundry] = dirs();
    let path = (name.contains('/') || name.ends_with(".json")).then(|| PathBuf::from(name));
    path.into_iter().chain([plain.join(format!("{name}.json")), plain.join(name), foundry.join(name)]).find(|p| p.is_file())
}

fn read(path: &Path) -> Value {
    let b = std::fs::read(path).unwrap_or_else(|e| die(&format!("{}: {e}", path.display())));
    serde_json::from_slice(&b).unwrap_or_else(|_| die(&format!("{} is not a keystore", path.display())))
}

fn recorded(ks: &Value) -> Option<Address> {
    format!("0x{}", ks["address"].as_str()?.trim_start_matches("0x")).parse().ok()
}

/// The account's address, without its password when the keystore records it.
pub fn address(name: &str) -> Option<Address> {
    recorded(&read(&file(name)?))
}

/// Accounts on this machine: name, address (if recorded), file.
pub fn all() -> Vec<(String, Option<Address>, PathBuf)> {
    let mut out = vec![];
    for d in dirs() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
            let ks: Option<Value> = std::fs::read(&f).ok().and_then(|b| serde_json::from_slice(&b).ok());
            if let Some(ks) = ks.filter(|k| k["crypto"].is_object() || k["Crypto"].is_object()) {
                if !out.iter().any(|(n, _, _)| *n == name) {
                    out.push((name, recorded(&ks), f));
                }
            }
        }
    }
    out
}

/// The account's private key; asks for its password.
pub fn unlock(name: &str) -> B256 {
    let path = file(name).unwrap_or_else(|| die(&format!("no account {name:?} (`plain account` lists them; `plain account new {name}` makes one)")));
    let ks = read(&path);
    for _ in 0..3 {
        let pw = ui::secret(&format!("password for {name}")).unwrap_or_else(|| die("cancelled"));
        match decrypt(&ks, &pw) {
            Ok(k) => return k,
            Err(e) => eprintln!("{e}"),
        }
    }
    die("wrong password")
}

fn decrypt(ks: &Value, pw: &str) -> Result<B256, String> {
    let c = if ks["crypto"].is_object() { &ks["crypto"] } else { &ks["Crypto"] };
    let h = |v: &Value| hex::decode(v.as_str().unwrap_or_default()).map_err(|_| "malformed keystore".to_string());
    let kp = &c["kdfparams"];
    let n = kp["n"].as_u64().unwrap_or(0);
    if c["kdf"] != "scrypt" || c["cipher"] != "aes-128-ctr" || !n.is_power_of_two() {
        return Err(format!("unsupported keystore ({} with {})", c["kdf"], c["cipher"]));
    }
    let len = kp["dklen"].as_u64().unwrap_or(32) as usize;
    let params = scrypt::Params::new(n.trailing_zeros() as u8, kp["r"].as_u64().unwrap_or(8) as u32, kp["p"].as_u64().unwrap_or(1) as u32, len).map_err(|e| e.to_string())?;
    let mut dk = vec![0u8; len.max(32)];
    scrypt::scrypt(pw.as_bytes(), &h(&kp["salt"])?, &params, &mut dk[..len]).map_err(|e| e.to_string())?;
    let mut key = h(&c["ciphertext"])?;
    if keccak256([&dk[16..32], &key].concat()).as_slice() != h(&c["mac"])? {
        return Err("wrong password".into());
    }
    ctr::Ctr128BE::<aes::Aes128>::new_from_slices(&dk[..16], &h(&c["cipherparams"]["iv"])?).map_err(|e| e.to_string())?.apply_keystream(&mut key);
    B256::try_from(key.as_slice()).map_err(|_| "malformed keystore".into())
}

fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).unwrap_or_else(|e| die(&format!("no randomness from the OS: {e}")));
    b
}

fn encrypt(key: B256, pw: &str) -> Value {
    let (salt, iv, mut id) = (random(32), random(16), random(16));
    // scrypt with N = 2^15: 32 MB and about 0.1 s to unlock, where geth's standard 2^18 takes 256 MB and a second
    let log_n = 15;
    let mut dk = [0u8; 32];
    scrypt::scrypt(pw.as_bytes(), &salt, &scrypt::Params::new(log_n, 8, 1, 32).unwrap(), &mut dk).unwrap();
    let mut ct = key.to_vec();
    ctr::Ctr128BE::<aes::Aes128>::new_from_slices(&dk[..16], &iv).unwrap().apply_keystream(&mut ct);
    let mac = keccak256([&dk[16..], &ct[..]].concat());
    // a version 4 UUID
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    let id = hex::encode(id);
    let addr = crate::evm::pk_to_addr(key.into()).unwrap_or_else(|e| die(&e));
    json!({
        "version": 3,
        "id": format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..]),
        "address": hex::encode(addr),
        "crypto": {
            "cipher": "aes-128-ctr", "cipherparams": {"iv": hex::encode(iv)}, "ciphertext": hex::encode(ct),
            "kdf": "scrypt", "kdfparams": {"dklen": 32, "n": 1u64 << log_n, "r": 8, "p": 1, "salt": hex::encode(salt)},
            "mac": hex::encode(mac),
        },
    })
}

/// `plain account [new|import <name>]`
pub fn cli(args: &[String]) {
    match (args.first().map(String::as_str), args.get(1)) {
        (None | Some("list"), _) => list(),
        (Some(how @ ("new" | "import")), Some(name)) => create(name, how == "import"),
        _ => die("usage: plain account [list | new <name> | import <name>]"),
    }
}

fn list() {
    let all = all();
    if all.is_empty() {
        println!("no accounts yet; `plain account new deployer` makes one, `plain account import deployer` stores a key you have");
        return;
    }
    let w = all.iter().map(|a| a.0.len()).max().unwrap_or(0);
    let home = std::env::var("HOME").unwrap_or_default();
    for (name, addr, path) in all {
        let addr = addr.map(|a| a.to_checksum(None)).unwrap_or_else(|| "(address shown once unlocked)".into());
        println!("{name:<w$}  {addr}  {}", path.display().to_string().replacen(&home, "~", 1));
    }
}

fn create(name: &str, import: bool) {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        die("an account name takes letters, digits, '-', '_' and '.'");
    }
    if let Some(f) = file(name) {
        die(&format!("account {name} already exists ({})", f.display()));
    }
    let key = match import {
        true => {
            let s = ui::secret("private key").unwrap_or_else(|| die("cancelled"));
            s.trim().parse::<B256>().ok().filter(|k| crate::evm::pk_to_addr((*k).into()).is_ok()).unwrap_or_else(|| die("a private key is 64 hex characters"))
        }
        false => loop {
            let k = B256::from_slice(&random(32));
            if crate::evm::pk_to_addr(k.into()).is_ok() {
                break k;
            }
        },
    };
    let pw = loop {
        let a = ui::secret(&format!("new password for {name}")).unwrap_or_else(|| die("cancelled"));
        if a.is_empty() {
            eprintln!("the password can't be empty");
            continue;
        }
        if ui::secret("same password again").unwrap_or_else(|| die("cancelled")) == a {
            break a;
        }
        eprintln!("those didn't match; once more");
    };
    let ks = encrypt(key, &pw);
    let dir = &dirs()[0];
    let path = dir.join(format!("{name}.json"));
    let mut dirb = std::fs::DirBuilder::new();
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        dirb.mode(0o700);
        open.mode(0o600);
    }
    dirb.recursive(true).create(dir).ok();
    use std::io::Write;
    open.open(&path).and_then(|mut f| f.write_all(serde_json::to_string_pretty(&ks).unwrap().as_bytes())).unwrap_or_else(|e| die(&format!("{}: {e}", path.display())));
    println!("{} {name}: {}", if import { "imported" } else { "created" }, recorded(&ks).unwrap().to_checksum(None));
    println!("the key is in {}, encrypted with your password; deploy with --account {name}", path.display());
    if !import {
        println!("send it some ETH before deploying (on a testnet, from a faucet), and back up that file and the password: together they're the only way back to this key");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let key: alloy_primitives::B256 = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap();
        let ks = super::encrypt(key, "hunter2");
        assert_eq!(ks["address"], "f39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        assert_eq!(super::decrypt(&ks, "hunter2").unwrap(), key);
        assert!(super::decrypt(&ks, "hunter3").is_err());
    }
}
