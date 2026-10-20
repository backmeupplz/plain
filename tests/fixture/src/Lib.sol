// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

// an external library: what uses it is linked against where it's deployed
library Adder {
    function add(uint256 a, uint256 b) external pure returns (uint256) { return a + b; }
}

contract UsesLib {
    function three() external pure returns (uint256) { return Adder.add(1, 2); }
}
