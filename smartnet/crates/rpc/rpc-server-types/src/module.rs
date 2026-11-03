use std::{collections::HashSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize, Serializer};
use strum::{ParseError, VariantNames};

/// Describes the modules that should be installed.
///
/// # Example
///
/// Create a [`RpcModuleSelection`] from a selection.
///
/// ```
/// use smartnet_rpc_server_types::{RpcModuleSelection, SmartnetRpcModule};
/// let config: RpcModuleSelection = vec![SmartnetRpcModule::Eth].into();
/// ```
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub enum RpcModuleSelection {
    /// Use _all_ available modules.
    All,
    /// The default modules `eth`, `net`, `web3`
    #[default]
    Standard,
    /// Only use the configured modules.
    Selection(HashSet<SmartnetRpcModule>),
}

// === impl RpcModuleSelection ===

impl RpcModuleSelection {
    /// The standard modules to instantiate by default `eth`, `net`, `web3`
    pub const STANDARD_MODULES: [SmartnetRpcModule; 3] =
        [SmartnetRpcModule::Eth, SmartnetRpcModule::Net, SmartnetRpcModule::Web3];
    /// Modules requested by default (default `--http.api` / `--ws.api` when omitted).
    ///
    /// This is the production whitelist: `eth`, `net`, `web3`, `rpc`, plus `txpool`, which the
    /// registry further gates on the `RPC_MEMPOOL` config (default off). Every other namespace
    /// (`admin`, `debug`, `trace`, `ots`, `wem`/`smartnet`, …) is hard-blocked at the registry
    /// regardless of this list or any `--http.api all` — see [`SmartnetRpcModule::is_allowed`].
    pub const ENABLED_MODULES: [SmartnetRpcModule; 5] = [
        SmartnetRpcModule::Eth,
        SmartnetRpcModule::Net,
        SmartnetRpcModule::Web3,
        SmartnetRpcModule::Rpc,
        SmartnetRpcModule::Txpool,
    ];

    /// Returns a selection of [`SmartnetRpcModule`] with all [`SmartnetRpcModule::all_variants`].
    pub fn all_modules() -> HashSet<SmartnetRpcModule> {
        SmartnetRpcModule::modules().into_iter().collect()
    }

    /// Returns the [`RpcModuleSelection::STANDARD_MODULES`] as a selection.
    pub fn standard_modules() -> HashSet<SmartnetRpcModule> {
        HashSet::from(Self::STANDARD_MODULES)
    }

    /// Default HTTP/WS API module set for `SmartNet` (production whitelist: eth, net, web3, rpc,
    /// txpool). The registry drops `txpool` unless `RPC_MEMPOOL` is enabled.
    pub fn default_http_ws_modules() -> HashSet<SmartnetRpcModule> {
        HashSet::from(Self::ENABLED_MODULES)
    }

    /// Creates a new _unique_ [`RpcModuleSelection::Selection`] from the given items.
    ///
    /// # Note
    ///
    /// This will dedupe the selection and remove duplicates while preserving the order.
    ///
    /// # Example
    ///
    /// Create a selection from the [`SmartnetRpcModule`] string identifiers
    ///
    /// ```
    /// use smartnet_rpc_server_types::{RpcModuleSelection, SmartnetRpcModule};
    /// let selection = vec!["eth", "admin"];
    /// let config = RpcModuleSelection::try_from_selection(selection).unwrap();
    /// assert_eq!(
    ///     config,
    ///     RpcModuleSelection::from([SmartnetRpcModule::Eth, SmartnetRpcModule::Admin])
    /// );
    /// ```
    ///
    /// Create a unique selection from the [`SmartnetRpcModule`] string identifiers
    ///
    /// ```
    /// use smartnet_rpc_server_types::{RpcModuleSelection, SmartnetRpcModule};
    /// let selection = vec!["eth", "admin", "eth", "admin"];
    /// let config = RpcModuleSelection::try_from_selection(selection).unwrap();
    /// assert_eq!(
    ///     config,
    ///     RpcModuleSelection::from([SmartnetRpcModule::Eth, SmartnetRpcModule::Admin])
    /// );
    /// ```
    pub fn try_from_selection<I, T>(selection: I) -> Result<Self, T::Error>
    where
        I: IntoIterator<Item = T>,
        T: TryInto<SmartnetRpcModule>,
    {
        selection.into_iter().map(TryInto::try_into).collect()
    }

    /// Returns the number of modules in the selection
    pub fn len(&self) -> usize {
        match self {
            Self::All => SmartnetRpcModule::variant_count(),
            Self::Standard => Self::STANDARD_MODULES.len(),
            Self::Selection(s) => s.len(),
        }
    }

    /// Returns true if no selection is configured
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Selection(sel) => sel.is_empty(),
            _ => false,
        }
    }

    /// Returns true if all modules are selected
    pub const fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// Returns an iterator over all configured [`SmartnetRpcModule`]
    pub fn iter_selection(&self) -> Box<dyn Iterator<Item = SmartnetRpcModule> + '_> {
        match self {
            Self::All => Box::new(SmartnetRpcModule::modules().into_iter()),
            Self::Standard => Box::new(Self::STANDARD_MODULES.iter().cloned()),
            Self::Selection(s) => Box::new(s.iter().cloned()),
        }
    }

    /// Clones the set of configured [`SmartnetRpcModule`].
    pub fn to_selection(&self) -> HashSet<SmartnetRpcModule> {
        match self {
            Self::All => Self::all_modules(),
            Self::Standard => Self::standard_modules(),
            Self::Selection(s) => s.clone(),
        }
    }

    /// Converts the selection into a [`HashSet`].
    pub fn into_selection(self) -> HashSet<SmartnetRpcModule> {
        match self {
            Self::All => Self::all_modules(),
            Self::Standard => Self::standard_modules(),
            Self::Selection(s) => s,
        }
    }

    /// Returns a new selection containing only the namespaces the production allowlist permits
    /// ([`SmartnetRpcModule::is_allowed`]); `txpool` is kept only when `txpool_enabled`.
    ///
    /// `All` and `Standard` are rewritten as explicit selections so that downstream checks like
    /// [`Self::contains`] cannot report a blocked namespace as configured.
    pub fn filter_allowed(&self, txpool_enabled: bool) -> Self {
        Self::Selection(self.iter_selection().filter(|m| m.is_allowed(txpool_enabled)).collect())
    }

    /// Returns true if both selections are identical.
    pub fn are_identical(http: Option<&Self>, ws: Option<&Self>) -> bool {
        match (http, ws) {
            // Shortcut for common case to avoid iterating later
            (Some(Self::All), Some(other)) | (Some(other), Some(Self::All)) => {
                other.len() == SmartnetRpcModule::variant_count()
            }

            // If either side is disabled, then the other must be empty
            (Some(some), None) | (None, Some(some)) => some.is_empty(),

            (Some(http), Some(ws)) => http.to_selection() == ws.to_selection(),
            (None, None) => true,
        }
    }

    /// Returns true if the selection contains the given module.
    pub fn contains(&self, module: &SmartnetRpcModule) -> bool {
        match self {
            Self::All => true,
            Self::Standard => Self::STANDARD_MODULES.contains(module),
            Self::Selection(s) => s.contains(module),
        }
    }

    /// Adds a module to the selection.
    ///
    /// If the selection is `All`, this is a no-op.
    /// Otherwise, converts to a `Selection` and adds the module.
    pub fn push(&mut self, module: SmartnetRpcModule) {
        if !self.is_all() {
            let mut modules = self.to_selection();
            modules.insert(module);
            *self = Self::Selection(modules);
        }
    }

    /// Returns a new selection with the given module added.
    ///
    /// If the selection is `All`, returns `All`.
    /// Otherwise, converts to a `Selection` and adds the module.
    pub fn append(self, module: SmartnetRpcModule) -> Self {
        if self.is_all() {
            Self::All
        } else {
            let mut modules = self.into_selection();
            modules.insert(module);
            Self::Selection(modules)
        }
    }

    /// Extends the selection with modules from an iterator.
    ///
    /// If the selection is `All`, this is a no-op.
    /// Otherwise, converts to a `Selection` and adds the modules.
    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = SmartnetRpcModule>,
    {
        if !self.is_all() {
            let mut modules = self.to_selection();
            modules.extend(iter);
            *self = Self::Selection(modules);
        }
    }

    /// Returns a new selection with modules from an iterator added.
    ///
    /// If the selection is `All`, returns `All`.
    /// Otherwise, converts to a `Selection` and adds the modules.
    pub fn extended<I>(self, iter: I) -> Self
    where
        I: IntoIterator<Item = SmartnetRpcModule>,
    {
        if self.is_all() {
            Self::All
        } else {
            let mut modules = self.into_selection();
            modules.extend(iter);
            Self::Selection(modules)
        }
    }
}

impl From<&HashSet<SmartnetRpcModule>> for RpcModuleSelection {
    fn from(s: &HashSet<SmartnetRpcModule>) -> Self {
        Self::from(s.clone())
    }
}

impl From<HashSet<SmartnetRpcModule>> for RpcModuleSelection {
    fn from(s: HashSet<SmartnetRpcModule>) -> Self {
        Self::Selection(s)
    }
}

impl From<&[SmartnetRpcModule]> for RpcModuleSelection {
    fn from(s: &[SmartnetRpcModule]) -> Self {
        Self::Selection(s.iter().cloned().collect())
    }
}

impl From<Vec<SmartnetRpcModule>> for RpcModuleSelection {
    fn from(s: Vec<SmartnetRpcModule>) -> Self {
        Self::Selection(s.into_iter().collect())
    }
}

impl<const N: usize> From<[SmartnetRpcModule; N]> for RpcModuleSelection {
    fn from(s: [SmartnetRpcModule; N]) -> Self {
        Self::Selection(s.into_iter().collect())
    }
}

impl<'a> FromIterator<&'a SmartnetRpcModule> for RpcModuleSelection {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = &'a SmartnetRpcModule>,
    {
        iter.into_iter().cloned().collect()
    }
}

impl FromIterator<SmartnetRpcModule> for RpcModuleSelection {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = SmartnetRpcModule>,
    {
        Self::Selection(iter.into_iter().collect())
    }
}

impl FromStr for RpcModuleSelection {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(Self::Selection(Default::default()));
        }
        let mut modules = s.split(',').map(str::trim).peekable();
        let first = modules.peek().copied().ok_or(ParseError::VariantNotFound)?;
        // We convert to lowercase to make the comparison case-insensitive
        //
        // This is a way to allow typing "all" and "ALL" and "All" and "aLl" etc.
        match first.to_lowercase().as_str() {
            "all" => Ok(Self::All),
            "none" => Ok(Self::Selection(Default::default())),
            _ => Self::try_from_selection(modules),
        }
    }
}

impl fmt::Display for RpcModuleSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}]",
            self.iter_selection().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")
        )
    }
}

/// Represents RPC modules supported by the `SmartNet` node
#[derive(Debug, Clone, Eq, PartialEq, Hash, VariantNames, Deserialize)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "kebab-case")]
pub enum SmartnetRpcModule {
    /// `admin_` module
    Admin,
    /// `debug_` module
    Debug,
    /// `eth_` module
    Eth,
    /// `net_` module
    Net,
    /// `trace_` module
    Trace,
    /// `txpool_` module
    Txpool,
    /// `web3_` module
    Web3,
    /// `rpc_` module
    Rpc,
    /// `wem_` module (operator-compat; dual-published with `smartnet_` when registered)
    Wem,
    /// `smartnet_` module (canonical; same handlers as `wem_` when registered)
    Smartnet,
    /// `ots_` module
    Ots,
    /// `flashbots_` module
    Flashbots,
    /// `miner_` module
    Miner,
    /// `mev_` module
    Mev,
    /// `testing_` module
    Testing,
    /// Custom RPC module not part of the standard set
    #[strum(default)]
    #[serde(untagged)]
    Other(String),
}

// === impl SmartnetRpcModule ===

impl SmartnetRpcModule {
    /// All standard variants (excludes Other)
    const STANDARD_VARIANTS: &'static [Self] = &[
        Self::Admin,
        Self::Debug,
        Self::Eth,
        Self::Net,
        Self::Trace,
        Self::Txpool,
        Self::Web3,
        Self::Rpc,
        Self::Wem,
        Self::Smartnet,
        Self::Ots,
        Self::Flashbots,
        Self::Miner,
        Self::Mev,
        Self::Testing,
    ];

    /// Returns the number of standard variants (excludes Other)
    pub const fn variant_count() -> usize {
        Self::STANDARD_VARIANTS.len()
    }

    /// Returns all variant names including Other (for parsing)
    pub const fn all_variant_names() -> &'static [&'static str] {
        <Self as VariantNames>::VARIANTS
    }

    /// Returns standard variant names (excludes "other") for CLI display
    pub fn standard_variant_names() -> impl Iterator<Item = &'static str> {
        <Self as VariantNames>::VARIANTS.iter().copied().filter(|&name| name != "other")
    }

    /// Returns all standard variants (excludes Other)
    pub const fn all_variants() -> &'static [Self] {
        Self::STANDARD_VARIANTS
    }

    /// Returns iterator over standard modules only
    pub fn modules() -> impl IntoIterator<Item = Self> + Clone {
        Self::STANDARD_VARIANTS.iter().cloned()
    }

    /// Returns the string representation of the module.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Other(s) => s.as_str(),
            _ => self.as_ref(), // Uses AsRefStr trait
        }
    }

    /// Returns true if this is an `Other` variant.
    pub const fn is_other(&self) -> bool {
        matches!(self, Self::Other(_))
    }

    /// Production allowlist: whether this namespace may ever be served on the public HTTP/WS
    /// transports.
    ///
    /// `eth`/`net`/`web3`/`rpc` are always allowed. `txpool` is allowed only when the operator
    /// sets `RPC_MEMPOOL` (a temporary sync-debugging aid; information leak + `DoS` surface).
    /// Every other namespace — `admin`, `debug`, `trace`, `ots`, `wem`/`smartnet`, `flashbots`,
    /// `miner`, `mev`, `testing`, and any unknown `Other` — is hard-blocked at the registry, so
    /// it is never registered even if `--http.api all` requests it.
    pub const fn is_allowed(&self, txpool_enabled: bool) -> bool {
        match self {
            Self::Eth | Self::Net | Self::Web3 | Self::Rpc => true,
            Self::Txpool => txpool_enabled,
            _ => false,
        }
    }
}

impl AsRef<str> for SmartnetRpcModule {
    fn as_ref(&self) -> &str {
        match self {
            Self::Other(s) => s.as_str(),
            // For standard variants, use the derive-generated static strings
            Self::Admin => "admin",
            Self::Debug => "debug",
            Self::Eth => "eth",
            Self::Net => "net",
            Self::Trace => "trace",
            Self::Txpool => "txpool",
            Self::Web3 => "web3",
            Self::Rpc => "rpc",
            Self::Wem => "wem",
            Self::Smartnet => "smartnet",
            Self::Ots => "ots",
            Self::Flashbots => "flashbots",
            Self::Miner => "miner",
            Self::Mev => "mev",
            Self::Testing => "testing",
        }
    }
}

impl FromStr for SmartnetRpcModule {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "admin" => Self::Admin,
            "debug" => Self::Debug,
            "eth" => Self::Eth,
            "net" => Self::Net,
            "trace" => Self::Trace,
            "txpool" => Self::Txpool,
            "web3" => Self::Web3,
            "rpc" => Self::Rpc,
            "wem" => Self::Wem,
            "smartnet" => Self::Smartnet,
            "ots" => Self::Ots,
            "flashbots" => Self::Flashbots,
            "miner" => Self::Miner,
            "mev" => Self::Mev,
            "testing" => Self::Testing,
            // Any unknown module becomes Other
            other => Self::Other(other.to_string()),
        })
    }
}

impl TryFrom<&str> for SmartnetRpcModule {
    type Error = ParseError;
    fn try_from(s: &str) -> Result<Self, <Self as TryFrom<&str>>::Error> {
        FromStr::from_str(s)
    }
}

impl fmt::Display for SmartnetRpcModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_ref())
    }
}

impl Serialize for SmartnetRpcModule {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

/// Trait for validating RPC module selections.
///
/// This allows customizing how RPC module names are validated when parsing
/// CLI arguments or configuration.
pub trait RpcModuleValidator: Clone + Send + Sync + 'static {
    /// Parse and validate an RPC module selection string.
    fn parse_selection(s: &str) -> Result<RpcModuleSelection, String>;

    /// Validates RPC module selection that was already parsed.
    ///
    /// This is used to validate modules that were parsed as `Other` variants
    /// to ensure they meet the validation rules of the specific implementation.
    fn validate_selection(modules: &RpcModuleSelection, arg_name: &str) -> Result<(), String> {
        // Re-validate the modules using the parser's validator
        // This is necessary because the clap value parser accepts any input
        // and we need to validate according to the specific parser's rules
        let RpcModuleSelection::Selection(module_set) = modules else {
            // All or Standard variants are always valid
            return Ok(());
        };

        for module in module_set {
            let SmartnetRpcModule::Other(name) = module else {
                // Standard modules are always valid
                continue;
            };

            // Try to parse and validate using the configured validator
            // This will check for typos and other validation rules
            Self::parse_selection(name)
                .map_err(|e| format!("Invalid RPC module '{name}' in {arg_name}: {e}"))?;
        }

        Ok(())
    }
}

/// Default validator that rejects unknown module names.
///
/// This validator only accepts known RPC module names.
#[derive(Debug, Clone, Copy)]
pub struct DefaultRpcModuleValidator;

impl RpcModuleValidator for DefaultRpcModuleValidator {
    fn parse_selection(s: &str) -> Result<RpcModuleSelection, String> {
        if s.eq_ignore_ascii_case("all") {
            return Err(
                "The `all` RPC module selection is disabled; allowed modules: eth, net, web3, rpc, txpool".to_string(),
            );
        }

        // First try standard parsing
        let selection = RpcModuleSelection::from_str(s)
            .map_err(|e| format!("Failed to parse RPC modules: {}", e))?;

        // Validate each module in the selection
        if let RpcModuleSelection::Selection(modules) = &selection {
            for module in modules {
                if let SmartnetRpcModule::Other(name) = module {
                    return Err(format!("Unknown RPC module: '{}'", name));
                }
                if !RpcModuleSelection::ENABLED_MODULES.contains(module) {
                    return Err(format!(
                        "RPC module '{}' is disabled; allowed modules: eth, net, web3, rpc, txpool",
                        module
                    ));
                }
            }
        }

        Ok(selection)
    }
}

/// Lenient validator that accepts any module name without validation.
///
/// This validator accepts any module name, including unknown ones.
#[derive(Debug, Clone, Copy)]
pub struct LenientRpcModuleValidator;

impl RpcModuleValidator for LenientRpcModuleValidator {
    fn parse_selection(s: &str) -> Result<RpcModuleSelection, String> {
        RpcModuleSelection::from_str(s).map_err(|e| format!("Failed to parse RPC modules: {}", e))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_default_validator_accepts_standard_modules() {
        // Default fork allowlist: eth, net, web3, rpc, txpool
        let result = DefaultRpcModuleValidator::parse_selection("eth,net,web3,rpc,txpool");
        assert!(result.is_ok());

        let selection = result.unwrap();
        assert!(matches!(selection, RpcModuleSelection::Selection(_)));
    }

    #[test]
    fn test_default_validator_rejects_unknown_modules() {
        // Should reject unknown module names
        let result = DefaultRpcModuleValidator::parse_selection("eth,mycustom");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown RPC module: 'mycustom'"));

        let result = DefaultRpcModuleValidator::parse_selection("unknownmodule");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown RPC module: 'unknownmodule'"));

        let result = DefaultRpcModuleValidator::parse_selection("eth,net,xyz123");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown RPC module: 'xyz123'"));
    }

    #[test]
    fn test_default_validator_all_selection() {
        // Fork disables bare `all` (debloated surface)
        let result = DefaultRpcModuleValidator::parse_selection("all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("disabled"));
    }

    #[test]
    fn test_default_validator_none_selection() {
        // Should accept "none" selection
        let result = DefaultRpcModuleValidator::parse_selection("none");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), RpcModuleSelection::Selection(Default::default()));
    }

    #[test]
    fn test_lenient_validator_accepts_unknown_modules() {
        // Lenient validator should accept any module name without validation
        let result = LenientRpcModuleValidator::parse_selection("eht,adimn,xyz123,customrpc");
        assert!(result.is_ok());

        let selection = result.unwrap();
        if let RpcModuleSelection::Selection(modules) = selection {
            assert!(modules.contains(&SmartnetRpcModule::Other("eht".to_string())));
            assert!(modules.contains(&SmartnetRpcModule::Other("adimn".to_string())));
            assert!(modules.contains(&SmartnetRpcModule::Other("xyz123".to_string())));
            assert!(modules.contains(&SmartnetRpcModule::Other("customrpc".to_string())));
        } else {
            panic!("Expected Selection variant");
        }
    }

    #[test]
    fn test_default_validator_mixed_standard_and_custom() {
        // Unknown names become `Other` and are rejected before allowlist checks
        let result = DefaultRpcModuleValidator::parse_selection("eth,rpc,mycustom");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown RPC module: 'mycustom'"));
    }
}
