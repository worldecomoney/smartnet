//! Fixed peer list + DNS gate (`SmartNet` sync peer policy).
//!
//! The DNS check is an **allow-list**: it proves *who* we are willing to talk to, not *what*
//! they say. Data authenticity comes from block seals validated at import. A peer passes when
//! (1) its connection IP is among the IPs its claimed hostname resolves to (forward match),
//! and (2) at least one PTR record of that IP points back into the allowed company-domain
//! suffix (reverse match). Both directions are checked so a stray record inside the domain
//! cannot vouch for an arbitrary host, and an unrelated host cannot claim the domain.

use std::net::Ipv4Addr;

/// A sync peer: claimed hostname (used for the DNS gate) + RPC endpoint.
#[derive(Debug, Clone)]
pub struct SyncPeer {
    /// Hostname this peer is expected to be reachable under (e.g. `node1.nodes.company.com`).
    pub host: String,
    /// JSON-RPC endpoint (e.g. `http://10.0.0.5:8545`). Kept as an IP-based URL on purpose:
    /// the DNS gate verifies host↔IP separately, so RPC itself never resolves the hostname.
    pub url: url::Url,
}

/// Allowed DNS suffix, matched case-insensitively against PTR names (e.g.
/// `nodes.company.com` admits `node1.nodes.company.com.` but not `nodes.company.com.evil.io`).
#[derive(Debug, Clone)]
pub struct PeerDnsPolicy {
    /// Domain suffix that PTR records must fall under (case-insensitive, trailing dot ignored).
    pub allowed_suffix: String,
}

impl PeerDnsPolicy {
    /// `name` (a PTR result) is inside the allowed suffix. An exact match also passes.
    fn ptr_allowed(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let suffix = self.allowed_suffix.trim_end_matches('.').to_ascii_lowercase();
        name == suffix || name.ends_with(&format!(".{suffix}"))
    }
}

/// DNS lookup abstraction so the gate is unit-testable without network access.
pub trait DnsResolver: Send + Sync {
    /// Forward: A records for `host`.
    fn resolve_ipv4(
        &self,
        host: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Ipv4Addr>, String>> + Send;
    /// Reverse: PTR names for `ip`.
    fn resolve_ptr(
        &self,
        ip: Ipv4Addr,
    ) -> impl std::future::Future<Output = Result<Vec<String>, String>> + Send;
}

/// Why a peer failed the DNS gate.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The connection IP is not among the peer's own A records (forward match failed).
    #[error("peer IP {ip} not among A records of {host} (got {found:?})")]
    ForwardMismatch {
        /// IP the connection actually came from.
        ip: String,
        /// Hostname the peer claimed.
        host: String,
        /// A records the forward lookup returned.
        found: Vec<Ipv4Addr>,
    },
    /// The IP has no PTR records at all (reverse match cannot be attempted).
    #[error("no PTR records for {ip}")]
    NoPtr {
        /// IP whose reverse lookup came back empty.
        ip: String,
    },
    /// The IP resolves, but none of its PTR names fall inside the allowed suffix.
    #[error("PTR records of {ip} ({names:?}) are outside allowed suffix")]
    PtrOutsideSuffix {
        /// IP whose PTR records were rejected.
        ip: String,
        /// Every PTR name that was returned.
        names: Vec<String>,
    },
    /// A DNS lookup itself failed (resolver/network error).
    #[error("DNS lookup failed: {0}")]
    Dns(String),
}

/// Verify one peer against the policy: forward A-match on the connection IP plus
/// PTR-match into the allowed suffix.
pub async fn verify_peer(
    resolver: &impl DnsResolver,
    policy: &PeerDnsPolicy,
    peer: &SyncPeer,
    connection_ip: Ipv4Addr,
) -> Result<(), GateError> {
    let ips = resolver.resolve_ipv4(&peer.host).await.map_err(GateError::Dns)?;
    if !ips.contains(&connection_ip) {
        return Err(GateError::ForwardMismatch {
            ip: connection_ip.to_string(),
            host: peer.host.clone(),
            found: ips,
        });
    }
    let ptrs = resolver.resolve_ptr(connection_ip).await.map_err(GateError::Dns)?;
    if ptrs.is_empty() {
        return Err(GateError::NoPtr { ip: connection_ip.to_string() });
    }
    if !ptrs.iter().any(|name| policy.ptr_allowed(name)) {
        return Err(GateError::PtrOutsideSuffix { ip: connection_ip.to_string(), names: ptrs });
    }
    Ok(())
}

/// Hickory-based resolver for production use (system resolver config).
#[derive(Debug, Clone)]
pub struct HickoryDns {
    resolver: hickory_resolver::TokioResolver,
}

impl HickoryDns {
    /// Build a resolver from the system's `/etc/resolv.conf` (production path).
    pub fn from_system_conf() -> Result<Self, String> {
        let resolver =
            hickory_resolver::TokioResolver::builder_tokio().map_err(|e| e.to_string())?.build();
        Ok(Self { resolver })
    }
}

impl DnsResolver for HickoryDns {
    async fn resolve_ipv4(&self, host: &str) -> Result<Vec<Ipv4Addr>, String> {
        use hickory_resolver::proto::rr::{rdata::A, RecordType};
        let name = host.trim_end_matches('.');
        let lookup = self.resolver.lookup(name, RecordType::A).await.map_err(|e| e.to_string())?;
        Ok(lookup
            .records()
            .iter()
            .filter_map(|r| r.try_borrow::<A>().map(|a| a.data().0))
            .collect())
    }

    async fn resolve_ptr(&self, ip: Ipv4Addr) -> Result<Vec<String>, String> {
        use hickory_resolver::proto::rr::{rdata::PTR, RecordType};
        let [a, b, c, d] = ip.octets();
        let reverse = format!("{d}.{c}.{b}.{a}.in-addr.arpa.");
        let lookup =
            self.resolver.lookup(reverse, RecordType::PTR).await.map_err(|e| e.to_string())?;
        Ok(lookup
            .records()
            .iter()
            .filter_map(|r| r.try_borrow::<PTR>().map(|ptr| ptr.data().0.to_string()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDns {
        a: Vec<Ipv4Addr>,
        ptr: Vec<String>,
    }

    impl DnsResolver for MockDns {
        async fn resolve_ipv4(&self, _host: &str) -> Result<Vec<Ipv4Addr>, String> {
            Ok(self.a.clone())
        }
        async fn resolve_ptr(&self, _ip: Ipv4Addr) -> Result<Vec<String>, String> {
            Ok(self.ptr.clone())
        }
    }

    fn peer() -> SyncPeer {
        SyncPeer {
            host: "node1.nodes.company.com".into(),
            url: "http://10.0.0.5:8545".parse().unwrap(),
        }
    }

    fn policy() -> PeerDnsPolicy {
        PeerDnsPolicy { allowed_suffix: "nodes.company.com".into() }
    }

    #[tokio::test]
    async fn pass_on_forward_and_reverse_match() {
        let dns = MockDns {
            a: vec!["10.0.0.5".parse().unwrap()],
            ptr: vec!["node1.nodes.company.com.".into()],
        };
        assert!(verify_peer(&dns, &policy(), &peer(), "10.0.0.5".parse().unwrap()).await.is_ok());
    }

    #[tokio::test]
    async fn reject_wrong_ip() {
        let dns = MockDns {
            a: vec!["10.0.0.9".parse().unwrap()],
            ptr: vec!["node1.nodes.company.com.".into()],
        };
        let err = verify_peer(&dns, &policy(), &peer(), "10.0.0.5".parse().unwrap()).await;
        assert!(matches!(err, Err(GateError::ForwardMismatch { .. })));
    }

    #[tokio::test]
    async fn reject_ptr_outside_suffix() {
        let dns = MockDns {
            a: vec!["10.0.0.5".parse().unwrap()],
            ptr: vec!["other.example.net.".into()],
        };
        assert!(matches!(
            verify_peer(&dns, &policy(), &peer(), "10.0.0.5".parse().unwrap()).await,
            Err(GateError::PtrOutsideSuffix { .. })
        ));
    }

    #[test]
    fn suffix_match_is_boundary_safe() {
        let p = PeerDnsPolicy { allowed_suffix: "nodes.company.com".into() };
        assert!(p.ptr_allowed("nodes.company.com"));
        assert!(p.ptr_allowed("a.b.nodes.company.com."));
        // suffix-lookalike outside the domain must NOT pass
        assert!(!p.ptr_allowed("nodes.company.com.evil.io"));
        assert!(!p.ptr_allowed("mynodes.company.com"));
    }
}
