//! Implements Compact for alloy types.

/// Will make it a pub mod if test-utils is enabled
macro_rules! cond_mod {
    ($($mod_name:ident),*) => {
        $(
            #[cfg(feature = "test-utils")]
            pub mod $mod_name;
            #[cfg(not(feature = "test-utils"))]
            pub(crate) mod $mod_name;
        )*
    };
}

cond_mod!(
    access_list,
    authorization_list,
    genesis_account,
    header,
    log,
    receipt,
    signature,
    trie,
    txkind,
    withdrawal
);

pub mod transaction;

