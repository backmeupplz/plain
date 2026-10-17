// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Counter {
    uint256 public number;
    address public owner;
    event Set(address indexed who, uint256 value);
    error NotOwner(address caller);

    constructor() { owner = msg.sender; }

    function set(uint256 n) external {
        if (msg.sender != owner) revert NotOwner(msg.sender);
        number = n;
        emit Set(msg.sender, n);
    }

    function increment() external { number++; }
    function fund() external payable {}
    function boom() external pure { require(false, "boom"); }
}
