//! TEST-ONLY keys — copied verbatim from `go-tools/1/seed`.
//!
//! **EXCEPTION (tools/README.md):** these throwaway keys from the dev mnemonic
//! "energy square tooth hub feature ordinary easily between risk mosquito remind gesture"
//! are deliberately hard-coded so the tools are zero-config on the local chain-315
//! test network. REAL keys must never be added to this crate, never tested, never
//! committed — see repo PRIVATE-KEY.md.

/// The dev mnemonic that produced every key below (test-only, zero real value).
pub const SEED_MNEMONIC: &str =
    "energy square tooth hub feature ordinary easily between risk mosquito remind gesture";

/// `(path, address, privkey)` rows of `go-tools/1/seed`, in file order.
pub const TEST_KEYS: &[(&str, &str, &str)] = &[
    (
        "m/44'/60'/0'/0/0",
        "0x55155936E45eAA889eC5acFd0e895622e803afcB",
        "e7e7daea7dc5fa479dabdc93dec5a53512a83de7be369d31eeb014dcad8e10e8",
    ),
    (
        "m/44'/60'/0'/0/1",
        "0xBd9c08F6A45BdafC0848dc5C882572d3f8C99635",
        "07e08b14a7ab909dee0db8e1fbc3a74c4b3258b60428346da7079dd5b79c9a98",
    ),
    (
        "m/44'/60'/0'/0/2",
        "0x28c0E392FfF9a9201e26Eb860435f63dd9d20D03",
        "17d742169e20e5209c27e61cbc5373b78f240dfcef564b9dceaa3e00f22db13c",
    ),
    (
        "m/44'/60'/0'/0/3",
        "0xD3a9476A6d87112b8BF6295c8a348F096D5EDAd5",
        "b9c8992f5dbdc66689696e675e296a9b9ce65e41f527578365525499e05fccad",
    ),
    (
        "m/44'/60'/0'/0/4",
        "0xE5De726deD4D8d21f6631032E98Dec8dd7e1438e",
        "60387146d83f119cca50d664e5cdb37271a57003e5495092b0c286057125a11a",
    ),
    (
        "m/44'/60'/0'/0/5",
        "0x25fcbc81983A89aD11848B65979B66e9Dc932872",
        "962da199fc919739559c9bad8356271c12b84ab1a1948c4e6a56b11bd64a616c",
    ),
    (
        "m/44'/60'/0'/0/6",
        "0x1EC93474049bc0e093B25e7d4B4A3a544b5E4622",
        "bfb7b3beffcbc30ff26a8b3833b8a5bed907a84f21e433ffa5d37a24e06e7ea8",
    ),
    (
        "m/44'/60'/0'/0/7",
        "0xbe806F1b3BE423F577BD10B691380F143854259F",
        "93cd15b06ba0bcb8f571a5af79fa200cd1e7f04dfeb43c46cd6a4273084d5724",
    ),
    (
        "m/44'/60'/0'/0/8",
        "0x46C6d7c07827d5B71D15A86934b0a66F28a71019",
        "8b74232df53f6a220717430f855f6248ef28450d398f1e5533b7476bde4085f6",
    ),
    (
        "m/44'/60'/0'/0/9",
        "0x4210c3729D9Becc024a267feecF4B69599B10C84",
        "3dfa336ac80f30f9e5863241fb984546c6124f0956d9f2c7ae3057427a0813fa",
    ),
    (
        "m/44'/60'/0'/0/10",
        "0x868982Fb4c603e437C521C19cE20b39BE9c77DBC",
        "ad132041ca77debb146c6616c60e77d81cf9e843c0b4a7dfed6f2eec2943d6c7",
    ),
];

/// Index-0 key: the load-test funder (== go-tools default `privKeyHex`).
pub const TEST_FUNDER_PRIV: &str = "e7e7daea7dc5fa479dabdc93dec5a53512a83de7be369d31eeb014dcad8e10e8";
/// Index-0 address == validator signer (0x5515…).
pub const TEST_FUNDER_ADDR: &str = "0x55155936E45eAA889eC5acFd0e895622e803afcB";
/// Index-10 address == go-tools default `addressTo`.
pub const TEST_RECIPIENT: &str = "0x868982Fb4c603e437C521C19cE20b39BE9c77DBC";
