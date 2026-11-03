//! Version information for `SmartNet`.
use std::{borrow::Cow, sync::OnceLock};

use alloy_primitives::Bytes;
use alloy_rpc_types_engine::ClientCode;
use smartnet_db::ClientVersion;

/// The client code reported upstream (alloy's fixed `ClientCode::RH`)
pub const CLIENT_CODE: ClientCode = ClientCode::RH;

/// The single version source: repo-root `VERSION` file, baked at build time (see `build.rs`).
pub const VERSION: &str = env!("SMARTNET_VERSION");

/// Global static version metadata
static VERSION_METADATA: OnceLock<CliVersionConsts> = OnceLock::new();

/// Initialize the global version metadata.
pub fn try_init_version_metadata(
    metadata: CliVersionConsts,
) -> Result<(), CliVersionConsts> {
    VERSION_METADATA.set(metadata)
}

/// Constants for smartnet-cli-core
///
/// Global defaults can be set via [`try_init_version_metadata`].
#[derive(Debug, Default)]
pub struct CliVersionConsts {
    /// The human readable name of the client
    pub name_client: Cow<'static, str>,

    /// The version from the repo-root `VERSION` file.
    pub version: Cow<'static, str>,

    /// The full SHA of the latest commit.
    pub vergen_git_sha_long: Cow<'static, str>,

    /// The 8 character short SHA of the latest commit.
    pub vergen_git_sha: Cow<'static, str>,

    /// The build timestamp.
    pub vergen_build_timestamp: Cow<'static, str>,

    /// The target triple.
    pub vergen_cargo_target_triple: Cow<'static, str>,

    /// The build features.
    pub vergen_cargo_features: Cow<'static, str>,

    /// The short version information for `SmartNet`.
    pub short_version: Cow<'static, str>,

    /// The long version information for `SmartNet`.
    pub long_version: Cow<'static, str>,
    /// The build profile name.
    pub build_profile_name: Cow<'static, str>,

    /// extra data used for payload building
    pub extra_data: Cow<'static, str>,
}

/// The default extra data used for payload building.
///
/// - The version from the repo-root `VERSION` file
/// - The OS identifier
///
/// # Example
///
/// ```text
/// smartnet/v{major}.{minor}.{patch}/{OS}
/// ```
pub fn default_extra_data() -> String {
    format!("smartnet/v{}/{}", VERSION, std::env::consts::OS)
}

/// The default extra data in bytes.
/// See [`default_extra_data`].
pub fn default_extra_data_bytes() -> Bytes {
    Bytes::from(default_extra_data().as_bytes().to_vec())
}

/// The default client version accessing the database.
pub fn default_client_version() -> ClientVersion {
    let meta = version_metadata();
    ClientVersion {
        version: meta.version.to_string(),
        git_sha: meta.vergen_git_sha.to_string(),
        build_timestamp: meta.vergen_build_timestamp.to_string(),
    }
}

/// Get a reference to the global version metadata
pub fn version_metadata() -> &'static CliVersionConsts {
    VERSION_METADATA.get_or_init(default_version_metadata)
}

/// default version metadata using compile-time env! macros.
pub fn default_version_metadata() -> CliVersionConsts {
    CliVersionConsts {
        name_client: Cow::Borrowed("SmartNet"),
        version: Cow::Borrowed(VERSION),
        vergen_git_sha_long: Cow::Borrowed(env!("VERGEN_GIT_SHA")),
        vergen_git_sha: Cow::Borrowed(env!("VERGEN_GIT_SHA_SHORT")),
        vergen_build_timestamp: Cow::Borrowed(env!("VERGEN_BUILD_TIMESTAMP")),
        vergen_cargo_target_triple: Cow::Borrowed(env!("VERGEN_CARGO_TARGET_TRIPLE")),
        vergen_cargo_features: Cow::Borrowed(env!("VERGEN_CARGO_FEATURES")),
        short_version: Cow::Borrowed(env!("SMARTNET_SHORT_VERSION")),
        long_version: Cow::Owned(format!(
            "{}\n{}\n{}\n{}\n{}",
            env!("SMARTNET_LONG_VERSION_0"),
            env!("SMARTNET_LONG_VERSION_1"),
            env!("SMARTNET_LONG_VERSION_2"),
            env!("SMARTNET_LONG_VERSION_3"),
            env!("SMARTNET_LONG_VERSION_4"),
        )),

        build_profile_name: Cow::Borrowed(env!("SMARTNET_BUILD_PROFILE")),
        extra_data: Cow::Owned(default_extra_data()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_extra_data_less_32bytes() {
        let extra_data = default_extra_data();
        assert!(extra_data.len() <= 32, "extra data must be less than 32 bytes: {extra_data}")
    }
}
