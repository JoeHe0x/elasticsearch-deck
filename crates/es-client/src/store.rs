//! Shared file handling for the JSON files under the config directory.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Writes through a temporary file and renames it over `path`, so a crash never leaves half a file.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Config(e.to_string()))?;
    }
    let tmp = sibling(path, ".tmp");
    std::fs::write(&tmp, text).map_err(|e| Error::Config(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::Config(e.to_string()))
}

/// Reads `path` as JSON. A missing file is `None`. A file that does not parse is renamed to
/// `<name>.bak` and reported, so the empty list the caller falls back to cannot overwrite it on the next save.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Config(e.to_string())),
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        let bak = sibling(path, ".bak");
        let kept = match std::fs::rename(path, &bak) {
            Ok(()) => format!("kept as {}", bak.display()),
            Err(_) => "could not be moved aside".to_string(),
        };
        Error::Config(format!("{} is unreadable ({e}); {kept}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_corrupt_file_is_kept_aside_and_a_missing_one_is_none() {
        let dir = std::env::temp_dir().join(format!("esdeck-store-{}", std::process::id()));
        let path = dir.join("x.json");
        assert_eq!(read_json::<Vec<u8>>(&path).unwrap(), None);
        write_atomic(&path, "[1,2").unwrap();
        assert!(read_json::<Vec<u8>>(&path).is_err());
        assert!(!path.exists(), "the corrupt file must not stay in place");
        assert_eq!(
            std::fs::read_to_string(sibling(&path, ".bak")).unwrap(),
            "[1,2"
        );
        write_atomic(&path, "[1,2]").unwrap();
        assert_eq!(read_json::<Vec<u8>>(&path).unwrap(), Some(vec![1, 2]));
        assert!(!sibling(&path, ".tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
