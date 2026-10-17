// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {Counter} from "../src/Counter.sol";
import {Box} from "../src/Box.sol";

// plain deploys these `new` expressions from compiled artifacts (see src/rewrite.rs); behavior must not change
contract RewriteTest is Test {
    function test_prankedCreate() public {
        address alice = makeAddr("alice");
        vm.prank(alice);
        Counter c = new Counter();
        assertEq(c.owner(), alice);
        assertEq(address(c), vm.computeCreateAddress(alice, 0));
    }

    function test_create2MatchesCreationCode() public {
        Counter c = new Counter{salt: bytes32(uint256(7))}();
        assertEq(address(c), vm.computeCreate2Address(bytes32(uint256(7)), keccak256(type(Counter).creationCode), address(this)));
    }

    function test_constructorArgsAndValue() public {
        Box b = new Box{value: 3}("hi", "tag", 7, address(this));
        assertEq(b.name(), "hi");
        assertEq(b.tag(), bytes32("tag"));
        assertEq(b.value(), 7);
        assertEq(address(b).balance, 3);
    }

    function test_revertingConstructor() public {
        vm.expectRevert(bytes("no zero"));
        new Box("x", "", 0, address(this));
    }
}
