# plain

Compile, test and deploy EVM smart contracts. One ~6 MB binary with no config, no `npm install` and no `forge install`.

```sh
plain init                 # src/Counter.sol + test/Counter.t.sol
plain test                 # compile + run every test
plain deploy Counter --network base-sepolia --broadcast
```

- **Compile:** finds `src/` (or `contracts/`), `test/` and `script/`, picks and downloads the right `solc` from your pragmas (it reuses Foundry's solc cache if present), and compiles in parallel jobs. Results are cached per file, keyed by content hash.
- **Test:** runs Solidity tests in the forge-std style (`*.t.sol`, `setUp`, `test*`, fuzzing, invariants, cheatcodes) on revm, in parallel. It also runs `test/**/*.test.ts` files against a fresh in-process chain per file.
- **Deploy:** runs Solidity scripts (`vm.startBroadcast`) or a single contract. It always simulates first, against a fork of the target network, then signs and sends when you add `--broadcast`.
- **Zero setup:** `forge-std` is built into the binary, so `import {Test} from "forge-std/Test.sol";` works without installing anything. `lib/` dependencies, `remappings.txt` and `node_modules` (`@openzeppelin/...`) resolve automatically.
- **Foundry projects work as they are:** `foundry.toml` `[profile.default]` is read, including solc version, optimizer, via_ir, remappings, fuzz/invariant runs and rpc_endpoints.

## Commands

```
plain build [--force]
plain test [filter] [-v] [--fork <network|url>] [--fuzz-runs N] [--seed N]
plain deploy <Contract|path/Script.s.sol> [constructor args...] [--network <name|url>] [--broadcast]
plain node [--port 8545] [--fork <network|url>]
plain init
plain <anything>            # runs `plain-<anything>` from PATH (plugins)
```

`filter` matches suite paths (`test/Vault.t.sol:VaultTest`) and test names. Exit code is non-zero on any failure.

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
export PRIVATE_KEY=0x...
plain deploy Counter 42 0xabc... --network sepolia                # simulate only
plain deploy script/Deploy.s.sol --network base --broadcast       # forge-style script, sends txs
```

Scripts are forge-compatible: `run()`, `vm.startBroadcast()` (the key comes from `$PRIVATE_KEY`), `vm.startBroadcast(pk)`, and `new X{salt: s}()`, which goes through the deterministic CREATE2 deployer. Deployed addresses are written to `deployments/<network>.json`.

Networks: `mainnet sepolia base base-sepolia optimism optimism-sepolia arbitrum arbitrum-sepolia polygon polygon-amoy bsc avalanche gnosis local`, or any URL. `$<NAME>_RPC_URL` overrides a built-in URL (for example `BASE_RPC_URL`). Add your own in `plain.toml`:

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

The numbers below are median wall times from hyperfine on an M1 Max (10 cores), using the same harness for every tool. Tool versions are forge 1.7.1, Hardhat 3.20.0 and solc as pinned by each project. The machine was under unrelated background load, which affected all three tools alike. All tests pass under `plain`: 303 in veydrift, 598 in v4-core and 568 in solmate.

| | veydrift (via-IR, 141 files) | Uniswap v4-core (via-IR) | solmate |
|---|---|---|---|
| cold build: Foundry / Hardhat / **plain** | 117s / 120s / **65s** | 155s / 145s / **29s** | 5.6s / 8.7s / **1.4s** |
| no-op build | 0.19s / 0.25s / **0.02s** | 0.24s / 0.27s / **0.02s** | 0.15s / 0.76s / **0.01s** |
| test, warm | 0.26s / 0.42s / **0.10s** | 4.45s / 6.16s / **2.91s** | 4.05s / 3.46s / **3.43s**¹ |

¹ forge 1.x and Hardhat 3 dropped `testFail*`, so their solmate timings exclude those tests (290 tests). `plain` runs all 568, including the legacy `testFail*` ones.

Where the speed comes from:
- **Cold builds:** sources are split into one solc job per core, balanced by import-closure size. Foundry and Hardhat run one solc process at a time.
- **No-op builds:** only content hashes are checked, with no other work.
- **Tests:** tests and individual fuzz/invariant runs execute in parallel on a single in-process revm.
- **Startup:** one native binary with no JS runtime.

## Not included (on purpose)

Gas snapshot files, coverage, formatting, Etherscan verification, multiple forks inside one test (`vm.createSelectFork` uses the single `--fork`), and the `vm.deployCode` cheatcode (use `vm.getCode` + `create`). Library linking only works for libraries under `src/`/`test/`/`script/`.
