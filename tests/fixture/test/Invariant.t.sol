// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";

contract Breakable {
    uint256 public x;
    function poke(uint8 v) external { if (v == 7) x = 1; }
}

contract InvariantTest is Test {
    Breakable b;
    function setUp() public { b = new Breakable(); }
    function invariantBadXStaysZero() public view { assertEq(b.x(), 0, "x broke"); }
}
