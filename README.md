# Omniscan

Use pyrometer to scan all of Ethereum mainnet*

Relies on the Zellic smart-contract-fiesta dataset snapshot as the source of smart contract code.

## Installation

You will need a local copy of the smart-contract-fiesta dataset:
- First, make sure to install [git-lfs](https://git-lfs.github.com/) if you haven't already
```bash
git clone https://huggingface.co/datasets/Zellic/smart-contract-fiesta
cd smart-contract-fiesta
git lfs install
git lfs fetch
git lfs checkout
```

TODO:
first grab all unique bytecode hashes within smart-contract-fiesta
set a pool of workers 
pop a bytecode hash into the pool
workers take a bytecode hash and find the source code
- preprocess filtering with sol >0.8
workers build a pyro obj with the source code and remappings



use pyrometer as lib
pyrometer remappings need to be inlined rather than cli
