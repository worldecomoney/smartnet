//! THE `bridges.json` schema (`BRIDGE-DOC.md` §3) — parsed, **fail-closed validated**,
//! and loaded from disk.
//!
//! Every node carries the same copy of `~/.smartnet/bridges.json`; the chair reads the
//! bridge list from it at every seal, the bridge scans with it. **One problem anywhere
//! rejects the file as a whole** ([`BridgesFile::validate`]): no bridge on that node, and
//! a bridge node must refuse to boot on a missing or rejected file (the callers decide
//! that; this module only ever hands over a fully validated file or an exact reason).

use alloy_primitives::{address, Address, U256};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::Deserialize;

use super::bridge_signal::OUT_AUTHORIZATION_WEI;

/// The burn sink for OUT when `burn_to` is absent from the file (§3.1).
pub const DEFAULT_BURN_TO: Address = address!("0x000000000000000000000000000000000000dead");

/// The parsed file — exactly the wire keys of §3. Unknown keys are ignored; missing
/// required keys fail the parse, which is fail-closed by definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BridgesFile {
    /// One fee for every transfer, both directions, every network — a plain decimal WEM
    /// amount (`"0.10"`). `""`/absent = no bridge fee.
    pub bridge_fee: Option<String>,
    /// The burn sink for OUT. Absent → [`DEFAULT_BURN_TO`].
    pub burn_to: Option<Address>,
    /// The watched pairs (§3.3).
    pub pairs: BTreeMap<String, Pair>,
    /// The networks (§3.2), keyed by hex chain id.
    pub networks: BTreeMap<String, Network>,
}

/// One watched pair: the pair's wallet — **the** watched address on both chains.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pair {
    /// The pair's wallet — **the** watched address on both chains.
    pub address: Address,
    /// The only place a direction is gated (§3.3). Missing = gated shut.
    #[serde(default)]
    pub in_enabled: bool,
    /// The only place a direction is gated (§3.3). Missing = gated shut.
    #[serde(default)]
    pub out_enabled: bool,
    /// Instrument name → chain. Exactly two legs, exactly one of them the own network.
    pub legs: BTreeMap<String, Leg>,
}

/// One pair leg: the instrument name is the key in the enclosing map.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Leg {
    /// The chain this leg lives on — a hex chain id of a configured network.
    pub chain_id: String,
}

/// One network (§3.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Network {
    /// Must mirror the map key (hex chain id, e.g. `"0x38"`).
    #[serde(rename = "chainId")]
    pub chain_id: String,
    /// Short name for logs (`"BSC"`).
    pub alias: String,
    /// Human name (informational).
    #[serde(rename = "chainName")]
    pub chain_name: String,
    /// **Exactly one** network is `true` — our chain.
    pub is_own: bool,
    /// Ordered HTTP(S) failover; every bridge call on this chain goes through it.
    #[serde(rename = "rpcUrls")]
    pub rpc_urls: Vec<String>,
    /// Informational only.
    #[serde(rename = "blockExplorerUrls", default)]
    pub block_explorer_urls: Option<Vec<String>>,
    /// The landing network's fee, in WEM. `""`/absent = none.
    pub network_fee: Option<String>,
    /// Smallest transfer this network accepts, in WEM. `""`/`"0"` = none.
    #[serde(rename = "minAmount")]
    pub min_amount: Option<String>,
    /// Maturity: hits shallower than this are not accepted (≥ 1).
    #[serde(rename = "minConfBlocks")]
    pub min_conf_blocks: u64,
    /// Age bound in blocks; must exceed `minConfBlocks` (a hit exactly this deep is still
    /// admitted), and stay ≤ 100 000 so the window never outlives the 48 h record memory.
    #[serde(rename = "maxConfBlocks")]
    pub max_conf_blocks: u64,
    /// Bridge identities the chair will answer — **own network only** (§3.2).
    pub bridges: Option<BTreeMap<String, BridgeIdentity>>,
    /// Reference instruments (§3.4).
    pub instruments: BTreeMap<String, Instrument>,
}

/// One bridge identity the chair answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BridgeIdentity {
    /// `""` is a stub and **matches nobody**; otherwise the bridge's node address.
    pub address: String,
    /// Missing = not active.
    #[serde(default)]
    pub status: bool,
}

/// One reference instrument (§3.4). `stable`/`price` are informational and deliberately
/// not modeled — they never enter a calculation, so they never enter the type.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Instrument {
    /// `"coin"` / `"native"` or `"erc20"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The contract — required for `erc20`.
    pub address: Option<Address>,
    /// Trusted as written — never verified over RPC. ≤ 77 (integer scaling must never
    /// wrap `U256`).
    pub decimals: u8,
}

/// Parses a **plain decimal WEM amount** (`"0.10"`, `"10"`, `"5.00"`) into wei. Strict:
/// digits, one optional `.`, at most 18 fraction digits — no signs, no exponents, no
/// floats. `None` on anything else.
pub fn parse_wem_decimal(s: &str) -> Option<U256> {
    if s.is_empty() {
        return None;
    }
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => {
            if f.is_empty() {
                return None; // "1." is not a plain decimal amount
            }
            (i, Some(f))
        }
        None => (s, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_digits = frac.unwrap_or("");
    if frac_digits.len() > 18 || !frac_digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let whole = int.parse::<U256>().ok()?;
    let mut wei = whole.checked_mul(U256::from(10).pow(U256::from(18)))?;
    if !frac_digits.is_empty() {
        let frac_val = frac_digits.parse::<U256>().ok()?;
        let scale = U256::from(10).pow(U256::from(18 - frac_digits.len() as u64));
        wei = wei.checked_add(frac_val.checked_mul(scale)?)?;
    }
    Some(wei)
}

fn wem_or_none(value: &Option<String>) -> Option<U256> {
    value.as_deref().and_then(|s| if s.is_empty() { None } else { parse_wem_decimal(s) })
}

fn hex_chain_id(s: &str) -> Option<u64> {
    let tail = s.strip_prefix("0x")?;
    if tail.is_empty() || tail.len() > 16 || !tail.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(tail, 16).ok()
}

impl BridgesFile {
    /// The parsed + **fully validated** file, or the exact reason the file is rejected as
    /// a whole (§3.6: one problem anywhere means no bridge on that node).
    pub fn parse(json: &str) -> Result<Self, String> {
        let file: Self = serde_json::from_str(json).map_err(|e| format!("bridges.json: {e}"))?;
        file.validate()?;
        Ok(file)
    }

    /// Fail-closed validation (§3.6 + §3.2 + §10). The first problem rejects the file.
    pub fn validate(&self) -> Result<(), String> {
        // §3.6 — plain decimal WEM amounts.
        if let Some(v) = &self.bridge_fee
            && !v.is_empty()
            && parse_wem_decimal(v).is_none()
        {
            return Err(format!("bridge_fee: {v:?} is not a plain decimal WEM amount"));
        }

        // Networks: key mirrors chainId, unique, well-formed.
        let mut chain_ids: Vec<u64> = Vec::new();
        let mut own_count = 0usize;
        for (key, net) in &self.networks {
            let Some(id) = hex_chain_id(key) else {
                return Err(format!("networks[{key}]: key is not a hex chain id"));
            };
            if &net.chain_id != key {
                return Err(format!(
                    "networks[{key}]: chainId {:?} does not mirror the map key",
                    net.chain_id
                ));
            }
            if chain_ids.contains(&id) {
                return Err(format!("networks[{key}]: duplicate chain id"));
            }
            chain_ids.push(id);
            if net.is_own {
                own_count += 1;
            }
            if net.alias.is_empty() {
                return Err(format!("networks[{key}]: alias is empty"));
            }
            if net.rpc_urls.is_empty() {
                return Err(format!("networks[{key}]: rpcUrls is empty"));
            }
            for url in &net.rpc_urls {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(format!("networks[{key}]: rpcUrl {url:?} is not HTTP(S)"));
                }
            }
            if net.min_conf_blocks < 1 {
                return Err(format!("networks[{key}]: minConfBlocks must be ≥ 1"));
            }
            if net.max_conf_blocks <= net.min_conf_blocks {
                return Err(format!("networks[{key}]: maxConfBlocks must exceed minConfBlocks"));
            }
            if net.max_conf_blocks > 100_000 {
                return Err(format!(
                    "networks[{key}]: maxConfBlocks must stay ≤ 100_000 (the 48 h memory)"
                ));
            }
            for (where_, raw) in [("network_fee", &net.network_fee), ("minAmount", &net.min_amount)]
            {
                if let Some(v) = raw
                    && !v.is_empty()
                    && parse_wem_decimal(v).is_none()
                {
                    return Err(format!("networks[{key}]: {where_} {v:?} is not a plain decimal WEM amount"));
                }
            }
            for (name, ins) in &net.instruments {
                if ins.kind != "coin" && ins.kind != "native" && ins.kind != "erc20" {
                    return Err(format!(
                        "networks[{key}].instruments[{name}]: type {:?} is not coin/native/erc20",
                        ins.kind
                    ));
                }
                if ins.decimals > 77 {
                    return Err(format!(
                        "networks[{key}].instruments[{name}]: decimals {} > 77",
                        ins.decimals
                    ));
                }
                if ins.kind == "erc20" && ins.address.is_none() {
                    return Err(format!(
                        "networks[{key}].instruments[{name}]: erc20 without an address"
                    ));
                }
            }
        }
        if own_count != 1 {
            return Err(format!("networks: exactly one is_own is required, found {own_count}"));
        }

        // Bridges: own network only; non-empty addresses valid and unique ("" = a stub
        // that matches nobody).
        let mut listed: Vec<Address> = Vec::new();
        for (key, net) in &self.networks {
            match (&net.bridges, net.is_own) {
                (Some(_), false) => {
                    return Err(format!("networks[{key}]: bridges are allowed on the own network only"));
                }
                (None, _) => {}
                (Some(bridges), true) => {
                    for (name, id) in bridges {
                        if id.address.is_empty() {
                            continue; // a stub — matches nobody
                        }
                        let Ok(addr) = id.address.parse::<Address>() else {
                            return Err(format!(
                                "networks[{key}].bridges[{name}]: {:?} is not an address",
                                id.address
                            ));
                        };
                        if listed.contains(&addr) {
                            return Err(format!(
                                "networks[{key}].bridges[{name}]: duplicate bridge address"
                            ));
                        }
                        listed.push(addr);
                    }
                }
            }
        }

        // §10 — exactly 0.01 WEM is the chair's OUT authorization marker, so an IN
        // deposit of that amount has no answerable shape: keep the own network's
        // minAmount above it.
        if let Some((_, own)) = self.own_network()
            && let Some(min) = wem_or_none(&own.min_amount)
            && min != U256::ZERO
            && min <= U256::from(OUT_AUTHORIZATION_WEI)
        {
            return Err(
                "own network: minAmount must be above 0.01 WEM (the chair's OUT marker)"
                    .to_string(),
            );
        }

        // Pairs: unique wallets, exactly two legs, legs name real instruments on real
        // chains, and exactly one leg is the own network (no foreign ↔ foreign).
        let own_chain = self.own_network().map(|(k, _)| k.clone());
        let mut pair_addrs: Vec<Address> = Vec::new();
        for (name, pair) in &self.pairs {
            if pair_addrs.contains(&pair.address) {
                return Err(format!("pairs[{name}]: duplicate pair address"));
            }
            pair_addrs.push(pair.address);
            if pair.legs.len() != 2 {
                return Err(format!("pairs[{name}]: exactly two legs are required"));
            }
            let mut on_own = 0usize;
            for (leg_name, leg) in &pair.legs {
                let Some(id) = hex_chain_id(&leg.chain_id) else {
                    return Err(format!("pairs[{name}].legs[{leg_name}]: chain_id is not hex"));
                };
                let Some((_, net)) = self
                    .networks
                    .iter()
                    .find(|(k, _)| hex_chain_id(k) == Some(id))
                else {
                    return Err(format!(
                        "pairs[{name}].legs[{leg_name}]: chain {:?} is not a configured network",
                        leg.chain_id
                    ));
                };
                if !net.instruments.contains_key(leg_name) {
                    return Err(format!(
                        "pairs[{name}].legs[{leg_name}]: instrument {leg_name:?} does not exist on {}",
                        net.alias
                    ));
                }
                if Some(&leg.chain_id) == own_chain.as_ref() {
                    on_own += 1;
                }
            }
            if on_own != 1 {
                return Err(format!(
                    "pairs[{name}]: exactly one leg must be the own network (no foreign ↔ foreign)"
                ));
            }
        }

        Ok(())
    }

    /// The own network (`is_own`) and its map key — exactly one exists after validation.
    pub fn own_network(&self) -> Option<(&String, &Network)> {
        self.networks.iter().find(|(_, n)| n.is_own)
    }

    /// The burn sink: the configured `burn_to`, or [`DEFAULT_BURN_TO`] when absent.
    pub fn burn_to_or_default(&self) -> Address {
        self.burn_to.unwrap_or(DEFAULT_BURN_TO)
    }

    /// The one bridge fee in wei — `""`/absent = none.
    pub fn bridge_fee_wei(&self) -> Option<U256> {
        wem_or_none(&self.bridge_fee)
    }

    /// The bridge identities the chair answers: own network, `status: true`, non-empty
    /// addresses only — a stub matches nobody.
    pub fn authorized_bridges(&self) -> Vec<Address> {
        let Some((_, own)) = self.own_network() else {
            return Vec::new();
        };
        own.bridges
            .as_ref()
            .map(|bs| {
                bs.values()
                    .filter(|b| b.status && !b.address.is_empty())
                    .filter_map(|b| b.address.parse::<Address>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Network {
    /// The landing network's fee in wei — `""`/absent = none.
    pub fn network_fee_wei(&self) -> Option<U256> {
        wem_or_none(&self.network_fee)
    }

    /// The smallest transfer this network accepts in wei — `""`/`"0"` = none.
    pub fn min_amount_wei(&self) -> Option<U256> {
        wem_or_none(&self.min_amount).filter(|v| *v != U256::ZERO)
    }
}

/// THE boot load: read `~/.smartnet/bridges.json` (from `$HOME`), parse and validate —
/// or the exact reason the file is rejected as a whole.
#[cfg(feature = "std")]
pub fn load_default() -> Result<BridgesFile, String> {
    load(&default_path())
}

/// The one configured path: `~/.smartnet/bridges.json`, from `$HOME` (§3).
#[cfg(feature = "std")]
pub fn default_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::Path::new(&home).join(".smartnet").join("bridges.json")
}

/// Read + parse + validate one file.
#[cfg(feature = "std")]
pub fn load(path: &std::path::Path) -> Result<BridgesFile, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("bridges.json ({}): {e}", path.display()))?;
    BridgesFile::parse(&raw).map_err(|e| format!("bridges.json ({}): {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, fully valid file: one foreign + one own network, a stub and a real
    /// bridge identity, one erc20 pair and one coin pair.
    const VALID: &str = r#"{
      "bridge_fee": "0.10",
      "burn_to": "0x000000000000000000000000000000000000dead",
      "pairs": {
        "BSC_USDT_WEM": {
          "address": "0x1234000000000000000000000000000000005555",
          "in_enabled": true,
          "out_enabled": true,
          "legs": { "USDT": { "chain_id": "0x38" }, "WEM": { "chain_id": "0x13b" } }
        },
        "BSC_BNB_WEM": {
          "address": "0x1234000000000000000000000000000000006666",
          "in_enabled": true,
          "out_enabled": false,
          "legs": { "BNB": { "chain_id": "0x38" }, "WEM": { "chain_id": "0x13b" } }
        }
      },
      "networks": {
        "0x38": {
          "chainId": "0x38",
          "alias": "BSC",
          "chainName": "BNB Smart Chain",
          "is_own": false,
          "rpcUrls": ["https://bsc-dataseed.binance.org/"],
          "network_fee": "0.10",
          "minAmount": "0",
          "minConfBlocks": 10,
          "maxConfBlocks": 100,
          "instruments": {
            "BNB":  { "type": "native", "decimals": 18 },
            "USDT": { "type": "erc20", "address": "0x55d398326f99059fF775485246999027B3197955", "decimals": 18 }
          }
        },
        "0x13b": {
          "chainId": "0x13b",
          "alias": "WEM",
          "chainName": "WEM Mainnet",
          "is_own": true,
          "rpcUrls": ["https://rpc2.wemblockchain.com"],
          "network_fee": "0.01",
          "minAmount": "1",
          "minConfBlocks": 8,
          "maxConfBlocks": 120,
          "bridges": {
            "bridge1": { "address": "", "status": true },
            "bridge2": { "address": "0x00000000000000000000000000000000000000bb", "status": true },
            "bridge3": { "address": "0x00000000000000000000000000000000000000cc", "status": false }
          },
          "instruments": {
            "WEM": { "type": "coin", "decimals": 18 }
          }
        }
      }
    }"#;

    /// The VALID fixture with one string replaced — helpers for the rejection cases.
    fn valid_but(old: &str, new: &str) -> String {
        assert!(VALID.contains(old), "fixture must contain {old:?}");
        VALID.replace(old, new)
    }

    #[test]
    fn the_valid_file_parses_and_validates() {
        let f = BridgesFile::parse(VALID).expect("the fixture is valid");
        assert_eq!(f.pairs.len(), 2);
        assert_eq!(f.networks.len(), 2);
        assert_eq!(f.own_network().unwrap().1.alias, "WEM");
        assert_eq!(f.burn_to_or_default(), DEFAULT_BURN_TO);
        assert_eq!(f.bridge_fee_wei(), Some(U256::from(100_000_000_000_000_000u64)));
    }

    #[test]
    fn parse_wem_decimal_is_exact_and_strict() {
        assert_eq!(parse_wem_decimal("0.10"), Some(U256::from(100_000_000_000_000_000u64)));
        assert_eq!(parse_wem_decimal("10"), Some(U256::from(10_000_000_000_000_000_000u64)));
        assert_eq!(parse_wem_decimal("0"), Some(U256::ZERO));
        assert_eq!(
            parse_wem_decimal("0.000000000000000001"),
            Some(U256::from(1u64)),
            "18 fraction digits are the exact wei"
        );
        for bad in ["", ".", "1.", ".1", "-1", "1e5", "0.10.1", "abc", "1,5", " 1", "0.0000000000000000001"]
        {
            assert_eq!(parse_wem_decimal(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn stubs_match_nobody_and_disabled_bridges_never_authorize() {
        let f = BridgesFile::parse(VALID).unwrap();
        let authorized = f.authorized_bridges();
        assert_eq!(
            authorized,
            vec!["0x00000000000000000000000000000000000000bb".parse::<Address>().unwrap()],
            "only the non-empty, status-true identity is answered"
        );
    }

    #[test]
    fn one_problem_anywhere_rejects_the_whole_file() {
        // §3.6 amounts ("" is legal — §3.1: absent/empty = no fee)
        for bad in ["\"1e5\"", "\"0.10.1\"", "\"-1\"", "\".1\""] {
            let f = BridgesFile::parse(&valid_but("\"bridge_fee\": \"0.10\"", &format!("\"bridge_fee\": {bad}")));
            assert!(f.is_err(), "bridge_fee {bad} must reject the file");
        }
        // key/chainId mirror + hex
        assert!(BridgesFile::parse(&valid_but("\"0x38\": {", "\"0x39\": {")).is_err());
        assert!(BridgesFile::parse(&valid_but("\"0x38\": {", "\"nope\": {")).is_err());
        // alias, rpcUrls
        assert!(BridgesFile::parse(&valid_but("\"alias\": \"BSC\"", "\"alias\": \"\"")).is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"rpcUrls\": [\"https://bsc-dataseed.binance.org/\"]",
            "\"rpcUrls\": []"
        ))
        .is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"rpcUrls\": [\"https://bsc-dataseed.binance.org/\"]",
            "\"rpcUrls\": [\"ws://nope\"]"
        ))
        .is_err());
        // conf windows
        assert!(BridgesFile::parse(&valid_but("\"minConfBlocks\": 10", "\"minConfBlocks\": 0")).is_err());
        assert!(BridgesFile::parse(&valid_but("\"maxConfBlocks\": 100", "\"maxConfBlocks\": 10")).is_err());
        assert!(BridgesFile::parse(&valid_but("\"maxConfBlocks\": 100", "\"maxConfBlocks\": 100001")).is_err());
        // exactly one is_own
        assert!(BridgesFile::parse(&valid_but(
            "\"alias\": \"BSC\",\n          \"chainName\": \"BNB Smart Chain\",\n          \"is_own\": false",
            "\"alias\": \"BSC\",\n          \"chainName\": \"BNB Smart Chain\",\n          \"is_own\": true"
        ))
        .is_err());
        // bridges on a foreign network
        assert!(BridgesFile::parse(&valid_but(
            "\"minConfBlocks\": 10,",
            "\"bridges\": { \"x\": { \"address\": \"0x00000000000000000000000000000000000000dd\", \"status\": true } },\n          \"minConfBlocks\": 10,"
        ))
        .is_err());
        // duplicate bridge address
        assert!(BridgesFile::parse(&valid_but(
            "\"bridge2\": { \"address\": \"0x00000000000000000000000000000000000000bb\", \"status\": true }",
            "\"bridge2\": { \"address\": \"0x00000000000000000000000000000000000000cc\", \"status\": true }"
        ))
        .is_err());
        // instruments: kind, decimals, erc20 address
        assert!(BridgesFile::parse(&valid_but("\"type\": \"coin\"", "\"type\": \"token\"")).is_err());
        assert!(BridgesFile::parse(&valid_but("\"decimals\": 18", "\"decimals\": 78")).is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"type\": \"erc20\", \"address\": \"0x55d398326f99059fF775485246999027B3197955\",",
            "\"type\": \"erc20\","
        ))
        .is_err());
        // pairs: leg count, unknown instrument, unknown chain, foreign ↔ foreign,
        // duplicate pair wallet
        assert!(BridgesFile::parse(&valid_but(
            "\"legs\": { \"USDT\": { \"chain_id\": \"0x38\" }, \"WEM\": { \"chain_id\": \"0x13b\" } }",
            "\"legs\": { \"USDT\": { \"chain_id\": \"0x38\" } }"
        ))
        .is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"legs\": { \"USDT\": { \"chain_id\": \"0x38\" }",
            "\"legs\": { \"NOPE\": { \"chain_id\": \"0x38\" }"
        ))
        .is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"USDT\": { \"chain_id\": \"0x38\" }, \"WEM\": { \"chain_id\": \"0x13b\" }",
            "\"USDT\": { \"chain_id\": \"0x38\" }, \"WEM\": { \"chain_id\": \"0x99\" }"
        ))
        .is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"USDT\": { \"chain_id\": \"0x38\" }, \"WEM\": { \"chain_id\": \"0x13b\" }",
            "\"USDT\": { \"chain_id\": \"0x38\" }, \"BNB\": { \"chain_id\": \"0x38\" }"
        ))
        .is_err());
        assert!(BridgesFile::parse(&valid_but(
            "\"address\": \"0x1234000000000000000000000000000000006666\"",
            "\"address\": \"0x1234000000000000000000000000000000005555\""
        ))
        .is_err());
    }

    #[test]
    fn the_own_min_amount_must_stay_above_the_chair_marker() {
        // 0.01 WEM exactly = the OUT authorization marker — rejected
        assert!(BridgesFile::parse(&valid_but("\"minAmount\": \"1\"", "\"minAmount\": \"0.01\"")).is_err());
        // above it — fine
        assert!(BridgesFile::parse(&valid_but("\"minAmount\": \"1\"", "\"minAmount\": \"0.02\"")).is_ok());
        // none at all — allowed (the accept pipeline sets an unanswerable 0.01 aside)
        assert!(BridgesFile::parse(&valid_but("\"minAmount\": \"1\"", "\"minAmount\": \"0\"")).is_ok());
    }

    #[test]
    fn missing_required_keys_fail_the_parse() {
        assert!(BridgesFile::parse("{}").is_err(), "pairs/networks are required");
        assert!(BridgesFile::parse("{\"pairs\": {}, \"networks\": {}}").is_err());
    }
}
