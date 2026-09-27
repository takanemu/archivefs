//! Common virtual filesystem interfaces for archive readers.

use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

/// A reader that can be used by archive clients without knowing the archive format.
pub trait ReadSeek: Read + Seek + Send + Sync {}

impl<T> ReadSeek for T where T: Read + Seek + Send + Sync {}

/// Metadata describing one entry in an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    path: PathBuf,
    size: u64,
    directory: bool,
}

impl ArchiveEntry {
    /// Creates metadata for a regular file.
    pub fn file(path: impl Into<PathBuf>, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
            directory: false,
        }
    }

    /// Creates metadata for a directory.
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            size: 0,
            directory: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn is_directory(&self) -> bool {
        self.directory
    }
}

/// A format-independent view of an archive.
pub trait ArchiveVfs: Send + Sync {
    /// Returns the entries known to this archive.
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>>;

    /// Opens one regular-file entry for reading and seeking.
    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>>;
}

#[cfg(test)]
mod tests {
    use super::ArchiveEntry;

    #[test]
    fn archive_entry_preserves_file_metadata() {
        let entry = ArchiveEntry::file("docs/readme.txt", 42);

        assert_eq!(entry.path().to_str(), Some("docs/readme.txt"));
        assert_eq!(entry.size(), 42);
        assert!(!entry.is_directory());
    }

    #[test]
    fn directory_entry_has_no_file_size() {
        let entry = ArchiveEntry::directory("docs/");

        assert_eq!(entry.path().to_str(), Some("docs/"));
        assert_eq!(entry.size(), 0);
        assert!(entry.is_directory());
    }
}
