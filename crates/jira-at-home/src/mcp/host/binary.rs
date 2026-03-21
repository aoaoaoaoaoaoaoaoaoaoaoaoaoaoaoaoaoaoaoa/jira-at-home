use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::mcp::protocol::BinaryFingerprint;

pub(crate) struct BinaryRuntime {
    pub(crate) path: PathBuf,
    startup_fingerprint: BinaryFingerprint,
    pub(crate) launch_path_stable: bool,
}

impl BinaryRuntime {
    pub(crate) fn new(path: PathBuf) -> io::Result<Self> {
        let startup_fingerprint = fingerprint_binary(&path)?;
        Ok(Self {
            launch_path_stable: !path
                .components()
                .any(|component| component.as_os_str().to_string_lossy() == "target"),
            path,
            startup_fingerprint,
        })
    }

    pub(crate) fn rollout_pending(&self) -> io::Result<bool> {
        Ok(fingerprint_binary(&self.path)? != self.startup_fingerprint)
    }
}

fn fingerprint_binary(path: &Path) -> io::Result<BinaryFingerprint> {
    let metadata = fs::metadata(path)?;
    let modified_unix_nanos = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| io::Error::other(format!("invalid binary mtime: {error}")))?
        .as_nanos();
    Ok(BinaryFingerprint {
        length_bytes: metadata.len(),
        modified_unix_nanos,
    })
}
