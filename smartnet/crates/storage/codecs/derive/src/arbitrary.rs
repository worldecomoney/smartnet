use proc_macro::TokenStream;
use proc_macro2::{Ident, TokenStream as TokenStream2};
use quote::ToTokens;

/// Generates the proptest roundtrip test module for a type.
///
/// This historically parsed `compact` / `rlp` / case-count arguments and emitted the
/// corresponding roundtrip proptests. That body was removed when the generic-upstream
/// codec tests were retired, so the function now always emits nothing. The signature is
/// kept so the `add_arbitrary_tests` and `generate_tests` call sites (which pass their
/// arguments through) still compile unchanged.
pub fn maybe_generate_tests(
    _args: TokenStream,
    _type_ident: &impl ToTokens,
    _mod_tests: &Ident,
) -> TokenStream2 {
    TokenStream2::default()
}
