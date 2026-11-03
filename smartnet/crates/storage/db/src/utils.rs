//! Utils crate for `db`.

use std::path::Path;

/// Check if a db is empty. It does not provide any information on the
/// validity of the data in it. We consider a database as non empty when it's a non empty directory.
pub fn is_database_empty<P: AsRef<Path>>(path: P) -> bool {
    let path = path.as_ref();

    if !path.exists() {
        true
    } else if path.is_file() {
        false
    } else if let Ok(mut dir) = path.read_dir() {
        // Check if directory has any entries without counting all of them
        dir.next().is_none()
    } else {
        true
    }
}
