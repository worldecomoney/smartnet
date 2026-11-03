//! Utility functions for node startup and shutdown, for example path parsing.

use std::path::PathBuf;

/// Parses a user-specified path into a [`PathBuf`].
pub fn parse_path(value: &str) -> PathBuf {
    PathBuf::from(value)
}
