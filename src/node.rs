//! In-process dev chain over JSON-RPC (automine, anvil/hardhat test methods). Used by TS tests and `plain node`.
use crate::decode;
use crate::evm::{self, Cheats, Commit, Db, Env};
use alloy_consensus::Transaction as _;
use alloy_consensus::TxEnvelope;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, Log, TxKind, U256, keccak256};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub const KEYS: [&str; 10] = [
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    "92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
    "4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    "dbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    "2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
];

struct Block {
    hash: B256,
    timestamp: u64,
    txs: Vec<B256>,
}

#[derive(Clone)]
struct TxRec {
    hash: B256,
    from: Address,
    to: Option<Address>,
    nonce: u64,
    value: U256,
    input: Bytes,
    gas: u64,
    gas_used: u64,
    logs: Vec<Log>,
    contract: Option<Address>,
    block: u64,
}

#[derive(Clone)]
struct Snapshot {
    db: Db,
    env: Env,
    blocks: usize,
    offset: i64,
}

pub struct Chain {
    db: Db,
    env: Env,
    blocks: Vec<Block>,
    txs: HashMap<B256, TxRec>,
    snapshots: Vec<Snapshot>,
    accounts: Vec<Address>,
    offset: i64,
    next_ts: Option<u64>,
    quiet: bool,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn hx(n: impl std::fmt::LowerHex) -> String {
    format!("0x{n:x}")
}

fn rpc_err(code: i64, msg: impl Into<String>, data: Option<String>) -> Value {
    let mut e = json!({"code": code, "message": msg.into()});
    if let Some(d) = data {
        e["data"] = json!(d);
    }
    e
}

fn revert_err(out: &[u8]) -> Value {
    rpc_err(3, format!("execution reverted: {}", decode::reason(out)), Some(format!("0x{}", hex::encode(out))))
}

impl Chain {
    pub fn new(fork: Option<String>) -> Chain {
        let remote = evm::Remote(fork.as_ref().map(|u| Arc::new(evm::Fork::new(u, None))));
        let mut db = evm::base_db(remote);
        let accounts: Vec<Address> = KEYS.iter().map(|k| evm::pk_to_addr(U256::from_str_radix(k, 16).unwrap()).unwrap()).collect();
        for a in &accounts {
            db.insert_account_info(*a, revm::state::AccountInfo::from_balance(U256::from(10_000u64) * U256::from(10u64).pow(U256::from(18))));
        }
        let mut env = Env::new(31337);
        env.block.number = U256::ZERO;
        env.block.timestamp = U256::from(now());
        let genesis = Block { hash: keccak256(b"plain genesis"), timestamp: now(), txs: vec![] };
        Chain { db, env, blocks: vec![genesis], txs: HashMap::new(), snapshots: vec![], accounts, offset: 0, next_ts: None, quiet: false }
    }

    fn next_block_env(&mut self) {
        let last = self.blocks.last().unwrap().timestamp;
        let ts = self.next_ts.take().unwrap_or_else(|| ((now() as i64 + self.offset) as u64).max(last + 1));
        self.env.block.number = U256::from(self.blocks.len());
        self.env.block.timestamp = U256::from(ts);
    }

    fn mine(&mut self, txs: Vec<B256>) {
        self.next_block_env();
        let n = self.blocks.len() as u64;
        let ts: u64 = self.env.block.timestamp.to();
        let hash = keccak256([self.blocks.last().unwrap().hash.as_slice(), &n.to_be_bytes()].concat());
        for t in &txs {
            if let Some(r) = self.txs.get_mut(t) {
                r.block = n;
            }
        }
        self.blocks.push(Block { hash, timestamp: ts, txs });
    }

    fn exec(&mut self, from: Address, to: Option<Address>, data: Bytes, value: U256, commit: Commit) -> evm::Exec {
        let mut cheats = Cheats::default();
        let mut env = self.env.clone();
        // execute in the context of the next block
        env.block.number = U256::from(self.blocks.len());
        let db = std::mem::replace(&mut self.db, Db::new(Default::default()));
        let (db, r) = evm::transact(db, &mut env, &mut cheats, from, to.map(TxKind::Call).unwrap_or(TxKind::Create), data, value, commit);
        self.db = db;
        if !self.quiet {
            for l in cheats.console {
                eprintln!("console.log: {l}");
            }
        }
        r
    }

    fn send(&mut self, from: Address, to: Option<Address>, data: Bytes, value: U256, gas: u64, hash: Option<B256>) -> Result<Value, Value> {
        use revm::database_interface::DatabaseRef;
        let nonce = self.db.basic_ref(from).ok().flatten().map(|a| a.nonce).unwrap_or(0);
        self.next_block_env();
        let r = self.exec(from, to, data.clone(), value, Commit::OnSuccess);
        if !r.ok {
            return Err(revert_err(&r.output));
        }
        let hash = hash.unwrap_or_else(|| keccak256([from.as_slice(), &nonce.to_be_bytes(), &data].concat()));
        let contract = if to.is_none() { r.address } else { None };
        self.txs.insert(hash, TxRec { hash, from, to, nonce, value, input: data, gas, gas_used: r.gas, logs: r.logs, contract, block: 0 });
        self.mine(vec![hash]);
        Ok(json!(hash))
    }

    fn block_json(&self, n: usize, full: bool) -> Value {
        let Some(b) = self.blocks.get(n) else { return Value::Null };
        let parent = if n == 0 { B256::ZERO } else { self.blocks[n - 1].hash };
        let txs: Vec<Value> = b.txs.iter().map(|h| if full { self.tx_json(&self.txs[h]) } else { json!(h) }).collect();
        let used: u64 = b.txs.iter().map(|h| self.txs[h].gas_used).sum();
        json!({
            "number": hx(n), "hash": b.hash, "parentHash": parent, "timestamp": hx(b.timestamp),
            "gasLimit": hx(evm::GAS_LIMIT), "gasUsed": hx(used), "baseFeePerGas": "0x0", "miner": Address::ZERO,
            "difficulty": "0x0", "totalDifficulty": "0x0", "extraData": "0x", "nonce": "0x0000000000000000",
            "mixHash": B256::ZERO, "sha3Uncles": B256::ZERO, "stateRoot": B256::ZERO, "receiptsRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO, "logsBloom": format!("0x{}", "0".repeat(512)), "size": "0x0",
            "uncles": [], "transactions": txs,
        })
    }

    fn tx_json(&self, t: &TxRec) -> Value {
        json!({
            "hash": t.hash, "nonce": hx(t.nonce), "blockHash": self.blocks[t.block as usize].hash, "blockNumber": hx(t.block),
            "transactionIndex": "0x0", "from": t.from, "to": t.to, "value": hx(t.value), "gas": hx(t.gas), "gasPrice": "0x0",
            "maxFeePerGas": "0x0", "maxPriorityFeePerGas": "0x0", "input": t.input, "type": "0x2", "chainId": hx(self.env.chain_id),
            "v": "0x0", "r": "0x0", "s": "0x0", "yParity": "0x0", "accessList": [],
        })
    }

    fn log_json(&self, t: &TxRec, i: usize, l: &Log) -> Value {
        json!({
            "address": l.address, "topics": l.topics(), "data": l.data.data, "blockNumber": hx(t.block),
            "blockHash": self.blocks[t.block as usize].hash, "transactionHash": t.hash, "transactionIndex": "0x0",
            "logIndex": hx(i), "removed": false,
        })
    }

    fn receipt_json(&self, t: &TxRec) -> Value {
        json!({
            "transactionHash": t.hash, "transactionIndex": "0x0", "blockHash": self.blocks[t.block as usize].hash,
            "blockNumber": hx(t.block), "from": t.from, "to": t.to, "cumulativeGasUsed": hx(t.gas_used), "gasUsed": hx(t.gas_used),
            "contractAddress": t.contract, "logs": t.logs.iter().enumerate().map(|(i, l)| self.log_json(t, i, l)).collect::<Vec<_>>(),
            "logsBloom": format!("0x{}", "0".repeat(512)), "status": "0x1", "effectiveGasPrice": "0x0", "type": "0x2",
        })
    }

    fn block_num(&self, v: Option<&Value>) -> usize {
        let last = self.blocks.len() - 1;
        match v.and_then(|v| v.as_str()) {
            None | Some("latest" | "pending" | "safe" | "finalized") => last,
            Some("earliest") => 0,
            Some(h) => usize::from_str_radix(h.trim_start_matches("0x"), 16).unwrap_or(last).min(last),
        }
    }

    fn handle(&mut self, method: &str, p: &[Value]) -> Result<Value, Value> {
        use revm::database_interface::DatabaseRef;
        let addr = |i: usize| p.get(i).and_then(|v| v.as_str()).and_then(|s| s.parse::<Address>().ok()).ok_or_else(|| rpc_err(-32602, "bad address", None));
        let num = |v: &Value| -> U256 { v.as_str().and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok()).or_else(|| v.as_u64().map(U256::from)).unwrap_or_default() };
        let bytes = |v: &Value| -> Bytes { v.as_str().and_then(|s| hex::decode(s.trim_start_matches("0x")).ok()).unwrap_or_default().into() };
        let call_args = |c: &Value| {
            let from = c["from"].as_str().and_then(|s| s.parse().ok()).unwrap_or(Address::ZERO);
            let to = c["to"].as_str().and_then(|s| s.parse().ok());
            let data = if c["input"].is_string() { bytes(&c["input"]) } else { bytes(&c["data"]) };
            (from, to, data, num(&c["value"]), num(&c["gas"]).saturating_to::<u64>())
        };
        Ok(match method {
            "web3_clientVersion" => json!("plain/0.1"),
            "net_version" => json!(self.env.chain_id.to_string()),
            "eth_chainId" => json!(hx(self.env.chain_id)),
            "eth_blockNumber" => json!(hx(self.blocks.len() - 1)),
            "eth_accounts" | "eth_requestAccounts" => json!(self.accounts),
            "eth_gasPrice" | "eth_maxPriorityFeePerGas" | "eth_blobBaseFee" => json!("0x0"),
            "eth_feeHistory" => json!({"oldestBlock": hx(self.blocks.len() - 1), "baseFeePerGas": ["0x0", "0x0"], "gasUsedRatio": [0.0], "reward": [["0x0"]]}),
            "eth_syncing" | "eth_mining" => json!(false),
            "eth_getBalance" => json!(hx(self.db.basic_ref(addr(0)?).ok().flatten().map(|a| a.balance).unwrap_or_default())),
            "eth_getTransactionCount" => json!(hx(self.db.basic_ref(addr(0)?).ok().flatten().map(|a| a.nonce).unwrap_or(0))),
            "eth_getCode" => {
                let a = self.db.basic_ref(addr(0)?).ok().flatten();
                let code = a.and_then(|a| a.code.map(|c| c.original_bytes())).unwrap_or_default();
                json!(code)
            }
            "eth_getStorageAt" => {
                let v = self.db.storage_ref(addr(0)?, num(&p[1])).unwrap_or_default();
                json!(B256::from(v))
            }
            "eth_call" | "eth_estimateGas" => {
                let (from, to, data, value, _) = call_args(&p[0]);
                let r = self.exec(from, to, data, value, Commit::Never);
                if !r.ok {
                    return Err(revert_err(&r.output));
                }
                if method == "eth_call" { json!(r.output) } else { json!(hx((r.gas * 13 / 10).max(21_000))) }
            }
            "eth_sendTransaction" => {
                let (from, to, data, value, gas) = call_args(&p[0]);
                return self.send(from, to, data, value, gas, None);
            }
            "eth_sendRawTransaction" => {
                let raw = bytes(&p[0]);
                let tx = TxEnvelope::decode_2718(&mut raw.as_ref()).map_err(|e| rpc_err(-32602, format!("bad tx: {e}"), None))?;
                let from = tx.recover_signer().map_err(|e| rpc_err(-32602, format!("bad signature: {e}"), None))?;
                let nonce = self.db.basic_ref(from).ok().flatten().map(|a| a.nonce).unwrap_or(0);
                if tx.nonce() != nonce {
                    return Err(rpc_err(-32000, format!("nonce mismatch: tx {} expected {nonce}", tx.nonce()), None));
                }
                let to = tx.to();
                return self.send(from, to, tx.input().clone(), tx.value(), tx.gas_limit(), Some(*tx.tx_hash()));
            }
            "eth_getTransactionByHash" => p.first().and_then(|h| h.as_str()?.parse::<B256>().ok()).and_then(|h| self.txs.get(&h)).map(|t| self.tx_json(t)).unwrap_or(Value::Null),
            "eth_getTransactionReceipt" => p.first().and_then(|h| h.as_str()?.parse::<B256>().ok()).and_then(|h| self.txs.get(&h)).map(|t| self.receipt_json(t)).unwrap_or(Value::Null),
            "eth_getBlockByNumber" => self.block_json(self.block_num(p.first()), p.get(1).and_then(|v| v.as_bool()).unwrap_or(false)),
            "eth_getBlockByHash" => {
                let h: B256 = p.first().and_then(|h| h.as_str()?.parse().ok()).unwrap_or_default();
                match self.blocks.iter().position(|b| b.hash == h) {
                    Some(n) => self.block_json(n, p.get(1).and_then(|v| v.as_bool()).unwrap_or(false)),
                    None => Value::Null,
                }
            }
            "eth_getLogs" => {
                let f = p.first().cloned().unwrap_or(json!({}));
                let (from, to) = match f["blockHash"].as_str().and_then(|h| h.parse::<B256>().ok()) {
                    Some(h) => {
                        let n = self.blocks.iter().position(|b| b.hash == h).unwrap_or(usize::MAX);
                        (n, n)
                    }
                    None => (self.block_num(f.get("fromBlock").filter(|v| !v.is_null()).or(Some(&json!("earliest")))), self.block_num(f.get("toBlock"))),
                };
                let addrs: Vec<Address> = match &f["address"] {
                    Value::String(s) => s.parse().into_iter().collect(),
                    Value::Array(a) => a.iter().filter_map(|x| x.as_str()?.parse().ok()).collect(),
                    _ => vec![],
                };
                let topics: Vec<Vec<B256>> = f["topics"]
                    .as_array()
                    .map(|t| {
                        t.iter()
                            .map(|x| match x {
                                Value::String(s) => s.parse().into_iter().collect(),
                                Value::Array(a) => a.iter().filter_map(|y| y.as_str()?.parse().ok()).collect(),
                                _ => vec![],
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut out = vec![];
                for n in from..=to.min(self.blocks.len().saturating_sub(1)) {
                    for h in &self.blocks[n].txs {
                        let t = &self.txs[h];
                        for (i, l) in t.logs.iter().enumerate() {
                            let ok_addr = addrs.is_empty() || addrs.contains(&l.address);
                            let ok_topics = topics.iter().enumerate().all(|(j, want)| want.is_empty() || l.topics().get(j).is_some_and(|t| want.contains(t)));
                            if ok_addr && ok_topics {
                                out.push(self.log_json(t, i, l));
                            }
                        }
                    }
                }
                json!(out)
            }
            "evm_snapshot" | "anvil_snapshot" => {
                self.snapshots.push(Snapshot { db: self.db.clone(), env: self.env.clone(), blocks: self.blocks.len(), offset: self.offset });
                json!(hx(self.snapshots.len() - 1))
            }
            "evm_revert" | "anvil_revert" => {
                let id = num(&p[0]).saturating_to::<usize>();
                if id >= self.snapshots.len() {
                    return Ok(json!(false));
                }
                let s = self.snapshots[id].clone();
                self.snapshots.truncate(id); // snapshots are single-use, like hardhat
                self.db = s.db;
                self.env = s.env;
                self.offset = s.offset;
                for b in self.blocks.drain(s.blocks..) {
                    for h in b.txs {
                        self.txs.remove(&h);
                    }
                }
                json!(true)
            }
            "evm_mine" | "anvil_mine" | "hardhat_mine" => {
                let n = if method == "evm_mine" { 1 } else { p.first().map(|v| num(v).saturating_to::<usize>()).unwrap_or(1).max(1) };
                if method == "evm_mine" {
                    if let Some(ts) = p.first().map(num) {
                        self.next_ts = Some(ts.saturating_to());
                    }
                }
                for _ in 0..n {
                    self.mine(vec![]);
                }
                json!("0x0")
            }
            "evm_increaseTime" | "anvil_increaseTime" => {
                let secs = num(&p[0]).saturating_to::<i64>();
                self.offset += secs;
                // relative to the last block, which may already run ahead of the wall clock
                let last = self.blocks.last().unwrap().timestamp as i64;
                self.next_ts = Some((last + secs).max(now() as i64 + self.offset) as u64);
                json!(hx(self.offset))
            }
            "evm_setNextBlockTimestamp" | "anvil_setNextBlockTimestamp" => {
                self.next_ts = Some(num(&p[0]).saturating_to());
                json!(null)
            }
            "anvil_setBalance" | "hardhat_setBalance" | "anvil_setNonce" | "hardhat_setNonce" | "anvil_setCode" | "hardhat_setCode" => {
                let a = addr(0)?;
                let mut info = self.db.basic_ref(a).ok().flatten().unwrap_or_default();
                match method.split('_').nth(1).unwrap() {
                    "setBalance" => info.balance = num(&p[1]),
                    "setNonce" => info.nonce = num(&p[1]).saturating_to(),
                    _ => {
                        let code = revm::state::Bytecode::new_raw(bytes(&p[1]));
                        info.code_hash = code.hash_slow();
                        info.code = Some(code);
                    }
                }
                self.db.insert_account_info(a, info);
                json!(true)
            }
            "anvil_setStorageAt" | "hardhat_setStorageAt" => {
                let a = addr(0)?;
                if self.db.basic_ref(a).ok().flatten().is_none() {
                    self.db.insert_account_info(a, Default::default());
                }
                self.db.insert_account_storage(a, num(&p[1]), num(&p[2])).ok();
                json!(true)
            }
            // every account is unlocked; impersonation is a no-op
            "anvil_impersonateAccount" | "hardhat_impersonateAccount" | "anvil_stopImpersonatingAccount" | "hardhat_stopImpersonatingAccount" | "anvil_setAutomine" | "evm_setAutomine" => json!(true),
            _ => return Err(rpc_err(-32601, format!("method {method} not supported by plain"), None)),
        })
    }

    pub fn dispatch(&mut self, req: &Value) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or_default();
        let params: Vec<Value> = req["params"].as_array().cloned().unwrap_or_default();
        match self.handle(method, &params) {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        }
    }
}

/// Serve the chain on `listener` until the process exits (one request per connection).
pub fn serve(listener: TcpListener, chain: Arc<Mutex<Chain>>) {
    for stream in listener.incoming().flatten() {
        let chain = chain.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(&stream);
            loop {
                let mut len = 0usize;
                let mut line = String::new();
                let mut got_request;
                loop {
                    line.clear();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    got_request = true;
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                if !got_request {
                    return;
                }
                let mut body = vec![0; len];
                if r.read_exact(&mut body).is_err() {
                    return;
                }
                let resp = match serde_json::from_slice::<Value>(&body) {
                    Ok(Value::Array(reqs)) => {
                        let mut c = chain.lock().unwrap();
                        Value::Array(reqs.iter().map(|q| c.dispatch(q)).collect())
                    }
                    Ok(q) => chain.lock().unwrap().dispatch(&q),
                    Err(_) => json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}),
                };
                let out = resp.to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\n\r\n",
                    out.len()
                );
                let mut w = &stream;
                if w.write_all(head.as_bytes()).and_then(|_| w.write_all(out.as_bytes())).is_err() {
                    return;
                }
            }
        });
    }
}

pub fn start(fork: Option<String>, port: u16, quiet: bool) -> (u16, Arc<Mutex<Chain>>) {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| crate::die(&format!("bind 127.0.0.1:{port}: {e}")));
    let port = listener.local_addr().unwrap().port();
    let mut c = Chain::new(fork);
    c.quiet = quiet;
    let chain = Arc::new(Mutex::new(c));
    let ch = chain.clone();
    std::thread::spawn(move || serve(listener, ch));
    (port, chain)
}
