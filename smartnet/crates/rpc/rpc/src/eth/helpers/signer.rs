//! An abstraction over ethereum signers.

use alloy_dyn_abi::TypedData;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{eip191_hash_message, map::AddressMap, Address, Signature, B256};
use alloy_signer::SignerSync;
use alloy_signer_local::{coins_bip39::English, MnemonicBuilder, PrivateKeySigner};
use smartnet_rpc_convert::SignableTxRequest;
use smartnet_rpc_eth_api::helpers::{signer::Result, EthSigner};
use smartnet_rpc_eth_types::SignError;

/// Holds developer keys
#[derive(Debug, Clone)]
pub struct DevSigner {
    addresses: Vec<Address>,
    accounts: AddressMap<PrivateKeySigner>,
}

impl DevSigner {
    /// Generates provided number of random dev signers
    /// which satisfy [`EthSigner`] trait
    pub fn random_signers<T: Decodable2718, TxReq: SignableTxRequest<T>>(
        num: u32,
    ) -> Vec<Box<dyn EthSigner<T, TxReq> + 'static>> {
        let mut signers = Vec::with_capacity(num as usize);
        for _ in 0..num {
            let sk = PrivateKeySigner::random();

            let address = sk.address();
            let addresses = vec![address];

            let accounts = AddressMap::from_iter([(address, sk)]);
            signers.push(Box::new(Self { addresses, accounts }) as Box<dyn EthSigner<T, TxReq>>);
        }
        signers
    }

    /// Generates dev signers deterministically from a fixed mnemonic.
    /// Uses the Ethereum derivation path: `m/44'/60'/0'/0/{index}`
    pub fn from_mnemonic<T: Decodable2718, TxReq: SignableTxRequest<T>>(
        mnemonic: &str,
        num: u32,
    ) -> Vec<Box<dyn EthSigner<T, TxReq> + 'static>> {
        let mut signers = Vec::with_capacity(num as usize);

        for i in 0..num {
            let sk = MnemonicBuilder::<English>::default()
                .phrase(mnemonic)
                .index(i)
                .expect("invalid derivation path")
                .build()
                .expect("failed to build signer from mnemonic");

            let address = sk.address();
            let addresses = vec![address];
            let accounts = AddressMap::from_iter([(address, sk)]);

            signers.push(Box::new(Self { addresses, accounts }) as Box<dyn EthSigner<T, TxReq>>);
        }

        signers
    }

    fn get_key(&self, account: Address) -> Result<&PrivateKeySigner> {
        self.accounts.get(&account).ok_or(SignError::NoAccount)
    }

    fn sign_hash(&self, hash: B256, account: Address) -> Result<Signature> {
        let signature = self.get_key(account)?.sign_hash_sync(&hash);
        signature.map_err(|_| SignError::CouldNotSign)
    }
}

#[async_trait::async_trait]
impl<T: Decodable2718, TxReq: SignableTxRequest<T>> EthSigner<T, TxReq> for DevSigner {
    fn accounts(&self) -> Vec<Address> {
        self.addresses.clone()
    }

    fn is_signer_for(&self, addr: &Address) -> bool {
        self.accounts.contains_key(addr)
    }

    async fn sign(&self, address: Address, message: &[u8]) -> Result<Signature> {
        // Hash message according to EIP 191:
        // https://ethereum.org/es/developers/docs/apis/json-rpc/#eth_sign
        let hash = eip191_hash_message(message);
        self.sign_hash(hash, address)
    }

    async fn sign_transaction(&self, request: TxReq, address: &Address) -> Result<T> {
        // create local signer wallet from signing key
        let signer = self.accounts.get(address).ok_or(SignError::NoAccount)?.clone();

        // build and sign transaction with signer
        let tx = request
            .try_build_and_sign(&signer)
            .await
            .map_err(|_| SignError::InvalidTransactionRequest)?;

        Ok(tx)
    }

    fn sign_typed_data(&self, address: Address, payload: &TypedData) -> Result<Signature> {
        let encoded = payload.eip712_signing_hash().map_err(|_| SignError::InvalidTypedData)?;
        self.sign_hash(encoded, address)
    }
}
