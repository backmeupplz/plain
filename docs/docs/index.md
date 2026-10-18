---
layout: docs
title: "plain docs: write, test, deploy, verify, upgrade"
description: "How to install plain and go from an empty folder to a verified, upgradeable deployment: write contracts, compile, test on every save, deploy with an encrypted account, verify on Etherscan and others, and upgrade OpenZeppelin proxies."
---

# plain docs
{: .no_toc}

<p class="lead">plain is one binary that compiles, tests, deploys, verifies and upgrades Solidity contracts. This page follows the loop you'll run, from an empty folder to an upgraded proxy, with or without Foundry on your machine.</p>

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

To update, run the installer again. To uninstall, delete `~/.plain` and the `# plain` lines the installer added to your shell profile. That folder holds the binary, its caches and your deployer accounts, so move the accounts somewhere safe first.

## The loop

Every step is one command, and each works the same in a new project or an existing Foundry one:

```sh
plain init                     # 1. write: a contract and its test to start from
plain build                    # 2. compile
plain test --watch             # 3. test, and again on every save
plain deploy Counter --network base-sepolia --account deployer --broadcast
                               # 4. deploy: simulate, send, save the address
plain verify --network base-sepolia
                               # 5. verify: deploys do it on their own, this retries
plain upgrade Box --network base-sepolia --account deployer --broadcast
                               # 6. upgrade a proxy to the contract as it is now
```

The sections below go through each step. If you'd rather not remember any of it, run `plain` on its own.

## The menu

`plain` with no arguments, in a terminal, opens a menu for the whole loop:

```
$ plain
plain. 0.3.0 · ~/code/my-project
Contracts live in src/ and tests in test/; each step shows the command it runs.
? What next?
❯ Build     compile the contracts
  Test      run the tests, once or on every save
  Deploy    put a contract on a chain, local or live
  Verify    publish the source code on block explorers
  Upgrade   point a proxy at a new implementation
  Settings  deployer accounts, Etherscan key, RPCs
  Quit
  ↑↓ move · enter pick · esc back
```

Each choice asks only what it needs, with the arrow keys or a line of text, and prints the command it runs before running it, so you learn the commands as you go and can put them in scripts later. Deploy asks for the contract, the network and the account, offers to put an upgradeable contract behind a proxy, asks for constructor or `initialize()` arguments one by one, and simulates before it asks you to confirm. Keys and passwords are typed without echo. The menu remembers your last network and account, and in a folder with no project it offers to create one. Escape goes back, and ctrl-c stops whatever is running and returns to the menu.

## 1. Write contracts

```sh
mkdir my-project && cd my-project
plain init
```

`plain init` writes three files: a `Counter` contract in `src/Counter.sol`, its test in `test/Counter.t.sol`, and a `.gitignore` that leaves out `.plain/`, `node_modules/` and `.env`. Contracts go in `src/` (or `contracts/` when there's no `src/`), tests in `test/` and deploy scripts in `script/`. No config file is needed, and forge-std ships inside the binary, so `import {Test} from "forge-std/Test.sol";` works without installing anything. Libraries come from npm or `lib/`, as [Dependencies](#dependencies) explains.

## 2. Compile

```sh
plain build           # compile src/, test/ and script/
plain build --force   # ignore every cache and recompile
```

The first build downloads the newest solc that satisfies your pragmas, or reuses one Foundry already downloaded, and `solc = "0.8.28"` in the config pins a version. Artifacts land in `.plain/out/<Contract>.json` with each contract's `abi` and `bytecode`. Builds are cached by content: a build with nothing changed takes about 20 ms, and a new clone or worktree of sources that any checkout already compiled compiles nothing, because every checkout shares `~/.plain/cache`.

## 3. Test

```sh
plain test                     # compile what changed, then run every test
plain test --watch             # the same, and again whenever you save a file
plain test Vault               # only test files, suites and tests matching "Vault"
plain test -v                  # list every test, with gas and fuzz runs (-vv, -vvv... work too)
plain test --mt test_Deposit   # forge's --match-test and --match-contract (--mc) work as filters
plain test --fork base         # run against a fork of a live network, at its latest block
plain test --fuzz-runs 1000 --seed 42
plain test --rerun             # run every suite again, even unchanged ones
plain test --force             # recompile everything, then run everything
```

`--watch` runs the tests, then watches `src/`, `test/`, `script/` and the config, and runs them again as soon as something changes. A compile error doesn't end it: fix the file and the next run starts on its own. Since unchanged suites reuse their results, a rerun after you edit one test usually takes about a second, even on big projects. It takes the other options too, so `plain test Vault --watch` keeps rerunning only the Vault tests.

A filter matches suite paths like `test/Vault.t.sol:VaultTest` and test names, and only the test files it can match get compiled. A filter that matches nothing is an error rather than a pass, and so is an option plain doesn't know, since a mistyped flag would otherwise run fewer tests than you meant. The exit code is non-zero when anything fails, so plain works in CI and in scripts as it is. `FOUNDRY_PROFILE=ci` picks `[profile.ci]` from `foundry.toml`, as with forge.

Tests run under the EVM rules your code is compiled for. That's the `evm_version` you configure; without one, plain follows forge: osaka, compiled for the newest EVM your solc supports, so the bytecode is the same forge would build. Fuzz inputs come partly from a dictionary of the constants in the contracts under test, which finds the magic values that random numbers miss.

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

Supported cheatcodes are `prank`/`startPrank`, `deal`, `warp`, `roll`, `fee`, `chainId`, `prevrandao`, `coinbase`, `store`/`load`, `etch`, nonces, `addr`/`sign`/`rememberKey`, `label`, `assume`, `skip`, `expectRevert` and `expectPartialRevert` in all their forms (with the reverting address too), `expectEmit`, `expectCall` (with value), `mockCall`, `recordLogs`/`getRecordedLogs`, `snapshotState`/`revertToState`, `env*`, `setEnv`, `toString`, `parse*`, `getCode`, `readFile`/`writeFile`/`exists`, `ffi`, `computeCreate*Address`, the broadcast cheatcodes, gas snapshots and every `assert*`. Anything else fails with `unsupported cheatcode vm.<name>(...)`. A comment like `/// forge-config: default.fuzz.runs = 10` above a test sets its fuzz runs, and `invariant.runs`, `invariant.depth` and `invariant.fail-on-revert` work the same way.

plain follows forge 1.0's rules where they changed. `expectRevert(Error.selector)` matches only an error with no arguments, and `expectPartialRevert` is the selector-only match. `testFail*` tests fail with a note, because a test that passes whenever its body fails also passes when it fails for the wrong reason. `vm.ffi` runs only with `ffi = true` in the config, and the file cheatcodes reach only what `fs_permissions` opens, exactly as in forge:

```toml
ffi = true
fs_permissions = [{ access = "read", path = "./deployments" }, { access = "read-write", path = "./out-files" }]
```

Invariant tests (`invariant*`, `statefulFuzz*`) check the state setUp leaves before the first call, call `afterInvariant()` after every run, and honor `targetContracts`, `targetSelectors`, `targetSenders` and their `exclude` forms. An invariant that has nothing to fuzz, or that uses `targetArtifacts` or `targetInterfaces`, fails rather than passing having checked less.

### Results are reused

A suite runs again only when its compiled code, the test settings, the plain binary, or something it read changed. That last part covers contracts it deployed through `getCode`, files and environment variables. Suites whose results were reused print `(cached)`. Suites that call `ffi`, `writeFile`, `unixTime` or `setEnv` always run, and so does every `--fork` run.

### TypeScript tests

TypeScript tests are programs that run with your permissions, so plain runs them only when the project asks for that in `plain.toml`:

```toml
typescript = true
```

Then any `test/**/*.test.ts` runs with `node --test`, or with `bun test` when it imports `bun:test`, and gets the project's `.env`. Each file gets its own fresh chain, and files run in parallel. Two environment variables point at it: `PLAIN_RPC_URL`, an anvil-compatible JSON-RPC endpoint on chain 31337 with the ten default anvil accounts funded and unlocked, and `PLAIN_ARTIFACTS`, the folder holding `<Contract>.json` files with `abi` and `bytecode`. With viem:

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

## 4. Deploy

A deploy needs three things: the contract, a network, and an account that signs the transactions and pays for their gas.

### A deployer account

Use an account made just for deploying rather than your main wallet. It only ever holds what deploys cost, and your wallet's key never goes near a terminal. plain makes one for you:

```sh
plain account new deployer
```

```
? new password for deployer (hidden)
? same password again (hidden)
created deployer: 0x9bbb06Cc6Ff76c8cD8211aE148638f6704b82E4E
```

The key is generated on your machine and saved in `~/.plain/keystores/deployer.json`, encrypted with your password in the keystore format that geth and Foundry's `cast wallet` use. You type the password only when a deploy actually sends something, since simulations need just the address, which the keystore keeps in the clear. Back up that file together with the password, because they're the only way back to the key.

Then send the new address some funds. On a testnet they're free from a faucet, such as [Chainlink's](https://faucets.chain.link/) (Sepolia, Base Sepolia, Arbitrum Sepolia and more), [Google Cloud's](https://cloud.google.com/application/web3/faucet/ethereum/sepolia) (Sepolia), or [Alchemy's](https://www.alchemy.com/faucets/base-sepolia) and [Coinbase's](https://portal.cdp.coinbase.com/products/faucet) (Base Sepolia). On a mainnet, send it a little ETH from your wallet; deploys on L2s usually cost cents or less.

There are other ways to sign:

- `plain account import deployer` stores a key you already have. You paste it into a hidden prompt and it's encrypted the same way.
- Keystores you made with `cast wallet import` work as they are, because `--account <name>` looks in `~/.foundry/keystores` too.
- `PRIVATE_KEY` in the environment or in the project's `.env` signs when there's no `--account`, which suits CI.
- On a local chain (`plain node`, anvil or Hardhat's node), plain signs with the first funded dev account, so you don't need a key at all.

`plain account` lists every account plain can see, with its address.

### Pick a network

`--network` takes any chain on [chainlist.org](https://chainlist.org) by name, short name or chain id (`base`, `base-sepolia`, `arbitrum-sepolia`, `linea`, `8453`), `local` for `127.0.0.1:8545`, or an RPC URL. You don't need an RPC provider: common chains use their official RPC, and for the rest plain tries chainlist's public RPCs that don't track users and takes the first that answers with the right chain id. [Networks](#networks) covers using your own RPC.

### Deploy

```sh
plain deploy Token 1000000000000000000000000 --network base-sepolia --account deployer               # simulate only
plain deploy Token 1000000000000000000000000 --network base-sepolia --account deployer --broadcast   # send and verify
```

Constructor arguments follow the contract name. Without `--broadcast`, plain simulates the deploy against a fork of the network as it is now and prints each transaction with its gas, along with what the account holds and the most the deploy can cost, so you see everything before anything is spent:

```
1. create Counter (from 0x9bbb06Cc6Ff76c8cD8211aE148638f6704b82E4E, gas 95173)
0x9bbb06Cc6Ff76c8cD8211aE148638f6704b82E4E has 0 ETH; sending can cost up to 0.000001361 ETH; that's not enough, so fund it first
simulated 1 transaction(s) against base-sepolia; add --broadcast to send
```

With `--broadcast`, plain asks for the account's password, sends the transactions, saves the addresses and verifies every contract the deploy created. Add `--confirm` to see the transactions and the most they can cost first, and send only when you answer yes; the menu deploys this way.

### To a local chain

`plain node` starts a chain on `127.0.0.1:8545` that speaks the same JSON-RPC as anvil and Hardhat, with the usual ten funded dev accounts. In another terminal:

```sh
plain deploy Counter --network local --broadcast
```

The menu starts a local chain by itself when you pick `local` and none is running, and that chain lasts as long as the menu does.

### With a script

Deploy scripts are forge scripts, and the same file works under both tools:

```solidity
import {Script} from "forge-std/Script.sol";
import {Counter} from "../src/Counter.sol";
import {Token} from "../src/Token.sol";

contract Deploy is Script {
    function run() public {
        vm.startBroadcast();               // signs with --account, or $PRIVATE_KEY
        Counter counter = new Counter();
        counter.increment();
        new Token(1_000_000e18);
        vm.stopBroadcast();
    }
}
```

```sh
plain deploy script/Deploy.s.sol --network base-sepolia --account deployer --broadcast
```

In `run()`, `msg.sender` and `tx.origin` are the deployer, as with forge, so `new Vault(msg.sender)` makes the deployer the owner. `vm.startBroadcast(pk)` signs with another key, and `new X{salt: s}()` deploys through the deterministic CREATE2 deployer, like forge. External libraries the contracts link against are deployed first, from the deployer's address. A script runs `run()` with no arguments.

### What --broadcast does

1. While solc compiles, plain connects to the network and reads the chain id, the latest block and the fees in one round trip.
2. It runs the script against a fork of that block under the EVM rules the code was compiled for, with the contract size limits chains enforce, then replays each transaction on its own, in order, the way the chain will run it. That gives each transaction's gas, sent with 30% headroom over what it needs before refunds. Nothing is sent if a transaction would revert on chain or the account can't pay.
3. It compiles every contract the transactions create again, from exactly the sources and settings explorers will get, and sends nothing unless the bytecode is identical to what it's about to put on chain. A stale or tampered cache can't deploy anything your sources don't say.
4. It checks that the account's next nonce is the one the simulation used, since later transactions call contracts at addresses that follow from it. It signs everything up front, sends the first transaction alone so a fee or balance problem stops the deploy early, sends the rest in one batch, and waits for the receipts.
5. It saves the addresses and the verification records of what landed, even when something after it failed, and verifies every contract it created. A node on your machine (`plain node`, anvil, or a fork of a live chain) is never verified.

### Addresses

Addresses go to `deployments/<network>.json`, which is worth committing:

```json
{
  "Counter": "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512",
  "Token": "0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9"
}
```

## 5. Verify

Verification is part of every deploy. Each contract the deploy creates, including proxies, libraries and contracts created by other contracts, goes to every explorer at once, with exactly the sources and settings solc compiled it from:

- **Sourcify**, which needs no key.
- **The chain's Blockscout and Routescan explorers**, found on chainlist.org, which need no key either.
- **Etherscan and its family** (Basescan, Arbiscan, Polygonscan and the rest of Etherscan's chains), which need an API key.

### The Etherscan key

A key is free at [etherscan.io/myapikey](https://etherscan.io/myapikey), and one key covers the whole Etherscan family. plain takes the first of these it finds:

- `ETHERSCAN_API_KEY` in the environment or in the project's `.env`.
- An `[etherscan]` key in `foundry.toml`, as Foundry projects already have.
- The key saved from the menu's Settings, which asks for it without echo and keeps it in `~/.plain/config.toml`, readable only by you and used by every project.

Without a key, plain verifies everywhere else and says it skipped Etherscan.

### Retry

If an explorer was down or the key was missing, `plain verify --network base-sepolia` sends what didn't verify yet, without compiling anything. Names or addresses after it limit it to those contracts. `--no-verify` on a deploy skips verification until you run this, and local chains are never verified.

### Verify by hand

Some explorers, or a contract you'd rather check yourself, need the files instead:

```sh
plain verify --network base-sepolia --export
```

For each contract plain deployed there, this writes `verify/base-sepolia/<Contract>.json`, the standard JSON input to upload (with linked libraries filled in), and `<Contract>.txt` with everything the explorer's form asks for, like this:

```
Counter at 0x5FbDB2315678afecb367f032d93F642f64180aa3 (chain 84532)

Verify page:
  https://sepolia.basescan.org/verifyContract?a=0x5FbDB2315678afecb367f032d93F642f64180aa3
  https://base-sepolia.blockscout.com/address/0x5FbDB2315678afecb367f032d93F642f64180aa3

Compiler type:     Solidity (Standard-Json-Input)
Compiler version:  v0.8.28+commit.7893614a
License:           MIT
File to upload:    Counter.json
Contract name:     src/Counter.sol:Counter
Constructor args:  none
```

On Basescan or any other Etherscan-family explorer, open the verify page (the link already carries the address), pick those values, upload the JSON file, and paste the constructor arguments if there are any. Blockscout's verification form takes the same file and values.

## 6. Upgrade

A contract behind a proxy keeps its address and its storage while the code behind it changes. plain deploys OpenZeppelin's proxies and upgrades them, and before each upgrade it checks that the new code keeps the data the proxy already holds.

### Write an upgradeable contract

Install OpenZeppelin's contracts, then write the contract the upgradeable way, with an `initialize()` function in place of the constructor and `UUPSUpgradeable` so that the contract can upgrade itself:

```sh
npm install @openzeppelin/contracts @openzeppelin/contracts-upgradeable
```

```solidity
import {Initializable} from "@openzeppelin/contracts-upgradeable/proxy/utils/Initializable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";
import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";

contract Box is Initializable, OwnableUpgradeable, UUPSUpgradeable {
    uint256 public value;

    /// @custom:oz-upgrades-unsafe-allow constructor
    constructor() {
        _disableInitializers();
    }

    function initialize(address owner, uint256 v) public initializer {
        __Ownable_init(owner);
        value = v;
    }

    function _authorizeUpgrade(address) internal override onlyOwner {}
}
```

### Deploy it behind a proxy

```sh
plain deploy Box 0x9bbb06Cc6Ff76c8cD8211aE148638f6704b82E4E 42 --proxy --network base-sepolia --account deployer --broadcast
```

With `--proxy`, the arguments go to `initialize()`, which runs in the same transaction that creates the proxy, so nobody can initialize it before you. A UUPS contract goes behind an `ERC1967Proxy`. Any other contract goes behind a `TransparentUpgradeableProxy`, whose `ProxyAdmin` belongs to the deployer account. Both proxies are compiled from your own `@openzeppelin/contracts` and verified along with the contract. `deployments/<network>.json` keeps the proxy under the contract's name, since that's the address you'll use, and the implementation next to it:

```json
{
  "Box": "0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0",
  "Box_Implementation": "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"
}
```

With an Etherscan key, plain also tells Etherscan that the proxy is a proxy, so its page reads and writes through `Box`'s functions.

### Upgrade it

Change `Box.sol`, then:

```sh
plain upgrade Box --network base-sepolia --account deployer --broadcast
```

plain reads the proxy's current implementation from the chain, compares its storage layout with the new code's, deploys the new implementation, and calls `upgradeToAndCall` on the proxy (or `upgradeAndCall` on the ProxyAdmin of a transparent proxy). Without `--broadcast` it stops after the check and the simulation, as in this upgrade on a local chain:

```
storage layout: BoxV2 can take over Box's storage; adds `bonus`
1. create Box_Implementation (from 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266, gas 611487)
2. call Box (0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0).upgradeToAndCall(0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9, 0x5b4e128c…0007) (from 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266, gas 67452)
simulated 2 transaction(s) against local; add --broadcast to send
```

To upgrade to a contract with another name, put it after the proxy, as in `plain upgrade Box BoxV2`. To call a function as part of the upgrade, such as a `reinitializer(2)` that sets up new variables, add `--call` with its name and the arguments after the contract: `plain upgrade Box BoxV2 --call initializeV2 7`. The proxy can be given by its address too.

### The storage check

The new contract has to keep every variable the proxy already stores, in the same slot and with a compatible type. It may rename variables, add new ones after the existing ones, or take new ones out of a `__gap` array. Anything else would make the proxy read old data as the wrong variable, so plain refuses:

```
error: BoxBad can't take over Box's storage:
  `value` moved from slot 0 to slot 1 (was something added before it?)
fix the order of the variables, or --unsafe-skip-storage-check if this is intended
```

The check compares solc's storage layout for the deployed implementation, compiled from the sources plain saved when it deployed it, with the layout of the new contract. It goes into structs, arrays and mappings, compares what a user-defined value type wraps and an enum's members (which may only be added at the end), and checks every ERC-7201 namespace the same way, which is where OpenZeppelin 5's own contracts keep their state. A proxy plain didn't deploy has no saved layout to compare with, so check its layout yourself and upgrade it with `--unsafe-skip-storage-check`. A UUPS proxy can only be upgraded to a contract that can upgrade again, and the simulation checks that the proxy really points at the new implementation afterwards. It also catches an upgrade from the wrong account before anything is sent:

```
error: upgrading Box reverted: OwnableUnauthorizedAccount(0x70997970C51812dc3A010C7d01b50e0d17dc79C8) (may 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 upgrade it?)
```

## Networks

You can deploy to any chain without setting up an RPC, because plain knows every chain on chainlist.org and finds a public RPC that works. Your own RPC always wins when there is one, which is worth having for heavy use or a provider you trust. `BASE_RPC_URL` sets it for `base`, and any `<NAME>_RPC_URL` works the same way, from the environment or the project's `.env`. So does a `[networks]` entry in `plain.toml` or `rpc_endpoints` in `foundry.toml`:

```toml
[networks]
my-l2 = "https://rpc.my-l2.xyz"
staging = "${STAGING_RPC}"
```

RPC URLs from providers like Alchemy or Infura contain your API key, so keep them in `.env`, which `plain init` leaves out of git, rather than in `plain.toml`. The menu's Settings can save an RPC URL to `.env` for you.

## Existing projects

### Foundry projects

Run plain in the project root as it is. It reads `foundry.toml`'s `[profile.default]` (or `FOUNDRY_PROFILE`'s: solc version, optimizer, via-IR, EVM version, remappings, fuzz and invariant settings, `ffi`, `fs_permissions`, `rpc_endpoints` and `[etherscan]` keys), `remappings.txt`, `lib/` and `.env`, so nothing needs to change. Add `.plain/` to your `.gitignore`.

plain and forge can share a project. plain writes only to `.plain/`, plus `deployments/` when you deploy and `verify/` when you export, and forge keeps using `out/` and `cache/`, so you can keep both and switch whenever you like. Keystores from `cast wallet import` work with `--account`, and forge scripts deploy with `plain deploy` unchanged.

### With or without Foundry installed

plain doesn't need Foundry, Node or anything else on your machine. If Foundry is installed, plain reuses the solc binaries it already downloaded and the keystores in `~/.foundry/keystores`. If it isn't, plain downloads solc into `~/.plain` the first time a project needs it. Either way the result is the same.

forge-std is built in, so a project works even when `lib/forge-std` isn't checked out. Other libraries in `lib/` still need `git submodule update --init --recursive` after a fresh clone, as they would for forge.

### Hardhat projects

Contracts in `contracts/` and imports from `node_modules` compile as they are. plain runs Solidity tests, and plain TypeScript tests once `typescript = true` is set (see above), so tests written for Hardhat's own runner keep running under Hardhat.

## Dependencies

plain resolves imports the way forge does, and it also looks in `node_modules`:

- **From npm:** after `npm install @openzeppelin/contracts`, an import like `import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";` works without any remapping.
- **From git:** put the library in `lib/`, for example with `git submodule add https://github.com/OpenZeppelin/openzeppelin-contracts lib/openzeppelin-contracts`. Every `lib/<name>` is importable as `<name>/`, pointing at its `src/` or `contracts/` folder, and the library's own `remappings.txt` applies too, which is what makes OpenZeppelin's `@openzeppelin/contracts/...` paths work.
- **Anything else:** add remappings to `remappings.txt`, or as `remappings = ["@x/=lib/x/src/"]` in `plain.toml` or `foundry.toml`.

## Configuration

Configuration is optional. `plain.toml` takes the same keys as `foundry.toml`'s `[profile.default]`, and either file works:

```toml
solc = "0.8.28"          # default: newest version satisfying every pragma
optimizer = true         # default: true (Foundry projects: on only if optimizer_runs is set, like forge)
optimizer_runs = 200
via_ir = false
evm_version = "cancun"   # default: as forge: osaka, compiled for the newest your solc supports
remappings = ["@x/=lib/x/src/"]
src = "src"              # default: src, or contracts if there is no src
ffi = false              # vm.ffi runs commands: off unless set, as in forge
fs_permissions = []      # what the file cheatcodes may read or write, as in forge
typescript = false       # run test/**/*.test.ts (plain.toml only)
[fuzz]
runs = 256
[invariant]
runs = 256
depth = 500
```

Secrets stay out of these files. plain reads the project's `.env` for `PRIVATE_KEY`, `ETHERSCAN_API_KEY`, `<NAME>_RPC_URL`, `${VAR}` in the config and the `vm.env*` cheatcodes, after your environment's own variables. It never puts `.env` into its environment or that of the programs it starts, so a cloned repository's `.env` can't change how plain or solc runs. Settings for every project, like the Etherscan key the menu saves, live in `~/.plain/config.toml`.

## Security

Building and testing a repository with plain stays inside the EVM and the repository's own folder unless its config asks for more, as with forge. That config can ask, so read a `foundry.toml` or `plain.toml` you don't trust like code (it can also name the RPCs your deploys go through):

- `plain build` runs nothing but solc. Solc versions come checksummed from soliditylang.org, or from Foundry's own cache, and a repository's `.env` can't point plain at another one.
- Tests run in the EVM. `vm.ffi` needs `ffi = true`, the file cheatcodes reach only what `fs_permissions` opens (never `.git`, `.plain`, the config or `.env`), and TypeScript tests need `typescript = true`.
- The build cache is keyed with a secret of your machine's own, so a `.plain/` folder that comes with a repository can't stand in for compiling it. Deploy records in `.plain/deploys` are checked before plain reads files or uploads anything they name.
- A deploy sends only bytecode that solc produces from your sources: everything it creates is compiled again, from the exact input explorers verify, and compared before signing. `plain build --check` runs the same comparison over every contract, for CI or before an audit.
- Keys stay in encrypted keystores, and a key is unlocked only when a transaction is about to be signed.

## Plugins

`plain foo args...` runs `plain-foo args...` from your `PATH`, with `PLAIN_ROOT` and `PLAIN_ARTIFACTS` in its environment, so a plugin can be written in any language.

## What plain doesn't do yet

Gas snapshot files, coverage, formatting, verifying contracts plain didn't deploy, more than one fork inside a test (`vm.createSelectFork` uses the single `--fork`), the `vm.deployCode` cheatcode (forge-std's `deployCode` works), `targetArtifacts`/`targetInterfaces` in invariant tests, OpenZeppelin 4's transparent proxies, beacon proxies, and Windows. Library linking only works for libraries under `src/`, `test/` or `script/`. If you need one of these, [open an issue](https://github.com/backmeupplz/plain/issues).
