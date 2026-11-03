#![allow(missing_docs)]

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::{address, Address, Bytes, B256};
use alloy_rlp::EMPTY_STRING_CODE;
use smartnet_trie::{proof::Proof, StorageProof};
use smartnet_trie_db::{DatabaseHashedCursorFactory, DatabaseProof, DatabaseTrieCursorFactory};
use smartnet_chainspec::{Chain, ChainSpec, MAINNET};
use smartnet_provider::test_utils::{create_test_provider_factory, insert_genesis};
use std::{
    str::FromStr,
    sync::{Arc, LazyLock},
};

type DbProof<'a, TX, A> =
    Proof<DatabaseTrieCursorFactory<&'a TX, A>, DatabaseHashedCursorFactory<&'a TX>>;

/*
    World State (sampled from <https://ethereum.stackexchange.com/questions/268/ethereum-block-architecture/6413#6413>)
    | address                                    | prefix    | hash                                                               | balance
    |--------------------------------------------|-----------|--------------------------------------------------------------------|--------
    | 0x2031f89b3ea8014eb51a78c316e42af3e0d7695f | 0xa711355 | 0xa711355ec1c8f7e26bb3ccbcb0b75d870d15846c0b98e5cc452db46c37faea40 |  45 eth
    | 0x33f0fc440b8477fcfbe9d0bf8649e7dea9baedb2 | 0xa77d337 | 0xa77d337781e762f3577784bab7491fcc43e291ce5a356b9bc517ac52eed3a37a |   1 wei
    | 0x62b0dd4aab2b1a0a04e279e2b828791a10755528 | 0xa7f9365 | 0xa7f936599f93b769acf90c7178fd2ddcac1b5b4bc9949ee5a04b7e0823c2446e | 1.1 eth
    | 0x1ed9b1dd266b607ee278726d324b855a093394a6 | 0xa77d397 | 0xa77d397a32b8ab5eb4b043c65b1f00c93f517bc8883c5cd31baf8e8a279475e3 | .12 eth

    All expected testspec results were obtained from querying proof RPC on the running geth instance `geth init crates/trie/testdata/proof-genesis.json && geth --http`.
*/
static TEST_SPEC: LazyLock<Arc<ChainSpec>> = LazyLock::new(|| {
    ChainSpec {
        chain: Chain::from_id(12345),
        genesis: serde_json::from_str(include_str!("../../trie/testdata/proof-genesis.json"))
            .expect("Can't deserialize test genesis json"),
        ..Default::default()
    }
    .into()
});

fn convert_to_proof<'a>(path: impl IntoIterator<Item = &'a str>) -> Vec<Bytes> {
    path.into_iter().map(Bytes::from_str).collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn testspec_proofs() {
    // Create test database and insert genesis accounts.
    let factory = create_test_provider_factory();
    let root = insert_genesis(&factory, TEST_SPEC.clone()).unwrap();

    let data = Vec::from([
        (
            "0x2031f89b3ea8014eb51a78c316e42af3e0d7695f",
            convert_to_proof([
                "0xe48200a7a040f916999be583c572cc4dd369ec53b0a99f7de95f13880cf203d98f935ed1b3",
                "0xf87180a04fb9bab4bb88c062f32452b7c94c8f64d07b5851d44a39f1e32ba4b1829fdbfb8080808080a0b61eeb2eb82808b73c4ad14140a2836689f4ab8445d69dd40554eaf1fce34bc080808080808080a0dea230ff2026e65de419288183a340125b04b8405cc61627b3b4137e2260a1e880",
                "0xf8719f31355ec1c8f7e26bb3ccbcb0b75d870d15846c0b98e5cc452db46c37faea40b84ff84d80890270801d946c940000a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            ]),
        ),
        (
            "0x33f0fc440b8477fcfbe9d0bf8649e7dea9baedb2",
            convert_to_proof([
                "0xe48200a7a040f916999be583c572cc4dd369ec53b0a99f7de95f13880cf203d98f935ed1b3",
                "0xf87180a04fb9bab4bb88c062f32452b7c94c8f64d07b5851d44a39f1e32ba4b1829fdbfb8080808080a0b61eeb2eb82808b73c4ad14140a2836689f4ab8445d69dd40554eaf1fce34bc080808080808080a0dea230ff2026e65de419288183a340125b04b8405cc61627b3b4137e2260a1e880",
                "0xe48200d3a0ef957210bca5b9b402d614eb8408c88cfbf4913eb6ab83ca233c8b8f0e626b54",
                "0xf851808080a02743a5addaf4cf9b8c0c073e1eaa555deaaf8c41cb2b41958e88624fa45c2d908080808080a0bfbf6937911dfb88113fecdaa6bde822e4e99dae62489fcf61a91cb2f36793d680808080808080",
                "0xf8679e207781e762f3577784bab7491fcc43e291ce5a356b9bc517ac52eed3a37ab846f8448001a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            ]),
        ),
        (
            "0x62b0dd4aab2b1a0a04e279e2b828791a10755528",
            convert_to_proof([
                "0xe48200a7a040f916999be583c572cc4dd369ec53b0a99f7de95f13880cf203d98f935ed1b3",
                "0xf87180a04fb9bab4bb88c062f32452b7c94c8f64d07b5851d44a39f1e32ba4b1829fdbfb8080808080a0b61eeb2eb82808b73c4ad14140a2836689f4ab8445d69dd40554eaf1fce34bc080808080808080a0dea230ff2026e65de419288183a340125b04b8405cc61627b3b4137e2260a1e880",
                "0xf8709f3936599f93b769acf90c7178fd2ddcac1b5b4bc9949ee5a04b7e0823c2446eb84ef84c80880f43fc2c04ee0000a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            ]),
        ),
        (
            "0x1ed9b1dd266b607ee278726d324b855a093394a6",
            convert_to_proof([
                "0xe48200a7a040f916999be583c572cc4dd369ec53b0a99f7de95f13880cf203d98f935ed1b3",
                "0xf87180a04fb9bab4bb88c062f32452b7c94c8f64d07b5851d44a39f1e32ba4b1829fdbfb8080808080a0b61eeb2eb82808b73c4ad14140a2836689f4ab8445d69dd40554eaf1fce34bc080808080808080a0dea230ff2026e65de419288183a340125b04b8405cc61627b3b4137e2260a1e880",
                "0xe48200d3a0ef957210bca5b9b402d614eb8408c88cfbf4913eb6ab83ca233c8b8f0e626b54",
                "0xf851808080a02743a5addaf4cf9b8c0c073e1eaa555deaaf8c41cb2b41958e88624fa45c2d908080808080a0bfbf6937911dfb88113fecdaa6bde822e4e99dae62489fcf61a91cb2f36793d680808080808080",
                "0xf86f9e207a32b8ab5eb4b043c65b1f00c93f517bc8883c5cd31baf8e8a279475e3b84ef84c808801aa535d3d0c0000a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            ]),
        ),
    ]);

    let provider = factory.provider().unwrap();
    smartnet_trie_db::with_adapter!(provider, |A| {
        for (target, expected_proof) in data {
            let target = Address::from_str(target).unwrap();
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(provider.tx_ref());
            let account_proof = proof.account_proof(target, &[]).unwrap();
            similar_asserts::assert_eq!(
                account_proof.proof,
                expected_proof,
                "proof for {target:?} does not match"
            );
            assert_eq!(account_proof.verify(root), Ok(()));
        }
    });
}

#[test]
fn testspec_empty_storage_proof() {
    // Create test database and insert genesis accounts.
    let factory = create_test_provider_factory();
    let root = insert_genesis(&factory, TEST_SPEC.clone()).unwrap();

    let target = address!("0x1ed9b1dd266b607ee278726d324b855a093394a6");
    let slots = Vec::from([B256::with_last_byte(1), B256::with_last_byte(3)]);

    let provider = factory.provider().unwrap();
    smartnet_trie_db::with_adapter!(provider, |A| {
        let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(provider.tx_ref());
        let account_proof = proof.account_proof(target, &slots).unwrap();
        assert_eq!(account_proof.storage_root, EMPTY_ROOT_HASH, "expected empty storage root");

        assert_eq!(slots.len(), account_proof.storage_proofs.len());
        for (idx, slot) in slots.into_iter().enumerate() {
            let proof = account_proof.storage_proofs.get(idx).unwrap();
            assert_eq!(
                proof,
                &StorageProof::new(slot).with_proof(vec![Bytes::from([EMPTY_STRING_CODE])])
            );
            assert_eq!(proof.verify(account_proof.storage_root), Ok(()));
        }
        assert_eq!(account_proof.verify(root), Ok(()));
    });
}

#[test]
fn fork_genesis_nonexistent_account_proof() {
    // The historic `mainnet_genesis_account_proof*` tests asserted fixtures captured from
    // Ethereum L1 mainnet (`cast proof ... --block 0`), but this fork's `MAINNET` spec is a
    // custom fork genesis (chain id 315, 20-account alloc), so those fixtures no longer match.
    // Keep the meaningful part: a non-inclusion proof against the fork's own genesis root.
    let factory = create_test_provider_factory();
    let root = insert_genesis(&factory, MAINNET.clone()).unwrap();

    // Address that does not exist in the fork genesis allocation.
    let target = address!("0x000d836201318ec6899a67540690382780743280");

    let provider = factory.provider().unwrap();
    smartnet_trie_db::with_adapter!(provider, |A| {
        let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(provider.tx_ref());
        let account_proof = proof.account_proof(target, &[]).unwrap();
        assert!(!account_proof.proof.is_empty());
        assert_eq!(account_proof.verify(root), Ok(()));
    });
}
