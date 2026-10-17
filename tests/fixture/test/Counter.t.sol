// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Counter} from "../src/Counter.sol";

interface Vm {
    function prank(address) external;
    function deal(address, uint256) external;
    function warp(uint256) external;
    function expectRevert(bytes calldata) external;
    function expectRevert(bytes4) external;
    function expectEmit(bool, bool, bool, bool) external;
    function assume(bool) external;
    function addr(uint256) external returns (address);
    function sign(uint256, bytes32) external returns (uint8, bytes32, bytes32);
    function store(address, bytes32, bytes32) external;
    function load(address, bytes32) external view returns (bytes32);
    function snapshotState() external returns (uint256);
    function revertToState(uint256) external returns (bool);
}

library console {
    function log(string memory s, uint256 v) internal view {
        bytes memory p = abi.encodeWithSignature("log(string,uint256)", s, v);
        assembly { pop(staticcall(gas(), 0x000000000000000000636F6e736F6c652e6c6f67, add(p, 32), mload(p), 0, 0)) }
    }
}

contract CounterTest {
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    Counter c;
    event Set(address indexed who, uint256 value);

    function setUp() public { c = new Counter(); }

    // ---- should pass ----
    function testSet() public { c.set(5); require(c.number() == 5); }
    function testPrankRevert() public {
        vm.prank(address(0xBEEF));
        vm.expectRevert(abi.encodeWithSelector(Counter.NotOwner.selector, address(0xBEEF)));
        c.set(1);
    }
    function testSelectorRevert() public { vm.prank(address(1)); vm.expectRevert(Counter.NotOwner.selector); c.set(1); }
    function testStringRevert() public { vm.expectRevert(bytes("boom")); c.boom(); }
    function testEmit() public { vm.expectEmit(true, false, false, true); emit Set(address(this), 7); c.set(7); }
    function testDealWarp() public {
        vm.deal(address(0xA), 3 ether); vm.warp(1000);
        require(address(0xA).balance == 3 ether && block.timestamp == 1000);
        vm.prank(address(0xA)); c.fund{value: 1 ether}();
        require(address(c).balance == 1 ether);
    }
    function testSign() public {
        address a = vm.addr(1); bytes32 h = keccak256("x");
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(1, h);
        require(ecrecover(h, v, r, s) == a, "sig");
    }
    function testStoreLoadSnapshot() public {
        vm.store(address(c), bytes32(0), bytes32(uint256(42)));
        require(c.number() == 42);
        uint256 id = vm.snapshotState();
        c.set(1);
        vm.revertToState(id);
        require(c.number() == 42 && uint256(vm.load(address(c), 0)) == 42);
    }
    function testFuzzSet(uint256 x) public { vm.assume(x != 3); c.set(x); require(c.number() == x); }
    function testIsolation() public view { require(c.number() == 0, "state leaked between tests"); }

    // ---- should fail ----
    function testBadRequire() public view { console.log("value is %s", 99); require(1 == 2, "one is not two"); }
    function testBadNoRevert() public { vm.expectRevert(bytes("boom")); c.increment(); }
    function testBadWrongRevert() public { vm.expectRevert(bytes("nope")); c.boom(); }
    function testBadEmit() public { vm.expectEmit(true, false, false, true); emit Set(address(this), 8); c.set(7); }
    function testBadFuzz(uint8 x) public pure { require(x < 200, "too big"); }
    function testBadPanic() public pure { uint256 a = 0; a - 1; }
}

contract LegacyTest {
    function testFailLegacyReverts() public pure { revert("expected"); }
    function testFailLegacyPassesIsBad() public pure {}
}
