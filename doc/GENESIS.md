# Proposal to make some defaults for genesis json file keys so we can clean genesis.json file further and distinguish between legacy clique genesis and new calibri* genesis


- We support 2 types of genesis, clique (legacy) and calibri-lite (current). There's difference what each genesis must contain, and how it must be handled.
- If genesis file miss protocol mention it meams it's calibri, all legacy clique must contain it.

## What should NOT be removed and should remain for all types:

 - "config"
 - "timestamp" // is better to have it fixed/written
 - "extraData"
 - "alloc"
 
## What can be abcent and replaced by defaults in DB on init for calibri*:

 - "nonce" // it's a geneses so nonce is 0x0
 - "difficulty" // is std, 0x1
 - "mixHash" // zero filled
 - "coinbase" // zero filled ??? clique thingy
 - "parentHash" // again, it's a geneses,
 
## Calibri* genesis has no extraData! and as result NO genesis validators! All validators come from validators.jsonl file!
## Old clique legacy genesis validator/miner also must be overrided by validators.jsonl file !!! this is already implemented and it is current behaviour!
## So clique validator/miner also overrided by validators.jsonl, we keep clique as byte-perfect preserved for 100% identical hash for import old legacy data
## So old clique genesis accepted and imported as is, no txs prefunded is added as per legacy rules, but validator will/can be overrided by validators.jsonl
## legacy clique already implemented and works, new calinri* genesis ops not yet implemented
## So, before we just eat calibri* same way as clique, now all what we want to change is to look at calibri* differenly, and apply different init protocol for it.. 
and not touch / keep "normal" legacy current clique init

!!! DO NOT CHANGE CURRENT LEGACY GENESIS OPERATIONS, KEEP IT AS IS, JUST ADJUST CALIBRI* NEW GENESIS OPERATIONS. EXTEND IT WITH PRE_FUND TXS, AND IGNORE VALIDATORS IN GENESIS 
 
## TX records in calibri* genesis:

- clique dooes not reflect pre-funded wallets as transactions, this is current behavior for all. New calibri* must act different and tx records must be implemented.
- calibri* genesis must contain all pre-funded wallets/addresses as a transactions! added to the block0, and written as normal tx on blockchain.
- the tx record from must contain genesis validator address.

Examples:

# Clique genesis - legacy, no txs pre-funded records in block0:

{
  "config": {
    "chainId": 315,
    "homesteadBlock": 0,
    "eip150Block": 0,
    "eip150Hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "eip155Block": 0,
    "eip158Block": 0,
    "byzantiumBlock": 0,
    "constantinopleBlock": 0,
    "petersburgBlock": 0,
    "istanbulBlock": 0,
    "clique": {
      "period": 5,
      "epoch": 30000
    }
  },
  "nonce": "0x0",
  "timestamp": "0x675fd019",
  "extraData": "0x000000000000000000000000000000000000000000000000000000000000000024bf191a358c09e79b9ead2d8999d08590b5af310000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
  "gasLimit": "0x47b760",
  "difficulty": "0x1",
  "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "coinbase": "0x0000000000000000000000000000000000000000",
  "alloc": {
    "0000000000000000000000000000000000000000": {
      "balance": "0x1"
    },
    "24bf191a358c09e79b9ead2d8999d08590b5af31": {
      "balance": "0x200000000000000000000000000000000000000"
    }
  },
  "number": "0x0",
  "gasUsed": "0x0",
  "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "baseFeePerGas": null
}

# Calibri genesis - txs pre-funded records in block0:
- technically genesis only used to 1 - set chainID, timestamp of genesis, protocol consensus name, and pre-fund some wallets, nothing else.. gasLimit remain as legacy, may be needed..
- V3 also add/changes home origin to MARS, confirm no city gate policy

# V1:

{
  "config": {
    "chainId": 315,
    "homesteadBlock": 0,
    "byzantiumBlock": 0,
    "calibri-lite": {
      "quorum": 1,
      "period": 1
    }
  },
  "alloc": {
    "868982Fb4c603e437C521C19cE20b39BE9c77DBC": {
      "balance": "0x20000000000000000000"
    }, 
   "55155936E45eAA889eC5acFd0e895622e803afcB": {
      "balance": "0x20000000000000000000"
    }
  },
  "timestamp": "0x675fd019",
  "extraData": "0x0",
  "gasLimit": "0x47b760",
  "gasUsed": "0x0",
  "baseFeePerGas": null
}

# V2:

{
  "config": {
    "chainId": 315,
    "homesteadBlock": 0,
    "byzantiumBlock": 0,
    "timestamp": "0x675fd019",
    "gasLimit": "0x47b760",
    "gasUsed": "0x0",
    "baseFeePerGas": null
  },
  "consensus": {
    "calibri-lite": {
      "quorum": 1,
      "period": 1
    },
  },
  "alloc": {
    "868982Fb4c603e437C521C19cE20b39BE9c77DBC": {
      "balance": "0x20000000000000000000"
    }, 
   "55155936E45eAA889eC5acFd0e895622e803afcB": {
      "balance": "0x20000000000000000000"
    }
  },
}

# V3:

{
  "config": {
    "chainId": 315,
    "home": "MARS",
    "consensus": "calibri-lite",
    "timestamp": "0x675fd019",
    "gasLimit": "0x47b760",
  },
  #  optional consesnsus details, but can be overrided in runtime, so commented out, no need to add to genesis V3.
  #  "calibri-lite": {
  #      "quorum": 1,
  #      "period": 1
  #  },
  "alloc": {
    "868982Fb4c603e437C521C19cE20b39BE9c77DBC": {
      "balance": "0x20000000000000000000"
    }, 
   "55155936E45eAA889eC5acFd0e895622e803afcB": {
      "balance": "0x20000000000000000000"
    }
  },
}
