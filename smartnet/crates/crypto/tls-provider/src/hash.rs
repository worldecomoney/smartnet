//! SHA-2 hash providers for [`rustls`](rustls::crypto::hash::Hash).

use core::fmt;

use rustls::crypto::hash::{Context, Hash, HashAlgorithm, Output};
use sha2::{Digest, Sha256, Sha384};

/// SHA-256 hash provider (RFC 6234).
#[derive(Debug, Clone, Copy, Default)]
pub struct Sha256Provider;

/// SHA-384 hash provider (RFC 6234).
#[derive(Debug, Clone, Copy, Default)]
pub struct Sha384Provider;

/// SHA-256 provider instance.
pub static SHA256: Sha256Provider = Sha256Provider;
/// SHA-384 provider instance.
pub static SHA384: Sha384Provider = Sha384Provider;

impl Hash for Sha256Provider {
    fn start(&self) -> Box<dyn Context> {
        Box::new(Sha256Context(Sha256::new()))
    }

    fn hash(&self, bytes: &[u8]) -> Output {
        let digest = Sha256::digest(bytes);
        Output::new(&digest)
    }

    fn output_len(&self) -> usize {
        32
    }

    fn algorithm(&self) -> HashAlgorithm {
        HashAlgorithm::SHA256
    }

    fn fips(&self) -> bool {
        false
    }
}

impl Hash for Sha384Provider {
    fn start(&self) -> Box<dyn Context> {
        Box::new(Sha384Context(Sha384::new()))
    }

    fn hash(&self, bytes: &[u8]) -> Output {
        let digest = Sha384::digest(bytes);
        Output::new(&digest)
    }

    fn output_len(&self) -> usize {
        48
    }

    fn algorithm(&self) -> HashAlgorithm {
        HashAlgorithm::SHA384
    }

    fn fips(&self) -> bool {
        false
    }
}

struct Sha256Context(Sha256);

impl fmt::Debug for Sha256Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sha256Context").finish_non_exhaustive()
    }
}

impl Context for Sha256Context {
    fn fork_finish(&self) -> Output {
        Output::new(&self.0.clone().finalize())
    }

    fn fork(&self) -> Box<dyn Context> {
        Box::new(Self(self.0.clone()))
    }

    fn finish(self: Box<Self>) -> Output {
        Output::new(&self.0.finalize())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

struct Sha384Context(Sha384);

impl fmt::Debug for Sha384Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sha384Context").finish_non_exhaustive()
    }
}

impl Context for Sha384Context {
    fn fork_finish(&self) -> Output {
        Output::new(&self.0.clone().finalize())
    }

    fn fork(&self) -> Box<dyn Context> {
        Box::new(Self(self.0.clone()))
    }

    fn finish(self: Box<Self>) -> Output {
        Output::new(&self.0.finalize())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn sha256_abc_vector() {
        let out = SHA256.hash(b"abc");
        assert_eq!(
            out.as_ref(),
            unhex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn sha384_abc_vector() {
        let out = SHA384.hash(b"abc");
        assert_eq!(
            out.as_ref(),
            unhex(
                "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
                 8086072ba1e7cc2358baeca134c825a7"
            )
        );
    }

    #[test]
    fn incremental_matches_one_shot() {
        let mut ctx = SHA256.start();
        ctx.update(b"ab");
        ctx.update(b"c");
        let incremental = ctx.finish();
        assert_eq!(incremental.as_ref(), SHA256.hash(b"abc").as_ref());

        let mut forked = SHA256.start();
        forked.update(b"ab");
        let fork = forked.fork();
        forked.update(b"c");
        assert_eq!(forked.finish().as_ref(), SHA256.hash(b"abc").as_ref());
        // the fork is an independent snapshot taken before the update
        assert_eq!(fork.finish().as_ref(), SHA256.hash(b"ab").as_ref());
    }
}
