import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createTestClient, http, publicActions, walletActions, parseEther, BaseError, ContractFunctionRevertedError } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { foundry } from "viem/chains";

const Counter = JSON.parse(readFileSync(`${process.env.PLAIN_ARTIFACTS}/Counter.json`, "utf8"));
const client = createTestClient({ chain: foundry, mode: "anvil", transport: http(process.env.PLAIN_RPC_URL) })
  .extend(publicActions)
  .extend(walletActions);
const [owner] = await client.getAddresses();

async function deploy() {
  const hash = await client.deployContract({ abi: Counter.abi, bytecode: Counter.bytecode, account: owner });
  const { contractAddress } = await client.waitForTransactionReceipt({ hash });
  return contractAddress!;
}

test("deploy, write, read, events", async () => {
  const address = await deploy();
  const hash = await client.writeContract({ address, abi: Counter.abi, functionName: "set", args: [42n], account: owner });
  const receipt = await client.waitForTransactionReceipt({ hash });
  assert.equal(receipt.status, "success");
  assert.equal(await client.readContract({ address, abi: Counter.abi, functionName: "number" }), 42n);
  const logs = await client.getContractEvents({ address, abi: Counter.abi, eventName: "Set", fromBlock: 0n });
  assert.equal(logs.length, 1);
  assert.equal(logs[0].args.value, 42n);
});

test("custom error revert is decoded", async () => {
  const address = await deploy();
  const stranger = privateKeyToAccount("0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");
  await assert.rejects(
    client.simulateContract({ address, abi: Counter.abi, functionName: "set", args: [1n], account: stranger }),
    (e: unknown) => {
      const r = e instanceof BaseError && e.walk((x) => x instanceof ContractFunctionRevertedError);
      return r instanceof ContractFunctionRevertedError && r.data?.errorName === "NotOwner";
    },
  );
});

test("signed raw tx, value transfer, snapshot/revert, time travel", async () => {
  const address = await deploy();
  const alice = privateKeyToAccount("0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a");
  const id = await client.snapshot();
  const hash = await client.writeContract({ address, abi: Counter.abi, functionName: "fund", value: parseEther("1"), account: alice });
  await client.waitForTransactionReceipt({ hash });
  assert.equal(await client.getBalance({ address }), parseEther("1"));
  await client.revert({ id });
  assert.equal(await client.getBalance({ address }), 0n);
  const before = (await client.getBlock()).timestamp;
  await client.increaseTime({ seconds: 3600 });
  await client.mine({ blocks: 1 });
  assert.ok((await client.getBlock()).timestamp >= before + 3600n);
});
