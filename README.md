# plain

Compile, test, deploy, verify and upgrade EVM smart contracts with one ~7 MB binary. There's no config, no `npm install` and no `forge install`, your Foundry project works as it is, and the loop you run all day takes seconds instead of minutes.

```sh
curl -fsSL https://getplain.org/install | sh   # macOS and Linux
plain init                 # src/Counter.sol + test/Counter.t.sol
plain test --watch         # compile + run every test, again on every save
plain account new deployer # a deployer key, encrypted with your password
plain deploy Counter --network base-sepolia --account deployer --broadcast   # deploy, then verify on explorers
plain                      # or do all of it from a menu
```

Or build it from source: `cargo install --git https://github.com/backmeupplz/plain`. The [docs](https://getplain.org/docs/) walk through new and existing projects, testing and deploying.

| On real projects ([details](#benchmarks)) | Foundry | Hardhat 3 | plain |
|---|---|---|---|
| Edit a test, rerun the suite (300 tests, via-IR) | 56 s | 61 s | **0.99 s** |
| Edit a contract, rerun the suite | 103 s | 109 s | **2.1 s** |
| Build from scratch | 116 s | 115 s | **15 s** |
| Deploy 40 transactions over an RPC 80 ms away | 8.0 s | — | **2.0 s** |

- **Compile:** finds `src/` (or `contracts/`), `test/` and `script/`, picks and downloads the right `solc` from your pragmas (it reuses Foundry's solc cache if present), and compiles in parallel jobs. Results are cached by content hash, per project and in `~/.plain/cache`, which every checkout shares (git worktrees, fresh clones).
- **Test:** runs Solidity tests in the forge-std style (`*.t.sol`, `setUp`, `test*`, fuzzing, invariants, cheatcodes) on revm, in parallel. It also runs `test/**/*.test.ts` files against a fresh in-process chain per file. A suite whose compiled code and inputs haven't changed reuses its last results.
- **Deploy:** runs Solidity scripts (`vm.startBroadcast`) or a single contract, optionally behind an OpenZeppelin proxy. It always simulates first, against a fork of the target network, then signs and sends when you add `--broadcast`, and verifies every contract it created on Sourcify, Etherscan and the chain's Blockscout. Keys stay in encrypted keystores (`plain account`, Foundry's work too).
- **Upgrade:** `plain upgrade` checks that the new implementation keeps the proxy's storage layout, then deploys it and points the proxy at it.
- **Menu:** `plain` with no arguments opens a menu for all of the above that prints every command it runs.
- **Networks:** any chain on [chainlist.org](https://chainlist.org), by name or chain id (`base`, `base-sepolia`, `linea`, `8453`, ...), or a URL.
- **Zero setup:** `forge-std` is built into the binary, so `import {Test} from "forge-std/Test.sol";` works without installing anything. `lib/` dependencies, `remappings.txt` and `node_modules` (`@openzeppelin/...`) resolve automatically.
- **Foundry projects work as they are:** `foundry.toml` `[profile.default]` is read, including solc version, optimizer, via_ir, remappings, fuzz/invariant runs and rpc_endpoints.

## Commands

```
plain                       # in a terminal: a menu for everything below
plain build [--force]
plain test [filter] [-v] [--fork <network|url>] [--fuzz-runs N] [--seed N] [--rerun] [--force] [--watch]
plain deploy <Contract|path/Script.s.sol> [args...] [--network <name|id|url>] [--account <name>] [--broadcast] [--proxy] [--no-verify]
plain upgrade <proxy> [Contract] [--call <function> args...] --network <name|id|url> [--account <name>] [--broadcast]
plain verify [--network <name|id|url>] [name|address...] [--export]
plain account [new <name> | import <name>]
plain node [--port 8545] [--fork <network|url>]
plain init
plain <anything>            # runs `plain-<anything>` from PATH (plugins)
```

`filter` matches suite paths (`test/Vault.t.sol:VaultTest`) and test names, and only the test files it can match get compiled. Exit code is non-zero on any failure.

Test results are cached: a suite reruns only when its compiled bytecode, the test settings, the plain binary, or something it read (contracts it deployed with `getCode`, files, env vars) changed. Cached suites print `(cached)`. Suites that call `ffi`, `writeFile`, `unixTime` or `setEnv`, and every `--fork` run, always rerun. `--rerun` runs every test again, and `--force` also recompiles.

## Writing tests (agents: read this)

Solidity: Foundry style, using forge-std:

```solidity
import {Test, console} from "forge-std/Test.sol";
import {Vault} from "../src/Vault.sol";

contract VaultTest is Test {
    Vault vault;
    address alice = makeAddr("alice");

    function setUp() public { vault = new Vault(); deal(alice, 10 ether); }

    function test_Deposit(uint96 amount) public {          // parameters = fuzz test
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

    function invariant_Solvent() public view {             // invariant: random calls to contracts made in setUp
        assertGe(address(vault).balance, vault.totalDeposits());
    }
}
```

Failures print the decoded reason (custom errors, panics, `require` strings), the fuzz counterexample, `console.log` output, and a short call trace that follows the path to the revert:

```
FAIL test_Deposit(uint96): OutOfFunds
     args: 79228162514264337593543950335
     trace:
       VaultTest.test_Deposit(79228162514264337593543950335)  ✗ reverted without reason
         vm.prank(alice)
         Vault.deposit() value: 79228162514264337593543950335  ✗ OutOfFunds
```

Supported cheatcodes: `prank`/`startPrank`, `deal`, `warp`, `roll`, `fee`, `chainId`, `prevrandao`, `coinbase`, `store`/`load`, `etch`, nonces, `addr`/`sign`/`rememberKey`, `label`, `assume`, `skip`, `expectRevert` (all forms), `expectEmit`, `expectCall`, `mockCall`, `recordLogs`/`getRecordedLogs`, `snapshotState`/`revertToState`, `env*`, `setEnv`, `toString`, `parse*`, `getCode`, `readFile`/`writeFile`, `ffi`, `computeCreate*Address`, broadcast cheatcodes, gas snapshots, and every `assert*`. Anything else fails with `unsupported cheatcode vm.<name>(...)`.

TypeScript: any `test/**/*.test.ts` runs with `node --test` (or `bun test` if it imports `bun:test`). Each file gets its own fresh chain, and files run in parallel. Two environment variables are set: `PLAIN_RPC_URL` (anvil/hardhat-compatible JSON-RPC, chain id 31337, the 10 default anvil accounts funded and unlocked) and `PLAIN_ARTIFACTS` (`.plain/out/<Contract>.json`, holding `abi` and `bytecode`). With viem:

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

The chain supports `evm_snapshot`/`evm_revert`, `evm_increaseTime`, `evm_mine`/`anvil_mine`, `anvil_setBalance`/`setCode`/`setStorageAt`/`setNonce`, impersonation (every account is unlocked), `eth_getLogs`, and reverts with decoded custom errors.

## Deploying

```sh
plain account new deployer                                         # once: a key encrypted with your password
export ETHERSCAN_API_KEY=...                                       # optional: one key covers Etherscan's chains
plain deploy Counter 42 0xabc... --network sepolia --account deployer                  # simulate only
plain deploy script/Deploy.s.sol --network base --account deployer --broadcast         # sends txs, verifies
```

The deployer key comes from `--account <name>` (a keystore in `~/.plain/keystores` or Foundry's `~/.foundry/keystores`, unlocked with its password when something is sent), else `$PRIVATE_KEY`. Local chains (chain id 31337) use the first dev account. The project's `.env` is read like forge reads it. A simulation also prints what the deployer holds and the most the deploy can cost.

Scripts are forge-compatible: `run()`, `vm.startBroadcast()` (signing with the deployer), `vm.startBroadcast(pk)`, and `new X{salt: s}()`, which goes through the deterministic CREATE2 deployer. External libraries that the deployed contracts link against are deployed first, from the deployer's address.

What `--broadcast` does:
1. While solc compiles (only what the target deploys), plain connects to the network and reads the chain id, the latest block and fees in one round trip.
2. It runs the script against a fork of that block, then replays each transaction on its own, in order, as the chain will run it. That gives each transaction's exact gas (sent with 30% headroom), and it stops before sending anything if a transaction would revert on chain or the sender can't pay. The addresses where contracts will land are assumed empty during the run and then all checked in one round trip. If one isn't empty (say, a CREATE2 salt was used before), the replay runs again on what's really there.
3. It signs everything up front. The first transaction goes out alone, so a fee, balance or chain problem stops the deploy before anything else is in the mempool. The rest go out in one batch, in nonce order. Then plain waits for the receipts, asking only for each sender's next one until it's in.
4. It writes the addresses to `deployments/<network>.json` and verifies every contract created, including proxies and contracts created by other contracts. Each contract goes to every explorer at once: [Sourcify](https://sourcify.dev) (no key), Etherscan's v2 API when `ETHERSCAN_API_KEY` is set (foundry.toml `[etherscan]` keys work too), and the chain's Blockscout and Routescan explorers from chainlist. Each explorer gets the contract's own import closure as standard JSON input, the same sources and settings solc compiled it from, so it compiles no more than it has to. `--no-verify` skips this step.

What explorers need is kept in `.plain/deploys/<chain id>.json`, so `plain verify --network base` retries whatever didn't verify (an explorer was down, or the key was missing). It doesn't compile anything. To verify only some contracts, pass names or addresses. `--export` writes `verify/<network>/<Contract>.json` (the standard JSON input, libraries linked) and `<Contract>.txt` (address, compiler version, license, constructor arguments and the explorer's verify page) for verifying by hand.

Upgradeable contracts: `plain deploy Box <initialize args> --proxy` deploys the implementation and an OpenZeppelin proxy (`ERC1967Proxy` for UUPS contracts, else `TransparentUpgradeableProxy`) compiled from the project's `@openzeppelin/contracts`, calling `initialize` in the proxy's constructor. `deployments/<network>.json` gets `Box` (the proxy) and `Box_Implementation`. `plain upgrade Box [BoxV2] [--call initializeV2 args...]` reads the current implementation from the ERC-1967 slot, compares solc's storage layouts (variables must keep their slots and types; renames, appends and `__gap` use are fine; ERC-7201 namespaced storage isn't checked), then deploys the new implementation and calls `upgradeToAndCall` (or the ProxyAdmin's `upgradeAndCall`). With an Etherscan key, proxies are also marked as proxies there.

Networks: any chain on [chainlist.org](https://chainlist.org) by name, short name or chain id (`base`, `base-sepolia`, `arbitrum-sepolia`, `linea`, `monad`, `8453`), plus `local` (127.0.0.1:8545) and any URL. Common chains use their official RPC. For other chains, plain races chainlist's public RPCs that don't track users and takes the first one that answers with the right chain id. `$<NAME>_RPC_URL` picks the RPC for a name (for example `BASE_RPC_URL`), as does a `plain.toml` entry or foundry.toml's `rpc_endpoints`:

```toml
[networks]
my-l2 = "https://rpc.my-l2.xyz"
staging = "${STAGING_RPC}"
```

## Config (optional)

`plain.toml` uses the same keys as `foundry.toml`'s `[profile.default]`, and either file works:

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

Per-test fuzz runs: `/// forge-config: default.fuzz.runs = 10` above the function.

## Plugins

`plain foo args...` runs `plain-foo args...` from `PATH`, with `PLAIN_ROOT` and `PLAIN_ARTIFACTS` in its environment. Any language works.

## Benchmarks

Median wall times from hyperfine on an M1 Max (10 cores), 2026-10-17, same harness for every tool: forge 1.7.1, Hardhat 3.20.0, solc as pinned by each project. "Cold" means nothing compiled anywhere (plain's shared cache cleared too). All tests pass under `plain`: 303 in veydrift, 598 in v4-core, 568 in solmate.

Foundry / Hardhat / **plain**:

| | veydrift (via-IR) | Uniswap v4-core (via-IR) | solmate |
|---|---|---|---|
| cold build | 116s / 115s / **15s** | 141s / 144s / **12s** | 5.6s / 6.8s / **1.1s** |
| no-op build | 0.18s / 0.25s / **0.02s** | 0.24s / 0.27s / **0.02s** | 0.15s / 0.21s / **0.01s** |
| test, warm (plain reuses unchanged results) | 0.25s / 0.43s / **0.04s** | 4.7s / 5.8s / **0.61s**³ | 4.0s / 3.5s / **0.02s**¹ |
| test, warm, every test run (`--rerun`) | 0.25s / 0.43s / **0.10s** | 4.7s / 5.8s / **3.2s** | 4.0s / 3.5s / **1.6s**¹ |
| cold test (compile + test) | 113s / 115s / **15s** | 144s / 147s / **16s** | 9.5s / 9.9s / **2.7s**¹ |
| edit a test, run tests² | 56s / 61s / **0.99s** | 24s / 26s / **1.8s** | 4.6s / 4.2s / **0.07s** |
| edit a src function, run tests² | 103s / 109s / **2.1s** | 130s / 132s / **4.2s** | 6.0s / 5.7s / **1.5s** |
| new worktree/clone of built sources, run tests | cold test / cold test / **0.06s** | cold test / cold test / **0.76s** | cold test / cold test / **0.03s** |

¹ forge 1.x and Hardhat 3 dropped `testFail*`, so their solmate test timings exclude those tests (290 tests). `plain` runs all 568, including the legacy `testFail*` ones.

² One agent iteration: add a statement to one function, then run the whole test command. Every run is a new edit, the first one right after a build. The test edits are in the largest test file (veydrift `VeydriftGame.t.sol`, 7,500 lines; v4-core `PoolManager.t.sol`; solmate `ERC20.t.sol`), the src edits in `VeydriftGame.settleFirstPlanet`, `PoolManager.swap` and `ERC20.transfer`.

³ v4-core's two `ffi` suites (they spawn `npm` per call) run every time.

Deploying veydrift's `script/Deploy.s.sol` (40 transactions, 74M gas) to an anvil with 2 s blocks, forge script / **plain deploy** (Hardhat has no forge-style scripts):

| | local RPC | RPC 80 ms away⁴ |
|---|---|---|
| warm (built) | 4.1s / **1.9s** | 8.0s / **2.0s** |
| cold (nothing compiled) | 30s / **5.8s** | 35s / **6.1s** |

⁴ Every answer delayed 80 ms by a local proxy, like a public RPC on another continent. Waiting for the block is about 1 s of plain's time. The anvil has a 300M block gas limit: at anvil's default 30M it drops queued transactions that don't fit the next block, which plain resends (11 s) and forge waits on forever (`--slow`: 69 s).

Cold builds also peak at about 0.7 GB of memory under plain (veydrift, v4-core), against 3.2 to 4.6 GB for Foundry and Hardhat.

Where the speed comes from (solc itself is unchanged: what gets deployed is byte-for-byte what solc produces):
- **Tests don't embed the contracts they deploy.** In a test contract, `new Vault(args)` would make solc compile and optimize `Vault` again inside the test, once per test file (via-IR makes that the bulk of a build). plain compiles test files with each `new X(...)` replaced by a deploy of X's compiled artifact: the same init code, deployer, nonce and address. The arguments are still type-checked against X's constructor (through `abi.encodeCall` on a generated contract), so a wrong argument is the same compile error as before. If a rewritten file fails to compile, plain compiles the original and reports its errors.
- **Test files compile as units, and an edit recompiles only the tests it touched.** A big test file compiles as several units at once, each with only its share of the test functions (the others are cut out of the source, so solc doesn't even type-check them). Every test file, split or compiled whole, remembers which tests each unit holds. While only test bodies change, every unit keeps the tests it compiled unchanged, and edited or new tests compile into one small "hot" unit. The hot unit compiles without the optimizer, unless its code reads gas (`gasleft()`, gas snapshots), which takes about a third of solc's time. Only gas spent inside the test contract itself differs; the code under test is compiled exactly as configured.
- **Editing src/ doesn't recompile tests.** Since tests no longer contain src bytecode, a test file is cached by its own source plus the *interfaces* of what it imports (function bodies left out). Editing a function body recompiles that file and whatever inherits from it, not the tests that deploy it. Deploy scripts compile the same way: a script's own bytecode never goes on chain, and the transactions it broadcasts are byte-identical.
- **Unchanged tests don't run again.** A suite's results are reused while its bytecode, the settings and everything it read (deployed artifacts, files, env vars) are unchanged, so after an edit only the tests that can see it run. Tests are deterministic (fixed fuzz seed, no wall clock), and anything that isn't (`ffi`, `unixTime`, forks) always reruns.
- **Only what's needed compiles.** `plain test <filter>` compiles only test files the filter can match. `plain deploy` compiles only the target and what it deploys.
- **Scheduling:** solc jobs run on a work queue, biggest first, sized from the last build's timings (or, cold, from a model of how much code each file compiles). Foundry and Hardhat run one solc process at a time.
- **Shared cache:** a new worktree or clone reuses artifacts, and test results, that any checkout already produced.
- **Tests:** suites, tests and individual fuzz/invariant runs execute in parallel on an in-process revm. Each run starts from a copy-on-write view of the post-`setUp` state.
- **Deploys count round trips, not just CPU.** Connecting happens while solc compiles. Gas comes from replaying each transaction locally, so there are no `eth_estimateGas` calls. Where contracts will land is assumed empty and checked in one batch afterwards, instead of one fetch per contract. Transactions go out in one batch after the first, and receipt polling asks for each sender's next receipt only. veydrift's 40-transaction deploy makes 13 round trips instead of 77. A block that passes without our pending transactions triggers a resend (some nodes drop queued transactions).
- **Verification:** each contract goes to every explorer at once, with only its own import closure as input. Sourcify gets the creation transaction hash, so it doesn't have to search for it.
- **Startup:** one native binary with no JS runtime.

## Not included (on purpose)

Gas snapshot files, coverage, formatting, verifying contracts plain didn't deploy, multiple forks inside one test (`vm.createSelectFork` uses the single `--fork`), the `vm.deployCode` cheatcode (use `vm.getCode` + `create`), beacon proxies, and checking ERC-7201 namespaced storage on upgrades. Library linking only works for libraries under `src/`/`test/`/`script/`.

Because tests deploy contracts from artifacts (see Benchmarks), the gas a test reports for a `new X(...)` is a little higher than under forge, and the test contract's own bytecode differs (more so for tests you just edited, which compile without the optimizer until the file is re-split). Everything else is exactly solc's output: what `plain deploy` sends, and the artifacts in `.plain/out` (test and script contracts aren't written there).
