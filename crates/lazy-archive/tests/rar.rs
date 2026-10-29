use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroUsize;

use lazy_archive::{EntryCache, RarArchiveVfs};
use rars::{ArchiveVersion, Builder};
use tempfile::NamedTempFile;
use vfs_archive::ArchiveVfs;

fn test_archive() -> NamedTempFile {
    let archive = NamedTempFile::new().expect("create RAR archive");
    let mut builder = Builder::new(ArchiveVersion::Rar50).solid(true);
    builder
        .add_bytes(
            b"first.txt".to_vec(),
            b"first solid entry".to_vec(),
            None,
            None,
        )
        .expect("add first RAR entry");
    builder
        .add_bytes(b"readme.txt".to_vec(), b"rar stream".to_vec(), None, None)
        .expect("add readme RAR entry");
    builder
        .write_to_path(archive.path(), None)
        .expect("write RAR archive");
    archive
}

#[test]
fn lists_and_reads_solid_rar_entries() {
    let archive = test_archive();
    let vfs = RarArchiveVfs::open(archive.path()).expect("open RAR archive");

    let entries = vfs.entries().expect("list RAR entries");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].path().to_str(), Some("readme.txt"));
    assert_eq!(entries[1].size(), 10);

    let mut reader = vfs
        .open(std::path::Path::new("readme.txt"))
        .expect("open RAR entry");
    reader.seek(SeekFrom::Start(1)).expect("seek from start");
    reader
        .seek(SeekFrom::Current(2))
        .expect("seek from current");
    reader.seek(SeekFrom::End(-6)).expect("seek from end");
    let mut contents = String::new();
    reader
        .read_to_string(&mut contents)
        .expect("read RAR entry");

    assert_eq!(contents, "stream");
}

#[test]
fn reuses_cached_rar_entry() {
    let archive = test_archive();
    let vfs = RarArchiveVfs::open(archive.path()).expect("open RAR archive");
    let path = std::path::Path::new("readme.txt");

    vfs.open(path).expect("open RAR entry first time");
    assert_eq!(
        vfs.cache_len().expect("read cache length"),
        2,
        "one RAR solid extraction must cache all regular entries"
    );
    vfs.open(path).expect("open RAR entry second time");
    vfs.open(std::path::Path::new("first.txt"))
        .expect("open another RAR entry from the materialized solid stream");
    assert_eq!(vfs.cache_len().expect("read cache length"), 2);
}

#[test]
fn corrupt_rar_returns_an_error() {
    let mut archive = NamedTempFile::new().expect("create corrupt RAR");
    std::io::Write::write_all(&mut archive, b"not a RAR archive").expect("write corrupt RAR");

    let result = RarArchiveVfs::open(archive.path());

    assert!(result.is_err(), "corrupt RAR must be rejected");
}

#[test]
fn streams_large_rar_entry_to_disk_backed_cache() {
    let archive = NamedTempFile::new().expect("create RAR archive");
    let contents = vec![b'x'; 4096];
    let mut builder = Builder::new(ArchiveVersion::Rar50).solid(true);
    builder
        .add_bytes(b"large.bin".to_vec(), contents.clone(), None, None)
        .expect("add large RAR entry");
    builder
        .write_to_path(archive.path(), None)
        .expect("write large RAR archive");
    let cache = EntryCache::with_memory_threshold(
        NonZeroUsize::new(2).expect("cache capacity is non-zero"),
        1,
    );
    let vfs = RarArchiveVfs::with_cache(archive.path(), cache).expect("open large RAR archive");

    let mut reader = vfs
        .open(std::path::Path::new("large.bin"))
        .expect("open large RAR entry");
    let mut actual = Vec::new();
    reader
        .read_to_end(&mut actual)
        .expect("read large RAR entry");

    assert_eq!(actual, contents);
    assert_eq!(vfs.cache_len().expect("read cache length"), 1);
}
