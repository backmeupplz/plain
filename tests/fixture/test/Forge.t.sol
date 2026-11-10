// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test, Vm} from "forge-std/Test.sol";
import {Counter} from "../src/Counter.sol";
import {UsesLib} from "../src/Lib.sol";

/// Writes slot 0 and reports what the write cost and what transient slot 0 held before.
contract Probe {
    uint256 value;

    function write(uint256 next) external returns (uint256 cost, uint256 transient) {
        uint256 before = gasleft();
        assembly { sstore(0, next) }
        cost = before - gasleft();
        assembly {
            transient := tload(0)
            tstore(0, next)
        }
    }
}

contract Token {
    mapping(address => uint256) public balanceOf;
}

// forge 1.x behavior that plain matches
contract ForgeTest is Test {
    struct Pair { uint256 z; string a; }
    // forge leaves mapping and function members out when it orders an object by a struct's members
    struct WithMapping { uint256 mz; mapping(uint256 => uint256) mm; uint256 my; }
    struct WithFunction { uint256 fz; function() external fe; uint256 fy; }

    function test_setUpLikeForge() public view {
        assertEq(address(this), 0x7FA9385bE102ac3EAc297483Dd6233D62b3e1496, "SENDER's nonce 1");
        assertEq(address(this).balance, type(uint96).max);
    }

    // vm.skip(true) on some of the inputs skips the whole fuzz test, as in forge
    function testFuzz_skipsSomeInputs(uint256 x) public {
        vm.skip(x % 2 == 0);
        assertEq(x % 2, 1);
    }

    function testFuzz_skipFalseRuns(uint256 x) public {
        vm.skip(false);
        assertEq(x - x, 0);
    }

    // vm.skip(true, reason) skips with that reason, and vm.skip(false, reason) runs on
    function test_skipWithReason() public {
        vm.skip(true, "needs mainnet");
        revert("ran after vm.skip(true, reason)");
    }

    function test_skipFalseWithReasonRuns() public {
        vm.skip(false, "never shown");
        assertEq(block.chainid, 31337);
    }

    function testFuzz_skipWithReason(uint256 x) public {
        vm.skip(x % 2 == 0, "even input");
        assertEq(x % 2, 1);
    }

    /// forge-config: default.isolate = true
    function test_isolatedCallsAreTransactions() public {
        Probe p = new Probe();
        p.write(1);
        (uint256 cost, uint256 transient) = p.write(2);
        assertGe(cost, 5000, "a cold, clean slot");
        assertEq(transient, 0, "transient storage starts empty");
        assertGt(vm.lastCallGas().gasTotalUsed, 21_000, "the transaction's gas, intrinsic included");
    }

    function test_callsShareTheTransaction() public {
        Probe p = new Probe();
        p.write(1);
        (uint256 cost, uint256 transient) = p.write(2);
        assertLt(cost, 5000, "warm and dirty");
        assertEq(transient, 1);
        assertLt(vm.lastCallGas().gasTotalUsed, 21_000);
    }

    function test_coolMakesStorageCold() public {
        Probe p = new Probe();
        p.write(1);
        vm.cool(address(p));
        (uint256 cost,) = p.write(2);
        assertGe(cost, 2100, "a cold slot");
    }

    function test_pausedMeteringSpendsNothing() public {
        vm.pauseGasMetering();
        uint256 before = gasleft();
        uint256 x;
        for (uint256 i; i < 1000; i++) x += i;
        uint256 left = gasleft();
        vm.resumeGasMetering();
        assertEq(before, left);
        assertEq(x, 499500);
    }

    function test_expectRevertTakesOutOfFunds() public {
        Counter c = new Counter();
        vm.prank(address(0xdead));
        vm.expectRevert();
        c.fund{value: 1 ether}();
    }

    function test_librariesLinked() public {
        UsesLib u = new UsesLib();
        assertEq(u.three(), 3);
        assertEq(keccak256(vm.getDeployedCode("Lib.sol:UsesLib")), keccak256(address(u).code));
    }

    function test_json() public {
        string memory j = '{"b":{"n":"0x10","list":[1,2,3],"who":"0x00000000000000000000000000000000000000aa"},"a":[{"a":"s","z":1}]}';
        assertEq(vm.parseJsonUint(j, ".b.n"), 16);
        assertEq(vm.parseJsonUintArray(j, ".b.list")[2], 3);
        assertEq(vm.parseJsonAddress(j, "$.b.who"), address(0xaa));
        assertTrue(vm.keyExistsJson(j, ".a[0].z") && !vm.keyExistsJson(j, ".nope"));
        assertEq(vm.parseJsonKeys(j, "$")[0], "b", "keys in the order of the text");
        // an object decodes in the member order of the struct with its keys (z, a), not by key
        Pair[] memory pairs = abi.decode(vm.parseJson(j, ".a"), (Pair[]));
        assertEq(pairs[0].z, 1);
        assertEq(pairs[0].a, "s");
        vm.serializeUint("o", "n", 5);
        assertEq(vm.serializeAddress("o", "who", address(0xaa)), '{"n":5,"who":"0x00000000000000000000000000000000000000AA"}');
    }

    function test_jsonSkipsMappingAndFunctionMembers() public pure {
        // (mz, my) and (fz, fy), not by key
        (uint256 a, uint256 b) = abi.decode(vm.parseJson('{"my": 1, "mz": 2}'), (uint256, uint256));
        assertEq(a, 2);
        assertEq(b, 1);
        (a, b) = abi.decode(vm.parseJson('{"fy": 1, "fz": 2}'), (uint256, uint256));
        assertEq(a, 2);
        assertEq(b, 1);
        // with the skipped member's key too, no struct has the members: by key (mm, my, mz)
        (uint256 c, , uint256 d) = abi.decode(vm.parseJson('{"mz": 1, "my": 2, "mm": 3}'), (uint256, uint256, uint256));
        assertEq(c, 3);
        assertEq(d, 1);
    }

    function test_recordAndStdStorageDeal() public {
        Token t = new Token();
        // forge-std's ERC-20 deal finds the balance slot with vm.record/vm.accesses
        deal(address(t), address(0xbeef), 7);
        assertEq(t.balanceOf(address(0xbeef)), 7);
        vm.record();
        t.balanceOf(address(1));
        (bytes32[] memory reads, bytes32[] memory writes) = vm.accesses(address(t));
        assertEq(reads.length, 1);
        assertEq(writes.length, 0);
    }

    function test_envArrays() public {
        vm.setEnv("PLAIN_FIXTURE_LIST", "1, 2,3");
        assertEq(vm.envUint("PLAIN_FIXTURE_LIST", ",")[1], 2);
        uint256[] memory dflt = new uint256[](1);
        assertEq(vm.envOr("PLAIN_FIXTURE_UNSET", ",", dflt).length, 1);
    }

    function test_writeJsonAndDumpState() public {
        vm.writeJson('{"z":1,"a":2}', "tmp/out.json");
        vm.writeJson("3", "tmp/out.json", ".m.k");
        assertEq(vm.readFile("tmp/out.json"), '{\n  "z": 1,\n  "a": 2,\n  "m": {\n    "k": 3\n  }\n}');
        vm.removeFile("tmp/out.json");
        vm.etch(address(0xc0ffee), hex"00");
        vm.store(address(0xc0ffee), 0, bytes32(uint256(9)));
        vm.dumpState("tmp/state.json");
        string memory s = vm.readFile("tmp/state.json");
        assertEq(vm.parseJsonUint(s, ".0x0000000000000000000000000000000000c0ffee.storage.0x0000000000000000000000000000000000000000000000000000000000000000"), 9);
        assertEq(vm.parseJsonString(s, ".0x0000000000000000000000000000000000c0ffee.code"), "0x00");
    }
}

// vm.createSelectFork inside a test, against the mock RPC tests/fixture.rs serves (chain 8453, block 100,
// 0x…beef returning its slot 0, which holds 42)
contract ForkTest is Test {
    uint256 kept = 7;

    function test_createSelectFork() public {
        string memory rpc = vm.envOr("PLAIN_FIXTURE_RPC", string(""));
        if (bytes(rpc).length == 0) return;
        Counter local = new Counter();
        uint256 id = vm.createSelectFork(rpc, 100);
        assertEq(vm.activeFork(), id);
        assertEq(block.chainid, 8453);
        assertEq(block.number, 100);
        (bool ok, bytes memory out) = address(0xbeef).staticcall("");
        assertTrue(ok);
        assertEq(abi.decode(out, (uint256)), 42, "the fork's state");
        assertEq(kept, 7, "the test contract keeps its own");
        assertEq(address(local).code.length, 0, "what it made before forking is gone");
        // without a block: the RPC's latest (100 here), asked once
        vm.createSelectFork(rpc);
        assertEq(block.number, 100);
        assertEq(vm.activeFork(), id + 1);
    }
}

// vm.skip(true, reason) in setUp skips every test of the contract with that reason
contract ForgeSkipSetUpTest is Test {
    function setUp() public {
        vm.skip(true, "no fork configured");
    }

    function test_skippedBySetUp() public pure {
        revert("ran after setUp skipped");
    }
}
