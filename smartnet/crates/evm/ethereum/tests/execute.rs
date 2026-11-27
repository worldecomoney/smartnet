//! Execution tests.

use alloy_consensus::{constants::ETH_TO_WEI, Header, TxLegacy};
use alloy_eips::{
    eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE, SYSTEM_ADDRESS},
    eip4895::Withdrawal,
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_PREDEPLOY_CODE},
    eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_evm::{
    block::BlockValidationError,
    eth::{disable_gate_rewards, gate_opens_for},
};
use alloy_primitives::{b256, fixed_bytes, keccak256, Bytes, TxKind, B256, U256};
use smartnet_ethereum_primitives::{Block, BlockBody, Transaction};
use smartnet_evm::{
    execute::{BasicBlockExecutor, Executor},
    ConfigureEvm,
};
use smartnet_primitives_traits::{
    crypto::secp256k1::public_key_to_address, Block as _, RecoveredBlock,
};
use smartnet_testing_utils::generators::{self, sign_tx_with_key_pair};
use revm::{
    database::{CacheDB, EmptyDB, TransitionState},
    primitives::address,
    state::{AccountInfo, Bytecode, EvmState},
    Database,
};
use smartnet_chainspec::{ChainSpecBuilder, EthereumHardfork, ForkCondition, MAINNET};
use smartnet_evm_ethereum::EthEvmConfig;
use std::sync::{mpsc, Arc};

fn create_database_with_beacon_root_contract() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(Default::default());

    let beacon_root_contract_account = AccountInfo {
        balance: U256::ZERO,
        code_hash: keccak256(BEACON_ROOTS_CODE.clone()),
        nonce: 1,
        code: Some(Bytecode::new_raw(BEACON_ROOTS_CODE.clone())),
        account_id: None,
    };

    db.insert_account_info(BEACON_ROOTS_ADDRESS, beacon_root_contract_account);

    db
}

fn create_database_with_withdrawal_requests_contract() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(Default::default());

    let withdrawal_requests_contract_account = AccountInfo {
        nonce: 1,
        balance: U256::ZERO,
        code_hash: keccak256(WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone()),
        code: Some(Bytecode::new_raw(WITHDRAWAL_REQUEST_PREDEPLOY_CODE.clone())),
        account_id: None,
    };

    db.insert_account_info(
        WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
        withdrawal_requests_contract_account,
    );

    db
}

// [catalog: DROP — Cancun EIP-4788 beacon-root feature; WEM is LONDON-only]
// Executes an empty block carrying a non-zero parent beacon block root under Cancun when NO
// code is deployed at BEACON_ROOTS_ADDRESS, asserting the pre-block call does not panic.
#[test]
fn eip_4788_no_code_cancun() {
    // This test ensures that we "silently fail" when cancun is active and there is no code at
    // // BEACON_ROOTS_ADDRESS
    let header = Header {
        timestamp: 1,
        number: 1,
        parent_beacon_block_root: Some(B256::with_last_byte(0x69)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let db = CacheDB::new(EmptyDB::default());

    // DON'T deploy the contract at genesis
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .london_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let provider = EthEvmConfig::new(chain_spec);

    // attempt to execute an empty block with parent beacon block root, this should not fail
    provider
        .batch_executor(db)
        .execute_one(&RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
            },
            vec![],
        ))
        .expect("Executing a block with no transactions while cancun is active should not fail");
}

// [catalog: DROP — Cancun EIP-4788; WEM is LONDON-only]
// Deploys the beacon-root contract, invokes it from an empty SYSTEM_ADDRESS, and asserts the
// system account nonce stays 0 (no spurious nonce increment on the pre-block call).
#[test]
fn eip_4788_empty_account_call() {
    // This test ensures that we do not increment the nonce of an empty SYSTEM_ADDRESS account
    // // during the pre-block call

    let mut db = create_database_with_beacon_root_contract();

    // insert an empty SYSTEM_ADDRESS
    db.insert_account_info(SYSTEM_ADDRESS, Default::default());

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .london_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(1))
            .build(),
    );

    let provider = EthEvmConfig::new(chain_spec);

    // construct the header for block one
    let header = Header {
        timestamp: 1,
        number: 1,
        parent_beacon_block_root: Some(B256::with_last_byte(0x69)),
        excess_blob_gas: Some(0),
        ..Header::default()
    };

    let mut executor = BasicBlockExecutor::new(provider, db);

    // attempt to execute an empty block with parent beacon block root, this should not fail
    executor
        .execute_one(&RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
            },
            vec![],
        ))
        .expect("Executing a block with no transactions while cancun is active should not fail");

    // ensure that the nonce of the system address account has not changed
    let nonce =
        executor.with_state_mut(|state| state.basic(SYSTEM_ADDRESS).unwrap().unwrap().nonce);
    assert_eq!(nonce, 0);
}

// [catalog: DROP — Cancun EIP-4788; WEM is LONDON-only]
// Genesis cancun block with a non-zero parent beacon block root must error; with a zero PBBR
// it must run and make NO storage changes (transition state stays default — no system call).
#[test]
fn eip_4788_genesis_call() {
    let db = create_database_with_beacon_root_contract();

    // activate cancun at genesis
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .london_activated()
            .with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(0))
            .build(),
    );

    let mut header = chain_spec.genesis_header().clone();
    let provider = EthEvmConfig::new(chain_spec);
    let mut executor = BasicBlockExecutor::new(provider, db);

    // attempt to execute the genesis block with non-zero parent beacon block root, expect err
    header.parent_beacon_block_root = Some(B256::with_last_byte(0x69));
    let _err = executor
        .execute_one(&RecoveredBlock::new_unhashed(
            Block { header: header.clone(), body: Default::default() },
            vec![],
        ))
        .expect_err(
            "Executing genesis cancun block with non-zero parent beacon block root field
    should fail",
        );

    // fix header
    header.parent_beacon_block_root = Some(B256::ZERO);

    // now try to process the genesis block again, this time ensuring that a system contract
    // call does not occur
    executor
        .execute_one(&RecoveredBlock::new_unhashed(
            Block { header, body: Default::default() },
            vec![],
        ))
        .unwrap();

    // there is no system contract call so there should be NO STORAGE CHANGES
    // this means we'll check the transition state
    let transition_state = executor.with_state_mut(|state| {
        state.transition_state.take().expect("the evm should be initialized with bundle updates")
    });

    // assert that it is the default (empty) transition state
    assert_eq!(transition_state, TransitionState::default());
}

/// Create a state provider with blockhashes and the EIP-2935 system contract.
fn create_database_with_block_hashes(latest_block: u64) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(Default::default());
    for block_number in 0..=latest_block {
        db.cache.block_hashes.insert(U256::from(block_number), keccak256(block_number.to_string()));
    }

    let blockhashes_contract_account = AccountInfo {
        balance: U256::ZERO,
        code_hash: keccak256(HISTORY_STORAGE_CODE.clone()),
        code: Some(Bytecode::new_raw(HISTORY_STORAGE_CODE.clone())),
        nonce: 1,
        account_id: None,
    };

    db.insert_account_info(HISTORY_STORAGE_ADDRESS, blockhashes_contract_account);

    db
}
// [catalog: DROP — EIP-2935 block-hash history is a Prague feature; WEM is LONDON-only]
// Under Prague set to Never, asserts a block hash is NOT written to the EIP-2935 history
// storage when the fork is not active.
#[test]
fn eip_2935_pre_fork() {
    let db = create_database_with_block_hashes(1);

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .london_activated()
            .with_fork(EthereumHardfork::Prague, ForkCondition::Never)
            .build(),
    );

    let provider = EthEvmConfig::new(chain_spec);
    let mut executor = BasicBlockExecutor::new(provider, db);

    // construct the header for block one
    let header = Header { timestamp: 1, number: 1, ..Header::default() };

    // attempt to execute an empty block, this should not fail
    executor
        .execute_one(&RecoveredBlock::new_unhashed(
            Block { header, body: Default::default() },
            vec![],
        ))
        .expect("Executing a block with no transactions while Prague is active should not fail");

    // ensure that the block hash was *not* written to storage, since this is before the fork
    // was activated
    //
    // we load the account first, because revm expects it to be
    // loaded
    executor.with_state_mut(|state| state.basic(HISTORY_STORAGE_ADDRESS).unwrap());
    assert!(executor.with_state_mut(|state| {
        state.storage(HISTORY_STORAGE_ADDRESS, U256::ZERO).unwrap().is_zero()
    }));
}

// [catalog: DROP — EIP-2935 Prague feature; WEM is LONDON-only]
// Genesis block under Prague: no ancestor hash exists yet, so nothing is written to history
// storage.
#[test]
fn eip_2935_fork_activation_genesis() {
    let db = create_database_with_block_hashes(0);

    let chain_spec = Arc::new(ChainSpecBuilder::from(&*MAINNET).london_activated().build());

    let header = chain_spec.genesis_header().clone();
    let provider = EthEvmConfig::new(chain_spec);
    let mut executor = BasicBlockExecutor::new(provider, db);

    // attempt to execute genesis block, this should not fail
    executor
        .execute_one(&RecoveredBlock::new_unhashed(
            Block { header, body: Default::default() },
            vec![],
        ))
        .expect("Executing a block with no transactions while Prague is active should not fail");

    // ensure that the block hash was *not* written to storage, since there are no blocks
    // preceding genesis
    //
    // we load the account first, because revm expects it to be
    // loaded
    executor.with_state_mut(|state| state.basic(HISTORY_STORAGE_ADDRESS).unwrap());
    assert!(executor.with_state_mut(|state| {
        state.storage(HISTORY_STORAGE_ADDRESS, U256::ZERO).unwrap().is_zero()
    }));
}

// <https://github.com/ethereum/EIPs/pull/9144>
// [catalog: DROP — EIP-2935 Prague feature; WEM is LONDON-only]
// Fork activation block beyond HISTORY_SERVE_WINDOW+256: the ancestor block hash is still
// within range and must be present in history storage.
#[test]
fn eip_2935_fork_activation_outside_window_bounds() {
    let fork_activation_block = (HISTORY_SERVE_WINDOW + 256) as u64;
    let db = create_database_with_block_hashes(fork_activation_block);

    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .london_activated()
            .with_fork(EthereumHardfork::Prague, ForkCondition::Timestamp(1))
            .build(),
    );

    let provider = EthEvmConfig::new(chain_spec);
    let mut executor = BasicBlockExecutor::new(provider, db);

    let header = Header {
        parent_hash: B256::random(),
        timestamp: 1,
        number: fork_activation_block,
        requests_hash: Some(EMPTY_REQUESTS_HASH),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::random()),
        ..Header::default()
    };

    // attempt to execute the fork activation block, this should not fail
    executor
        .execute_one(&RecoveredBlock::new_unhashed(
            Block { header, body: Default::default() },
            vec![],
        ))
        .expect("Executing a block with no transactions while Prague is active should not fail");

    // the hash for the ancestor of the fork activation block should be present
    assert!(
        executor.with_state_mut(|state| state.basic(HISTORY_STORAGE_ADDRESS).unwrap().is_some())
    );
}

// [catalog: KEEP — generic tx-gas validation, chain-id-agnostic; rewrite chain id -> 315]
// A tx whose gas limit (2.5M) exceeds the block gas limit (1.5M) must fail with
// TransactionGasLimitMoreThanAvailableBlockGas.
#[test]
fn block_gas_limit_error() {
    // Create a chain specification with fork conditions set for Prague
    let chain_spec = Arc::new(
        ChainSpecBuilder::from(&*MAINNET)
            .london_activated()
            .with_fork(EthereumHardfork::Prague, ForkCondition::Timestamp(0))
            .build(),
    );

    // Create a state provider with the withdrawal requests contract pre-deployed
    let mut db = create_database_with_withdrawal_requests_contract();

    // Generate a new key pair for the sender
    let sender_key_pair = generators::generate_key(&mut generators::rng());
    // Get the sender's address from the public key
    let sender_address = public_key_to_address(*sender_key_pair.verifying_key());

    // Insert the sender account into the state with a nonce of 1 and a balance of 1 ETH in Wei
    db.insert_account_info(
        sender_address,
        AccountInfo { nonce: 1, balance: U256::from(ETH_TO_WEI), ..Default::default() },
    );

    // Define the validator public key and withdrawal amount as fixed bytes
    let validator_public_key = fixed_bytes!(
            "111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111"
        );
    let withdrawal_amount = fixed_bytes!("2222222222222222");
    // Concatenate the validator public key and withdrawal amount into a single byte array
    let input: Bytes = [&validator_public_key[..], &withdrawal_amount[..]].concat().into();
    // Ensure the input length is 56 bytes
    assert_eq!(input.len(), 56);

    // Create a genesis block header with a specified gas limit and gas used
    let mut header = chain_spec.genesis_header().clone();
    header.gas_limit = 1_500_000;
    header.gas_used = 134_807;
    header.receipts_root =
        b256!("0xb31a3e47b902e9211c4d349af4e4c5604ce388471e79ca008907ae4616bb0ed3");

    // Create a transaction with a gas limit higher than the block gas limit
    let tx = sign_tx_with_key_pair(
        sender_key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 1,
            gas_price: header.base_fee_per_gas.unwrap().into(),
            gas_limit: 2_500_000, // higher than block gas limit
            to: TxKind::Call(WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS),
            value: U256::from(1),
            input,
        }),
    );

    // Create an executor from the state provider
    let evm_config = EthEvmConfig::new(chain_spec);
    let mut executor = evm_config.batch_executor(db);

    // Execute the block and capture the result
    let exec_result = executor.execute_one(
        &Block { header, body: BlockBody { transactions: vec![tx], ..Default::default() } }
            .try_into_recovered()
            .unwrap(),
    );

    // Check if the execution result is an error and assert the specific error type
    match exec_result {
        Ok(_) => panic!("Expected block gas limit error"),
        Err(err) => assert!(matches!(
            *err.as_validation().unwrap(),
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: 2_500_000,
                block_available_gas: 1_500_000,
            }
        )),
    }
}

// [catalog: REVIEW — the balance-not-duplicated assertion is generic, but it is wired through
// a withdrawal (Shanghai/Cancun feature). Keep only as a generic "credit an account on block
// exec" test; otherwise DROP.]
#[test]
fn test_balance_increment_not_duplicated() {
    let chain_spec = Arc::new(ChainSpecBuilder::from(&*MAINNET).london_activated().build());

    let withdrawal_recipient = address!("0x1000000000000000000000000000000000000000");

    let mut db = CacheDB::new(EmptyDB::default());
    let initial_balance = 100;
    db.insert_account_info(
        withdrawal_recipient,
        AccountInfo { balance: U256::from(initial_balance), nonce: 1, ..Default::default() },
    );

    let withdrawal =
        Withdrawal { index: 0, validator_index: 0, address: withdrawal_recipient, amount: 1 };

    let header = Header {
        timestamp: 1,
        number: 1,
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::random()),
        ..Header::default()
    };

    let block = &RecoveredBlock::new_unhashed(
        Block {
            header,
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(vec![withdrawal].into()),
            },
        },
        vec![],
    );

    let provider = EthEvmConfig::new(chain_spec);
    let executor = provider.batch_executor(db);

    let (tx, rx) = mpsc::channel();
    let tx_clone = tx.clone();

    let _output = executor
        .execute_with_state_hook(block, move |_, state: &EvmState| {
            if let Some(account) = state.get(&withdrawal_recipient) {
                let _ = tx_clone.send(account.info.balance);
            }
        })
        .expect("Block execution should succeed");

    drop(tx);
    let balance_changes: Vec<U256> = rx.try_iter().collect();

    if let Some(final_balance) = balance_changes.last() {
        let expected_final_balance = U256::from(initial_balance) + U256::from(1_000_000_000); // initial + 1 Gwei in Wei
        assert_eq!(
            *final_balance, expected_final_balance,
            "Final balance should match expected value after withdrawal"
        );
    }
}

// [catalog: KEEP — historical RLP import accommodation: legacy geth history carries txs
// that fail today's value + gas_limit*fee_cap check. Import mode skips ONLY that check;
// every other validity rule (nonce, base fee, tip) stays on. Sync/default mode keeps the
// ordinary balance check.]
#[test]
fn import_mode_admits_the_legacy_underfunded_tx() {
    let chain_spec = Arc::new(ChainSpecBuilder::from(&*MAINNET).london_activated().build());

    let sealer_key_pair = generators::generate_key(&mut generators::rng());
    let sealer_address = public_key_to_address(*sealer_key_pair.verifying_key());
    let sender_key_pair = generators::generate_key(&mut generators::rng());
    let bridge_address = address!("0x00000000000000000000000000000000000000bb");

    let db = CacheDB::new(EmptyDB::default());

    let mut header = chain_spec.genesis_header().clone();
    header.gas_limit = 1_500_000;
    header.beneficiary = sealer_address;

    let value = U256::from(ETH_TO_WEI);
    let tx = sign_tx_with_key_pair(
        sender_key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: header.base_fee_per_gas.unwrap().into(),
            gas_limit: 30_000,
            to: TxKind::Call(bridge_address),
            value,
            input: Bytes::new(),
        }),
    );

    let provider = EthEvmConfig::new(chain_spec).for_import();
    let executor = provider.batch_executor(db);

    let (state_tx, state_rx) = mpsc::channel();
    executor
        .execute_with_state_hook(
            &Block { header, body: BlockBody { transactions: vec![tx], ..Default::default() } }
                .try_into_recovered()
                .unwrap(),
            move |_, state: &EvmState| {
                if let Some(account) = state.get(&bridge_address) {
                    let _ = state_tx.send(account.info.balance);
                }
            },
        )
        .expect("import mode must replay the legacy underfunded tx");

    let balances: Vec<U256> = state_rx.try_iter().collect();
    assert_eq!(
        *balances.last().expect("the recipient must appear in the state diff"),
        value,
        "import mode credits the full value of the legacy tx"
    );
}

// [catalog: KEEP — disabled-boundary proof, NOT a gate test: a beneficiary-signed,
// mandate-shaped overdraft from an empty wallet is refused during normal block processing.
// No privileged fallback execution when the GATE opt-out is in force.]
#[test]
fn gate_cannot_issue_during_block_processing() {
    assert!(disable_gate_rewards(), "GATE must be disabled unconditionally");

    let chain_spec = Arc::new(ChainSpecBuilder::from(&*MAINNET).london_activated().build());
    let chair_key_pair = generators::generate_key(&mut generators::rng());
    let chair_address = public_key_to_address(*chair_key_pair.verifying_key());
    let bridge_address = address!("0x00000000000000000000000000000000000000bb");

    let mut header = chain_spec.genesis_header().clone();
    header.gas_limit = 1_500_000;
    header.beneficiary = chair_address; // caller == beneficiary, but the gate stays shut

    assert!(!gate_opens_for(chair_address, chair_address));
    assert!(!gate_opens_for(chair_address, bridge_address));

    let tx = sign_tx_with_key_pair(
        chair_key_pair,
        Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_spec.chain.id()),
            nonce: 0,
            gas_price: header.base_fee_per_gas.unwrap().into(),
            gas_limit: 30_000,
            to: TxKind::Call(bridge_address),
            value: U256::from(ETH_TO_WEI),
            input: Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF]),
        }),
    );

    let provider = EthEvmConfig::new(chain_spec);
    let mut executor = provider.batch_executor(CacheDB::new(EmptyDB::default()));
    let err = executor
        .execute_one(
            &Block { header, body: BlockBody { transactions: vec![tx], ..Default::default() } }
                .try_into_recovered()
                .unwrap(),
        )
        .expect_err("a beneficiary overdraft must never issue when GATE is disabled");

    assert!(
        format!("{err}").contains("lack of funds"),
        "the ordinary balance check must stand, got: {err}"
    );
}

// [catalog: KEEP — disabled-boundary proof for historical replay: the import flag is a
// separate legacy-history accommodation. It never turns on GATE, and a live/replay config
// without that flag refuses the exact same beneficiary issuance.]
#[test]
fn historical_replay_never_activates_gate() {
    assert!(disable_gate_rewards(), "GATE must be disabled on every execution path");

    let chain_spec = Arc::new(ChainSpecBuilder::from(&*MAINNET).london_activated().build());
    let chair_key_pair = generators::generate_key(&mut generators::rng());
    let chair_address = public_key_to_address(*chair_key_pair.verifying_key());
    let bridge_address = address!("0x00000000000000000000000000000000000000bb");

    let mut header = chain_spec.genesis_header().clone();
    header.gas_limit = 1_500_000;
    header.beneficiary = chair_address;

    // Import/replay may carry its own bypass; the gate adds nothing to it, so no
    // execution path can turn a beneficiary tx into a privileged issuance.
    for incoming_flag in [false, true] {
        assert_eq!(incoming_flag || gate_opens_for(chair_address, chair_address), incoming_flag);
    }

    let build_tx = |key_pair| {
        sign_tx_with_key_pair(
            key_pair,
            Transaction::Legacy(TxLegacy {
                chain_id: Some(chain_spec.chain.id()),
                nonce: 0,
                gas_price: header.base_fee_per_gas.unwrap().into(),
                gas_limit: 30_000,
                to: TxKind::Call(bridge_address),
                value: U256::from(ETH_TO_WEI),
                input: Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF]),
            }),
        )
    };
    let live_tx = build_tx(chair_key_pair.clone());
    let replay_tx = build_tx(chair_key_pair);

    // Live/sync (no import flag): refused even though caller == beneficiary.
    let err = EthEvmConfig::new(chain_spec.clone())
        .batch_executor(CacheDB::new(EmptyDB::default()))
        .execute_one(
            &Block {
                header: header.clone(),
                body: BlockBody { transactions: vec![live_tx], ..Default::default() },
            }
            .try_into_recovered()
            .unwrap(),
        )
        .expect_err("replay/sync execution must not issue when GATE is disabled");
    assert!(
        format!("{err}").contains("lack of funds"),
        "disabled GATE cannot be activated by replay execution, got: {err}"
    );

    // Import mode still replays legacy history through its own flag only; GATE stays off.
    EthEvmConfig::new(chain_spec)
        .for_import()
        .batch_executor(CacheDB::new(EmptyDB::default()))
        .execute_one(
            &Block {
                header,
                body: BlockBody { transactions: vec![replay_tx], ..Default::default() },
            }
            .try_into_recovered()
            .unwrap(),
        )
        .expect("import replay keeps its historical bypass, not a GATE issuance");
    assert!(disable_gate_rewards(), "import mode must not change the GATE opt-out");
}
