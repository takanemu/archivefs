use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;

use flate2::{Compression, write::GzEncoder};
use lazy_archive::{EntryCache, TarGzArchiveVfs};
use tar::{Builder, Header};
use vfs_archive::ArchiveVfs;

fn test_archive() -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("create temporary archive");
    let encoder = GzEncoder::new(
        file.reopen().expect("reopen temporary archive"),
        Compression::default(),
    );
    let mut builder = Builder::new(encoder);
    let contents = b"solid archive";
    let mut header = Header::new_gnu();
    header.set_path("docs/readme.txt").expect("set path");
    header.set_size(contents.len() as u64);
    header.set_cksum();
    builder.append(&header, &contents[..]).expect("append file");
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip");
    file
}

#[test]
fn lists_and_reads_tar_gz_entries() {
    let archive = test_archive();
    let vfs = TarGzArchiveVfs::with_cache(
        archive.path(),
        EntryCache::new(NonZeroUsize::new(2).expect("non-zero capacity")),
    )
    .expect("open archive");

    let entries = vfs.entries().expect("list entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path().to_str(), Some("docs/readme.txt"));
    assert_eq!(entries[0].size(), 13);

    let mut reader = vfs
        .open(std::path::Path::new("docs/readme.txt"))
        .expect("open entry");
    reader.seek(SeekFrom::Start(2)).expect("seek from start");
    reader
        .seek(SeekFrom::Current(4))
        .expect("seek from current");
    reader.seek(SeekFrom::End(-7)).expect("seek from end");
    let mut suffix = String::new();
    reader.read_to_string(&mut suffix).expect("read suffix");
    assert_eq!(suffix, "archive");
}

#[test]
fn missing_entry_returns_not_found_without_panicking() {
    let archive = test_archive();
    let vfs = TarGzArchiveVfs::open(archive.path()).expect("open archive");

    let result = vfs.open(std::path::Path::new("missing.txt"));
    let error = match result {
        Ok(_) => panic!("missing entry must fail"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn empty_archive_has_no_entries() {
    let file = tempfile::NamedTempFile::new().expect("create temporary archive");
    let encoder = GzEncoder::new(
        file.reopen().expect("reopen temporary archive"),
        Compression::default(),
    );
    Builder::new(encoder)
        .into_inner()
        .expect("finish empty tar")
        .finish()
        .expect("finish empty gzip");
    let vfs = TarGzArchiveVfs::open(file.path()).expect("open empty archive");

    assert!(vfs.entries().expect("list empty archive").is_empty());
}

#[test]
fn corrupt_archive_returns_an_error_without_panicking() {
    let mut file = tempfile::NamedTempFile::new().expect("create temporary archive");
    file.write_all(b"not a gzip archive")
        .expect("write corrupt archive");
    let vfs = TarGzArchiveVfs::open(file.path()).expect("open archive path");

    let result = vfs.entries();

    assert!(result.is_err(), "corrupt TAR.GZ must be rejected");
}

#[test]
fn reuses_cached_entry() {
    let archive = test_archive();
    let vfs = TarGzArchiveVfs::open(archive.path()).expect("open archive");
    let path = std::path::Path::new("docs/readme.txt");

    vfs.open(path).expect("open first time");
    assert_eq!(vfs.cache_len().expect("read cache length"), 1);
    vfs.open(path).expect("open second time");
    assert_eq!(vfs.cache_len().expect("read cache length"), 1);
}
