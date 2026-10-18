---
layout: docs
title: "plain docs: install, test, deploy"
description: "How to install plain, start a new Solidity project or use an existing Foundry one, and build, test, deploy and verify contracts with it, with or without Foundry installed."
---

# plain docs
{: .no_toc}

<p class="lead">plain is one binary that compiles, tests, deploys and verifies Solidity contracts. This page takes you from installing it to a verified deployment, whether or not Foundry is on your machine.</p>

* TOC
{:toc}

## Install

```sh
curl -fsSL https://getplain.org/install | sh
```

The installer puts the latest release in `~/.plain/bin` and adds that folder to your shell's `PATH`, so `plain` works in any new terminal. Set `PLAIN_DIR` to install it somewhere else. Prebuilt binaries cover macOS on Apple silicon and Intel, and Linux on x86_64 with glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and later).

On any other platform, or to build from source with Rust:

```sh
cargo install --git https://github.com/backmeupplz/plain
```

To update, run the installer again. To uninstall, delete `~/.plain` (the binary and its caches) and the `# plain` lines the installer added to your shell profile.

## Start a new project

```sh
mkdir my-project && cd my-project
plain init
plain test
```

`plain init` writes three files: a `Counter` contract in `src/Counter.sol`, its test in `test/Counter.t.sol`, and a `.gitignore` that leaves out `.plain/` and `node_modules/`. The first build downloads the newest solc that satisfies your pragmas, and after that everything is cached:

```
downloading solc 0.8.37
compiled 2 files with solc 0.8.37 in 10.08s (2 contracts)
test/Counter.t.sol:CounterTest: 1 passed (799µs)
1 passed, 0 failed, 0 skipped (1 suites, 1.18ms)
```

plain looks for contracts in `src/` (or `contracts/` when there's no `src/`), tests in `test/` and deploy scripts in `script/`. No config file is needed. forge-std ships inside the binary, so `import {Test} from "forge-std/Test.sol";` works without installing anything.

## Use an existing project

### Foundry projects

Run plain in the project root as it is. It reads `foundry.toml`'s `[profile.default]` (solc version, optimizer, via-IR, remappings, fuzz and invariant settings, `rpc_endpoints` and `[etherscan]` keys), `remappings.txt` and `lib/`, so nothing needs to change. Add `.plain/` to your `.gitignore`.

plain and forge can share a project. plain writes only to `.plain/`, plus `deployments/` when you deploy, and forge keeps using `out/` and `cache/`, so you can keep both and switch whenever you like.

### With or without Foundry installed

plain doesn't need Foundry, Node or anything else on your machine. If Foundry is installed, plain reuses the solc binaries it already downloaded. If it isn't, plain downloads solc into `~/.plain` the first time a project needs it. Either way the result is the same.

forge-std is built in, so a project works even when `lib/forge-std` isn't checked out. Other libraries in `lib/` still need `git submodule update --init --recursive` after a fresh clone, as they would for forge.

### Hardhat projects

Contracts in `contracts/` and imports from `node_modules` compile as they are. plain runs Solidity tests and plain TypeScript tests (see below), so tests written for Hardhat's own runner keep running under Hardhat.

## Dependencies

plain resolves imports the way forge does, and it also looks in `node_modules`:

- **From npm:** after `npm install @openzeppelin/contracts`, an import like `import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";` works without any remapping.
- **From git:** put the library in `lib/`, for example with `git submodule add https://github.com/OpenZeppelin/openzeppelin-contracts lib/openzeppelin-contracts`. Every `lib/<name>` is importable as `<name>/`, pointing at its `src/` or `contracts/` folder, and the library's own `remappings.txt` applies too, which is what makes OpenZeppelin's `@openzeppelin/contracts/...` paths work.
- **Anything else:** add remappings to `remappings.txt`, or as `remappings = ["@x/=lib/x/src/"]` in `plain.toml` or `foundry.toml`.

## Build

```sh
plain build           # compile src/, test/ and script/
plain build --force   # ignore every cache and recompile
```

Artifacts land in `.plain/out/<Contract>.json` with each contract's `abi` and `bytecode`. Builds are cached by content: a build with nothing changed takes about 20 ms. A new clone or worktree of sources that any checkout already compiled compiles nothing, because every checkout shares `~/.plain/cache`. plain picks the newest solc that satisfies every pragma in the project, and `solc = "0.8.28"` in the config pins a version.

## Test

```sh
plain test                     # compile what changed, then run every test
plain test Vault               # only test files, suites and tests matching "Vault"
plain test -v                  # list every test, with gas and fuzz runs
plain test --fork base         # run against a fork of a live network
plain test --fuzz-runs 1000 --seed 42
plain test --rerun             # run every suite again, even unchanged ones
plain test --force             # recompile everything, then run everything
```

A filter matches suite paths like `test/Vault.t.sol:VaultTest` and test names, and only the test files it can match get compiled. The exit code is non-zero when anything fails, so plain works in CI and in scripts as it is.

### Writing tests

Tests are Foundry-style Solidity using forge-std:

```solidity
import {Test, console} from "forge-std/Test.sol";
import {Vault} from "../src/Vault.sol";

contract VaultTest is Test {
    Vault vault;
    address alice = makeAddr("alice");

    function setUp() public { vault = new Vault(); deal(alice, 10 ether); }

    function test_Deposit(uint96 amount) public {          // parameters make it a fuzz test
        vm.assume(amount <= 10 ether);
        vm.prank(alice);
        vault.deposit{value: amount}();
        assertEq(vault.balanceOf(alice), amount);
    }

    function test_RevertWhen_Overdraw() public {
        vm.expectRevert(abi.encodeWithSelector(Vault.Insufficient.selector, 0, 1));
        vm.prank(alice);
        vault.withdraw(1);
    }

    function invariant_Solvent() public view {             // random calls to the contracts made in setUp
        assertGe(address(vault).balance, vault.totalDeposits());
    }
}
```

A failure prints the decoded reason (custom errors, panics and `require` strings), the fuzz counterexample, `console.log` output, and a short call trace that follows the path to the revert:

```
FAIL test_Deposit(uint96): OutOfFunds
     args: 79228162514264337593543950335
     trace:
       VaultTest.test_Deposit(79228162514264337593543950335)  ✗ reverted without reason
         vm.prank(alice)
         Vault.deposit() value: 79228162514264337593543950335  ✗ OutOfFunds
```

Supported cheatcodes are `prank`/`startPrank`, `deal`, `warp`, `roll`, `fee`, `chainId`, `prevrandao`, `coinbase`, `store`/`load`, `etch`, nonces, `addr`/`sign`/`rememberKey`, `label`, `assume`, `skip`, `expectRevert` in all its forms, `expectEmit`, `expectCall`, `mockCall`, `recordLogs`/`getRecordedLogs`, `snapshotState`/`revertToState`, `env*`, `setEnv`, `toString`, `parse*`, `getCode`, `readFile`/`writeFile`, `ffi`, `computeCreate*Address`, the broadcast cheatcodes, gas snapshots and every `assert*`. Anything else fails with `unsupported cheatcode vm.<name>(...)`. A comment like `/// forge-config: default.fuzz.runs = 10` above a test sets its fuzz runs.

### Results are reused

A suite runs again only when its compiled code, the test settings, the plain binary, or something it read changed. That last part covers contracts it deployed through `getCode`, files and environment variables. Suites whose results were reused print `(cached)`. Suites that call `ffi`, `writeFile`, `unixTime` or `setEnv` always run, and so does every `--fork` run.

### TypeScript tests

Any `test/**/*.test.ts` runs with `node --test`, or with `bun test` when it imports `bun:test`. Each file gets its own fresh chain, and files run in parallel. Two environment variables point at it: `PLAIN_RPC_URL`, an anvil-compatible JSON-RPC endpoint on chain 31337 with the ten default anvil accounts funded and unlocked, and `PLAIN_ARTIFACTS`, the folder holding `<Contract>.json` files with `abi` and `bytecode`. With viem:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createTestClient, http, publicActions, walletActions } from "viem";
import { foundry } from "viem/chains";

const Counter = JSON.parse(readFileSync(`${process.env.PLAIN_ARTIFACTS}/Counter.json`, "utf8"));
const client = createTestClient({ chain: foundry, mode: "anvil", transport: http(process.env.PLAIN_RPC_URL) })
  .extend(publicActions).extend(walletActions);
const [me] = await client.getAddresses();

test("increments", async () => {
  const hash = await client.deployContract({ abi: Counter.abi, bytecode: Counter.bytecode, account: me });
  const { contractAddress } = await client.waitForTransactionReceipt({ hash });
  await client.writeContract({ address: contractAddress!, abi: Counter.abi, functionName: "increment", account: me });
  assert.equal(await client.readContract({ address: contractAddress!, abi: Counter.abi, functionName: "number" }), 1n);
});
```

The chain supports `evm_snapshot`/`evm_revert`, `evm_increaseTime`, `evm_mine`/`anvil_mine`, `anvil_setBalance`/`setCode`/`setStorageAt`/`setNonce`, impersonation (every account is unlocked) and `eth_getLogs`, and it reverts with decoded custom errors.

## Deploy

### To a local chain

`plain node` starts a local chain on `127.0.0.1:8545` that speaks the same JSON-RPC as anvil and Hardhat, and prints the funded dev accounts:

```
plain node on http://127.0.0.1:8545 (chain 31337); accounts are anvil/hardhat defaults, e.g.
  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

In another terminal, deploy with that key:

```sh
export PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
plain deploy Counter --network local --broadcast
```

### To a live network

```sh
export PRIVATE_KEY=0x...        # the deployer's key
export ETHERSCAN_API_KEY=...    # optional: Etherscan verification on 60+ chains
plain deploy Token 1000000000000000000000000 --network base-sepolia               # simulate only
plain deploy Token 1000000000000000000000000 --network base-sepolia --broadcast   # send and verify
```

Constructor arguments follow the contract name. Without `--broadcast`, plain only simulates against a fork of the network and prints each transaction with its gas, so you can check a deploy before it costs anything:

```
1. create Token (from 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266, gas 541089)
simulated 1 transaction(s) against base-sepolia; add --broadcast to send
```

### With a script

Deploy scripts are forge scripts, and the same file works under both tools:

```solidity
import {Script} from "forge-std/Script.sol";
import {Counter} from "../src/Counter.sol";
import {Token} from "../src/Token.sol";

contract Deploy is Script {
    function run() public {
        vm.startBroadcast();               // signs with $PRIVATE_KEY
        Counter counter = new Counter();
        counter.increment();
        new Token(1_000_000e18);
        vm.stopBroadcast();
    }
}
```

```sh
plain deploy script/Deploy.s.sol --network base-sepolia --broadcast
```

`vm.startBroadcast(pk)` signs with another key, and `new X{salt: s}()` deploys through the deterministic CREATE2 deployer, like forge. External libraries the contracts link against are deployed first, from `$PRIVATE_KEY`'s address.

### What --broadcast does

1. While solc compiles, plain connects to the network and reads the chain id, the latest block and the fees in one round trip. It compiles only the target and what it deploys.
2. It runs the script against a fork of that block, then replays each transaction on its own, in order, the way the chain will run it. That gives each transaction's exact gas (sent with 30% headroom). Nothing is sent if a transaction would revert on chain or the sender can't pay.
3. It signs everything up front, sends the first transaction alone so a fee or balance problem stops the deploy early, sends the rest in one batch, and waits for the receipts.
4. It saves the addresses and verifies every contract it created.

### Addresses and verification

Addresses go to `deployments/<network>.json`, which is worth committing:

```json
{
  "Counter": "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512",
  "Token": "0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9"
}
```

Every contract a deploy creates, including proxies and contracts created by other contracts, is verified on Sourcify, the chain's Blockscout and Routescan explorers, and on Etherscan when `ETHERSCAN_API_KEY` is set (keys in foundry.toml's `[etherscan]` work too). Each explorer gets the contract's own sources and settings, exactly what solc compiled it from. If an explorer was down or a key was missing, `plain verify --network base-sepolia` retries what didn't verify without compiling anything, and you can pass names or addresses to verify only some contracts. `--no-verify` skips verification, and local chains skip it on their own.

## Networks

`--network` takes any chain on [chainlist.org](https://chainlist.org) by name, short name or chain id (`base`, `base-sepolia`, `arbitrum-sepolia`, `linea`, `8453`), `local` for `127.0.0.1:8545`, or a URL. Common chains use their official RPC. For the others, plain tries chainlist's public RPCs that don't track users and takes the first that answers with the right chain id.

Your own RPC always wins. `BASE_RPC_URL` sets it for `base` (any `<NAME>_RPC_URL` works the same way), and so does a `[networks]` entry in `plain.toml` or `rpc_endpoints` in `foundry.toml`:

```toml
[networks]
my-l2 = "https://rpc.my-l2.xyz"
staging = "${STAGING_RPC}"
```

## Configuration

Configuration is optional. `plain.toml` takes the same keys as `foundry.toml`'s `[profile.default]`, and either file works:

```toml
solc = "0.8.28"          # default: newest version satisfying every pragma
optimizer = true         # default: true (Foundry projects: on only if optimizer_runs is set, like forge)
optimizer_runs = 200
via_ir = false
evm_version = "cancun"   # default: solc's default
remappings = ["@x/=lib/x/src/"]
src = "src"              # default: src, or contracts if there is no src
[fuzz]
runs = 256
[invariant]
runs = 256
depth = 500
```

## Plugins

`plain foo args...` runs `plain-foo args...` from your `PATH`, with `PLAIN_ROOT` and `PLAIN_ARTIFACTS` in its environment, so a plugin can be written in any language.

## What plain doesn't do yet

Gas snapshot files, coverage, formatting, verifying contracts plain didn't deploy, more than one fork inside a test (`vm.createSelectFork` uses the single `--fork`), and the `vm.deployCode` cheatcode (use `vm.getCode` with `create`). Library linking only works for libraries under `src/`, `test/` or `script/`. If you need one of these, [open an issue](https://github.com/backmeupplz/plain/issues).
