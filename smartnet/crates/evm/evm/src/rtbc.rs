//! RTBC deployment admission policy — size limits + executable-`SELFDESTRUCT` screening.
//!
//! The separate class of `SC-FILTER.md`: pure bytecode policy, no interpreter
//! dependencies, no EVM semantics. Hooks (`create` / `create_end`, Wave 3) call into
//! this module and refuse; revm executes and rolls back as usual.

use alloy_primitives::Bytes;
use revm::bytecode::Bytecode;

/// Deployment admission policy (`SC-FILTER.md` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtbcPolicy {
    /// Maximum creation-input (initcode) length in bytes. EIP-3860's value; revm does
    /// not enforce it under the MARS `SpecId::LONDON` execution spec (`W0.1b`).
    pub max_initcode_size: usize,
    /// Maximum deployed runtime bytecode length in bytes. EIP-170's value — revm
    /// already enforces this (belt-only here, `W0.1`).
    pub max_runtime_size: usize,
}

impl Default for RtbcPolicy {
    /// Mirrors `smartnet_chainspec::{MAX_INITCODE_SIZE, MAX_RUNTIME_SIZE}` — those
    /// are the authoritative protocol constants (`SC-FILTER.md` §7.4); this crate
    /// sits below chainspec's consumers and cannot depend on it, so the values are
    /// repeated here for tests and dependency-free use. Wiring (Wave 3) constructs
    /// the policy **from chainspec**, not from `default()`.
    fn default() -> Self {
        Self {
            max_initcode_size: 49_152,
            max_runtime_size: 24_576,
        }
    }
}

/// Why a deployment was refused — deterministic, never a panic (`SC-FILTER.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtbcReject {
    /// Creation input exceeded `RtbcPolicy::max_initcode_size`.
    InitcodeTooLarge,
    /// Returned runtime bytecode exceeded `RtbcPolicy::max_runtime_size`.
    RuntimeTooLarge,
    /// Executable `SELFDESTRUCT` (0xff) found in the runtime instruction stream.
    ForbiddenOpcode {
        /// Byte offset of the offending opcode.
        pc: usize,
        /// The offending opcode (always 0xff today; kept generic for diagnostics).
        opcode: u8,
    },
}

/// Stage 1 — before initcode execution: **size check only** (`SC-FILTER.md` §2).
///
/// No opcode scan of initcode: its trailing regions (embedded runtime blob,
/// constructor args) are data with a different decode alignment than the runtime —
/// scanning them false-rejects real contracts (measured, §5). The post-constructor
/// [`check_runtime`] is the sound gate.
pub const fn check_initcode(initcode: &[u8], policy: &RtbcPolicy) -> Result<(), RtbcReject> {
    if initcode.len() > policy.max_initcode_size {
        return Err(RtbcReject::InitcodeTooLarge);
    }
    Ok(())
}

/// Stage 2 — after the constructor, before install (`SC-FILTER.md` §2 + §5):
/// size check, then PUSH-aware scan for executable `SELFDESTRUCT` on the metadata-
/// stripped body.
///
/// Load-bearing rules (both measured against real solc output, §5):
/// - trailing solc CBOR metadata is stripped first (see [`strip_solc_metadata`]);
/// - a truncated PUSH ends the decode — it is never an error (EVM zero-pads).
pub fn check_runtime(code: &[u8], policy: &RtbcPolicy) -> Result<(), RtbcReject> {
    if code.len() > policy.max_runtime_size {
        return Err(RtbcReject::RuntimeTooLarge);
    }
    let body = strip_solc_metadata(code);
    // `new_legacy` → LegacyAnalyzed, so `iter_opcodes` walks real instructions and
    // skips PUSH immediates (the PUSH-aware decoder of §5 — revm's, not ours).
    let bytecode = Bytecode::new_legacy(Bytes::copy_from_slice(body));
    let mut iter = bytecode.iter_opcodes();
    loop {
        // `position()` is the offset of the byte `next()` will yield.
        let pc = iter.position();
        let Some(op) = iter.next() else { break };
        if op == 0xff {
            return Err(RtbcReject::ForbiddenOpcode { pc, opcode: op });
        }
    }
    Ok(())
}

/// Strip the trailing solc CBOR metadata suffix if it validates as such
/// (`SC-FILTER.md` §5) — otherwise return the input unchanged.
///
/// Validation: trailing 2-byte big-endian length `L`; the `L` bytes ending right
/// before it must start with a CBOR map header (0xa0..=0xbf), be at most 256 bytes,
/// and contain a known solc metadata key (`ipfs`, `bzzr0`, `bzzr1`, `solc`). Without
/// this strip, the linear decode walks past solc's `fe` terminator into hash bytes
/// and can false-reject production contracts (measured: `wemx` solc-7.6).
fn strip_solc_metadata(code: &[u8]) -> &[u8] {
    match metadata_suffix(code) {
        Some(blob) => &code[..code.len() - 2 - blob.len()],
        None => code,
    }
}

/// The validated trailing solc CBOR metadata blob — the shared rules of
/// [`strip_solc_metadata`] and [`solc_version`].
fn metadata_suffix(code: &[u8]) -> Option<&[u8]> {
    if code.len() < 4 {
        return None;
    }
    let len = u16::from_be_bytes([code[code.len() - 2], code[code.len() - 1]]) as usize;
    if len == 0 || len + 2 > code.len() || len > 256 {
        return None;
    }
    let blob = &code[code.len() - 2 - len..code.len() - 2];
    let header = *blob.first()?;
    if !(0xa0..=0xbf).contains(&header) {
        return None;
    }
    let known_key = contains_bytes(blob, b"ipfs")
        || contains_bytes(blob, b"bzzr0")
        || contains_bytes(blob, b"bzzr1")
        || contains_bytes(blob, b"solc");
    if !known_key {
        return None;
    }
    Some(blob)
}

/// The compiler stamp of the trailing solc CBOR metadata: the `solc` key's
/// `major.minor.patch` triple. `None` when there is no metadata suffix or the stamp
/// carries no `solc` key (pre-0.5.9 builds emit only `bzzr0`/`bzzr1`/`ipfs`).
///
/// Used by the contract-metadata capture (`{address}.meta` carries `compiler`).
pub fn solc_version(code: &[u8]) -> Option<[u8; 3]> {
    let blob = metadata_suffix(code)?;
    // The `solc` key is CBOR text(4): `0x64 's' 'o' 'l' 'c'`, value = array(3) of uints.
    let mut idx = 0;
    while let Some(pos) = blob[idx..].windows(4).position(|w| w == b"solc").map(|p| p + idx) {
        if pos > 0 && blob[pos - 1] == 0x64 {
            let rest = &blob[pos + 4..];
            // solc stamps the version as bytes(3) — `0x43 0x0M 0x0m 0x0p` (measured on
            // 0.5.16 / 0.7.6 / 0.8.26 artifacts); some tools emit array(3) of uints.
            return match *rest.first()? {
                0x43 => Some([*rest.get(1)?, *rest.get(2)?, *rest.get(3)?]),
                0x83 => {
                    let mut rest = &rest[1..];
                    let mut version = [0u8; 3];
                    for slot in &mut version {
                        let (value, used) = cbor_uint(rest)?;
                        *slot = value;
                        rest = &rest[used..];
                    }
                    Some(version)
                }
                _ => None,
            };
        }
        idx = pos + 4;
    }
    None
}

/// One CBOR unsigned integer (0..=255) — `(value, bytes consumed)`.
fn cbor_uint(rest: &[u8]) -> Option<(u8, usize)> {
    match *rest.first()? {
        value @ 0x00..=0x17 => Some((value, 1)),
        0x18 => Some((*rest.get(1)?, 2)),
        _ => None,
    }
}

/// True if `needle` occurs in `haystack`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};

    fn policy() -> RtbcPolicy {
        RtbcPolicy::default()
    }

    #[test]
    fn executable_selfdestruct_rejected_with_pc() {
        // JUMPDEST, SELFDESTRUCT, STOP
        let code = [0x5b, 0xff, 0x00];
        assert_eq!(
            check_runtime(&code, &policy()),
            Err(RtbcReject::ForbiddenOpcode { pc: 1, opcode: 0xff })
        );
    }

    #[test]
    fn selfdestruct_after_terminal_opcode_still_rejected() {
        // STOP then SELFDESTRUCT — linear decode continues past terminals. Reaching
        // it via jump-into-data is the documented FN class; plain reach is not.
        let code = [0x00, 0xff];
        assert_eq!(
            check_runtime(&code, &policy()),
            Err(RtbcReject::ForbiddenOpcode { pc: 1, opcode: 0xff })
        );
    }

    #[test]
    fn runtime_size_boundary() {
        let at_limit = vec![0x00; 24_576];
        assert_eq!(check_runtime(&at_limit, &policy()), Ok(()));
        let over = vec![0x00; 24_577];
        assert_eq!(check_runtime(&over, &policy()), Err(RtbcReject::RuntimeTooLarge));
    }

    #[test]
    fn initcode_size_boundary() {
        let at_limit = vec![0x00; 49_152];
        assert_eq!(check_initcode(&at_limit, &policy()), Ok(()));
        let over = vec![0x00; 49_153];
        assert_eq!(check_initcode(&over, &policy()), Err(RtbcReject::InitcodeTooLarge));
    }

    #[test]
    fn push_immediate_ff_passes() {
        // PUSH1 0xff STOP — 0xff is data, not an opcode.
        assert_eq!(check_runtime(&[0x60, 0xff, 0x00], &policy()), Ok(()));
        // PUSH20 ff…ff address-mask run (the real-world pattern), then STOP.
        let mut mask = vec![0x73]; // PUSH20
        mask.extend_from_slice(&[0xff; 20]);
        mask.push(0x00);
        assert_eq!(check_runtime(&mask, &policy()), Ok(()));
    }

    #[test]
    fn truncated_push_is_decode_end_not_error() {
        // PUSH32 with only two data bytes — decode ends, no reject (§5; the
        // 86-byte SafeMath case would otherwise false-fail).
        assert_eq!(check_runtime(&[0x7f, 0x01, 0x02], &policy()), Ok(()));
        // PUSH1 with no data at all.
        assert_eq!(check_runtime(&[0x60], &policy()), Ok(()));
        // Code ending mid-push after a jumpdest/stop pair.
        let code = [0x5b, 0x00, 0x61, 0x41, 0x42];
        assert_eq!(check_runtime(&code, &policy()), Ok(()));
    }

    #[test]
    fn initcode_is_size_only_even_with_ff() {
        // Executable 0xff in initcode must NOT be scanned (§2 operator ruling):
        // size-only is the whole rule for stage 1.
        let code = [0x5b, 0xff, 0x00];
        assert_eq!(check_initcode(&code, &policy()), Ok(()));
    }

    #[test]
    fn empty_and_tiny_pass() {
        assert_eq!(check_runtime(&[], &policy()), Ok(()));
        assert_eq!(check_runtime(&[0x00], &policy()), Ok(()));
    }

    /// Synthetic metadata suffix containing a raw 0xff byte: strip must hide it.
    #[test]
    fn solc_version_reads_the_metadata_stamp() {
        // synthetic: stand-in runtime + CBOR map(1) {"solc": h'000706'} + 2-byte length —
        // the bytes(3) form real solc builds carry
        let mut code = vec![0xfe];
        code.extend([0xa1, 0x64, b's', b'o', b'l', b'c', 0x43, 0x00, 0x07, 0x06]);
        let len = code.len() - 1;
        code.extend((len as u16).to_be_bytes());
        assert_eq!(solc_version(&code), Some([0, 7, 6]));

        // the array(3) form some tools emit
        let mut code = vec![0xfe];
        code.extend([0xa1, 0x64, b's', b'o', b'l', b'c', 0x83, 0x00, 0x07, 0x06]);
        let len = code.len() - 1;
        code.extend((len as u16).to_be_bytes());
        assert_eq!(solc_version(&code), Some([0, 7, 6]));
    }

    #[test]
    fn solc_version_decodes_two_byte_uints_and_absent_key() {
        // minor/patch >= 24 encode as 0x18 xx — e.g. 0.8.26
        let mut code = vec![0xfe];
        code.extend([0xa1, 0x64, b's', b'o', b'l', b'c', 0x83, 0x00, 0x08, 0x18, 0x1a]);
        let len = code.len() - 1;
        code.extend((len as u16).to_be_bytes());
        assert_eq!(solc_version(&code), Some([0, 8, 26]));

        // bzzr-only stamp (pre-`solc`-key): no version
        let mut code = vec![0xfe];
        code.extend([0xa1, 0x64, b'b', b'z', b'z', b'r', b'0', 0x40]);
        let len = code.len() - 1;
        code.extend((len as u16).to_be_bytes());
        assert_eq!(solc_version(&code), None);

        // no suffix at all
        assert_eq!(solc_version(&[0xfe, 0x00]), None);
    }

    #[test]
    fn solc_version_reads_real_artifacts() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for (file, expected) in [
            ("wemx.7.6.opt.json", Some([0u8, 7, 6])),
            ("wemc.8.26.london.opt.json", Some([0u8, 8, 26])),
        ] {
            let raw = fs::read_to_string(dir.join(file)).expect("fixture");
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            for (name, contract) in json["contracts"].as_object().expect("contracts map") {
                let runtime_hex = contract["bin-runtime"].as_str().unwrap_or_default();
                if runtime_hex.is_empty() {
                    continue;
                }
                let runtime = alloy_primitives::hex::decode(runtime_hex).unwrap();
                assert_eq!(solc_version(&runtime), expected, "{file}:{name}");
            }
        }
    }

    #[test]
    fn metadata_suffix_with_ff_passes() {
        // body: JUMPDEST STOP
        let mut code = vec![0x5b, 0x00];
        // CBOR map(1) { "ipfs": bytes(32) } — hash bytes deliberately carry 0xff.
        let mut blob = vec![0xa1, 0x64];
        blob.extend_from_slice(b"ipfs");
        blob.extend_from_slice(&[0x58, 0x20]);
        let mut hash = [0x11u8; 32];
        hash[3] = 0xff;
        hash[17] = 0xff;
        blob.extend_from_slice(&hash);
        assert!(contains_bytes(&blob, b"ipfs"));
        code.extend_from_slice(&blob);
        code.extend_from_slice(&(blob.len() as u16).to_be_bytes());
        // Raw 0xff exists only inside the suffix; decoded-as-metadata it never
        // appears as an opcode.
        assert_eq!(check_runtime(&code, &policy()), Ok(()));
    }

    /// Suffix that fails validation must NOT be stripped — 0xff stays visible.
    #[test]
    fn invalid_suffix_is_not_stripped_and_rejects() {
        // No CBOR map header in the pointed-to blob → strip declines → JUMPDEST,
        // 0xff … the decode hits the opcode.
        let code = [0x5b, 0xff, 0x00, 0x00, 0x00, 0x03];
        assert_eq!(
            check_runtime(&code, &policy()),
            Err(RtbcReject::ForbiddenOpcode { pc: 1, opcode: 0xff })
        );
        // Map header but no known solc key → strip still declines → 0xff at pc 2.
        let code = [0x5b, 0xa0, 0xff, 0x00, 0x00, 0x03];
        assert_eq!(
            check_runtime(&code, &policy()),
            Err(RtbcReject::ForbiddenOpcode { pc: 2, opcode: 0xff })
        );
    }

    /// Real solc artifacts (§5/§6): every `tools/erc-20` compilation must pass both
    /// stages — the metadata-FP probe, with `wemx` 7.6 as the known canary.
    #[test]
    fn real_artifacts_pass_both_stages() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let policy = RtbcPolicy::default();
        let mut checked = 0usize;
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("fixtures dir {:?}: {e}", dir))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "no fixture JSONs in {:?}", dir);
        for path in entries {
            let raw = fs::read_to_string(&path).unwrap();
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            for (name, contract) in json["contracts"].as_object().expect("contracts map") {
                let runtime_hex = contract["bin-runtime"].as_str().unwrap_or_default();
                if runtime_hex.is_empty() {
                    continue;
                }
                let runtime = alloy_primitives::hex::decode(runtime_hex)
                    .unwrap_or_else(|e| panic!("{}: bad hex: {e}", path.display()));
                check_runtime(&runtime, &policy).unwrap_or_else(|e| {
                    panic!("{} contract {name}: {e:?}", path.file_name().unwrap().to_string_lossy())
                });
                let init_hex = contract["bin"].as_str().unwrap_or_default();
                if !init_hex.is_empty() {
                    let init = alloy_primitives::hex::decode(init_hex).expect("creation hex");
                    check_initcode(&init, &policy).unwrap_or_else(|e| {
                        panic!(
                            "{} contract {name}: {e:?}",
                            path.file_name().unwrap().to_string_lossy()
                        )
                    });
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 43, "expected 43 compiled contract variants (26 + V2/proxy/create2/wemu/wwem)");
    }
}
