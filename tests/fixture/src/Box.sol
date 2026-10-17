// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Box {
    string public name;
    bytes32 public tag;
    uint256 public value;
    address public owner;

    constructor(string memory n, bytes32 t, uint256 v, address o) payable {
        require(v != 0, "no zero");
        (name, tag, value, owner) = (n, t, v, o);
    }
}
