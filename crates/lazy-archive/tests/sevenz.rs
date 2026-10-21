use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use lazy_archive::SevenZArchiveVfs;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};
use tempfile::NamedTempFile;
use vfs_archive::ArchiveVfs;

fn test_archive() -> NamedTempFile {
    let archive = NamedTempFile::new().expect("create 7z destination");
    let mut writer = ArchiveWriter::create(archive.path()).expect("create 7z writer");
    writer
        .push_archive_entries(
            vec![
                ArchiveEntry::new_file("first.txt"),
                ArchiveEntry::new_file("readme.txt"),
            ],
            vec![
                SourceReader::new(Cursor::new(b"first solid entry".to_vec())),
                SourceReader::new(Cursor::new(b"seven z stream".to_vec())),
            ],
        )
        .expect("write solid block");
    writer.finish().expect("finish solid archive");

    let parsed = sevenz_rust2::Archive::open(archive.path()).expect("parse test archive");
    assert!(parsed.is_solid, "fixture must be a solid 7z archive");
    assert_eq!(
        parsed.blocks.len(),
        1,
        "fixture entries must share one solid block"
    );
    archive
}

#[test]
fn lists_and_reads_sevenz_entries() {
    let archive = test_archive();
    let vfs = SevenZArchiveVfs::open(archive.path()).expect("open 7z archive");

    let entries = vfs.entries().expect("list 7z entries");
    assert!(
        entries
            .iter()
            .any(|entry| entry.path().ends_with("readme.txt")),
        "7z entry list must contain readme.txt"
    );
    let path = entries
        .iter()
        .find(|entry| entry.path().ends_with("readme.txt"))
        .expect("find readme entry")
        .path()
        .to_owned();

    let mut reader = vfs.open(&path).expect("open 7z entry");
    reader.seek(SeekFrom::Start(6)).expect("seek from start");
    reader
        .seek(SeekFrom::Current(2))
        .expect("seek from current");
    reader.seek(SeekFrom::End(-6)).expect("seek from end");
    let mut contents = String::new();
    reader.read_to_string(&mut contents).expect("read 7z entry");

    assert_eq!(contents, "stream");
}

#[test]
fn reuses_cached_sevenz_entry() {
    let archive = test_archive();
    let vfs = SevenZArchiveVfs::open(archive.path()).expect("open 7z archive");
    let path = vfs
        .entries()
        .expect("list 7z entries")
        .into_iter()
        .find(|entry| entry.path().ends_with("readme.txt"))
        .expect("find readme entry")
        .path()
        .to_owned();

    vfs.open(&path).expect("open 7z entry first time");
    assert_eq!(
        vfs.cache_len().expect("read cache length"),
        2,
        "materializing one entry must cache every entry in its solid block"
    );
    vfs.open(&path).expect("open 7z entry second time");
    assert_eq!(vfs.cache_len().expect("read cache length"), 2);
}

#[test]
fn missing_sevenz_entry_returns_not_found() {
    let archive = test_archive();
    let vfs = SevenZArchiveVfs::open(archive.path()).expect("open 7z archive");

    let result = vfs.open(std::path::Path::new("missing.txt"));
    let error = match result {
        Ok(_) => panic!("missing 7z entry must fail"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn reads_an_entry_after_another_entry_in_the_solid_stream() {
    let archive = test_archive();
    let vfs = SevenZArchiveVfs::open(archive.path()).expect("open 7z archive");
    let entries = vfs.entries().expect("list 7z entries");
    let first_index = entries
        .iter()
        .position(|entry| entry.path().ends_with("first.txt"))
        .expect("find first solid entry");
    let target_index = entries
        .iter()
        .position(|entry| entry.path().ends_with("readme.txt"))
        .expect("find later solid entry");
    assert!(
        target_index > first_index,
        "readme entry must follow first.txt in the generated solid archive"
    );
    let path = entries
        .into_iter()
        .find(|entry| entry.path().ends_with("readme.txt"))
        .expect("find later solid entry")
        .path()
        .to_owned();

    let mut reader = vfs.open(&path).expect("open later solid entry");
    let mut contents = String::new();
    reader
        .read_to_string(&mut contents)
        .expect("read later solid entry");

    assert_eq!(contents, "seven z stream");
}

#[test]
fn corrupt_sevenz_archive_returns_an_error() {
    let mut archive = tempfile::NamedTempFile::new().expect("create corrupt 7z archive");
    archive
        .write_all(b"not a 7z archive")
        .expect("write corrupt 7z archive");

    let result = SevenZArchiveVfs::open(archive.path());

    assert!(result.is_err(), "corrupt 7z archive must be rejected");
}
