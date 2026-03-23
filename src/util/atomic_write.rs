use std::fs;
use std::path::Path;

/// Atomically write content to a file via tmp + rename.
pub fn write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
