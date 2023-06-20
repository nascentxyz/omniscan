# Omniscan

Use pyrometer to scan all of Ethereum mainnet*

Relies on the Zellic smart-contract-fiesta dataset snapshot as the source of smart contract code.

## Installation

You will need a local copy of the smart-contract-fiesta dataset:
- First, make sure to install [git-lfs](https://git-lfs.github.com/) if you haven't already
```bash
git clone https://huggingface.co/datasets/Zellic/smart-contract-fiesta
```

TODO:

1. convert the stdout of the pyrometer process to an exit type

foresee these exit types:
    success
    thread panic
    pyrometer error
    timeout
*this conversion is not trivial, the stdout is messy

honestly this is likely to change, dont spend so much time making it perfect
intuition says thread panic can be regexed, pyrometer error will have info from the first red `Error:` line, need to see if there is identifying info from there
timeout i just need to add in the secs used for timeout. say "Timeout: 10s"

2. fill in the `ResultsRow::convert_to_csv_string()` function


3. view the proportions of exit types as a pie chart, share dataset with dan/brock