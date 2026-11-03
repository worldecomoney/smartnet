#![allow(missing_docs)]

use std::{env, error::Error, process::Command};
use vergen::{BuildBuilder, CargoBuilder, Emitter};

/// Placeholder used when building from a source tree without git metadata.
const NO_GIT_SHA: &str = "0000000000000000000000000000000000000000";

/// Runs `git <args>` in the crate directory and returns the trimmed stdout, if it succeeded.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(stdout.trim().to_string())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut emitter = Emitter::default();

    let build_builder = BuildBuilder::default().build_timestamp(true).build()?;

    emitter.add_instructions(&build_builder)?;

    let cargo_builder = CargoBuilder::default().features(true).target_triple(true).build()?;

    emitter.add_instructions(&cargo_builder)?;

    emitter.emit_and_set()?;

    // Git metadata is read through the `git` CLI instead of a bindings crate, so the build
    // does not link any C library.
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| NO_GIT_SHA.to_string());
    let sha_short = &sha[0..7];
    // Equivalent to the previous `git describe` invocation (`--always --dirty`, no `--tags`):
    // if not on a tag: v0.2.0-beta.3-82-g1939939b
    // if on a tag: v0.2.0-beta.3
    let describe =
        git(&["describe", "--always", "--dirty"]).unwrap_or_else(|| sha_short.to_string());
    let is_dirty =
        git(&["status", "--porcelain", "--untracked-files=normal"]).is_some_and(|s| !s.is_empty());
    let not_on_tag = describe.ends_with(&format!("-g{sha_short}"));

    println!("cargo:rustc-env=VERGEN_GIT_SHA={sha}");
    println!("cargo:rustc-env=VERGEN_GIT_DIRTY={}", if is_dirty { "true" } else { "false" });

    // Re-run this script whenever the checked out commit changes.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let head = format!("{git_dir}/HEAD");
        println!("cargo:rerun-if-changed={head}");
        if let Some(git_ref) = std::fs::read_to_string(&head)
            .ok()
            .and_then(|h| h.trim().strip_prefix("ref: ").map(str::to_string))
        {
            let ref_path = format!("{git_dir}/{git_ref}");
            if std::path::Path::new(&ref_path).exists() {
                println!("cargo:rerun-if-changed={ref_path}");
            } else {
                println!("cargo:rerun-if-changed={git_dir}/packed-refs");
            }
        }
    }
    let version_suffix = if is_dirty || not_on_tag { "-dev" } else { "" };

    // Set short SHA
    println!("cargo:rustc-env=VERGEN_GIT_SHA_SHORT={}", &sha[..8]);

    // Set the build profile
    let out_dir = env::var("OUT_DIR").unwrap();
    let profile = out_dir.rsplit(std::path::MAIN_SEPARATOR).nth(3).unwrap();
    println!("cargo:rustc-env=SMARTNET_BUILD_PROFILE={profile}");

    // The single version source: the repo-root VERSION file (operator-editable).
    let version_path =
        std::path::PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../../../../VERSION");
    println!("cargo:rerun-if-changed={}", version_path.display());
    let version = std::fs::read_to_string(&version_path)
        .map_err(|err| format!("cannot read {}: {err}", version_path.display()))?
        .trim()
        .to_string();
    if version.is_empty() {
        return Err(format!("{} is empty", version_path.display()).into());
    }
    println!("cargo:rustc-env=SMARTNET_VERSION={version}");

    // The short version information for SmartNet.
    // - The version from the repo-root VERSION file
    // - The short SHA of the latest commit.
    // Example: 3.7.2 (defa64b2)
    println!("cargo:rustc-env=SMARTNET_SHORT_VERSION={version}{version_suffix} ({sha_short})");

    // LONG_VERSION
    // The long version information for SmartNet.
    //
    // - The version from the repo-root VERSION file + version suffix (if any)
    // - The full SHA of the latest commit
    // - The build datetime
    // - The build features
    // - The build profile
    //
    // Example:
    //
    // ```text
    // Version: 0.1.0
    // Commit SHA: defa64b2
    // Build Timestamp: 2023-05-19T01:47:19.815651705Z
    // Build Features: default
    // Build Profile: maxperf
    // ```
    println!("cargo:rustc-env=SMARTNET_LONG_VERSION_0=Version: {version}{version_suffix}");
    println!("cargo:rustc-env=SMARTNET_LONG_VERSION_1=Commit SHA: {sha}");
    println!(
        "cargo:rustc-env=SMARTNET_LONG_VERSION_2=Build Timestamp: {}",
        env::var("VERGEN_BUILD_TIMESTAMP")?
    );
    println!(
        "cargo:rustc-env=SMARTNET_LONG_VERSION_3=Build Features: {}",
        env::var("VERGEN_CARGO_FEATURES")?
    );
    println!("cargo:rustc-env=SMARTNET_LONG_VERSION_4=Build Profile: {profile}");

    Ok(())
}
