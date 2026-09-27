use std::io::{Read, Seek, SeekFrom, Write};

use lazy_archive::ZipArchiveVfs;
use vfs_archive::ArchiveVfs;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

fn test_archive() -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("create temporary archive");
    let mut writer = zip::ZipWriter::new(file.reopen().expect("reopen temporary archive"));
    writer
        .start_file(
            "docs/readme.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .expect("start file");
    writer.write_all(b"archivefs").expect("write file");
    writer
        .add_directory("empty/", SimpleFileOptions::default())
        .expect("add directory");
    writer.finish().expect("finish archive");
    file
}

#[test]
fn lists_files_and_directories() {
    let archive = test_archive();
    let vfs = ZipArchiveVfs::open(archive.path()).expect("open archive");

    let entries = vfs.entries().expect("list entries");

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path().to_str(), Some("docs/readme.txt"));
    assert_eq!(entries[0].size(), 9);
    assert!(entries[1].is_directory());
}

#[test]
fn opens_one_entry_with_seek_support() {
    let archive = test_archive();
    let vfs = ZipArchiveVfs::open(archive.path()).expect("open archive");
    let mut reader = vfs
        .open(std::path::Path::new("docs/readme.txt"))
        .expect("open entry");

    reader.seek(SeekFrom::Start(2)).expect("seek from start");
    reader
        .seek(SeekFrom::Current(3))
        .expect("seek from current");
    reader.seek(SeekFrom::End(-4)).expect("seek from end");
    let mut suffix = String::new();
    reader.read_to_string(&mut suffix).expect("read suffix");

    assert_eq!(suffix, "vefs");
    assert_eq!(vfs.cache_len().expect("read cache length"), 0);
}

#[test]
fn compressed_entry_supports_incremental_reads() {
    let archive = test_archive();
    let vfs = ZipArchiveVfs::open(archive.path()).expect("open archive");
    let mut reader = vfs
        .open(std::path::Path::new("docs/readme.txt"))
        .expect("open compressed entry");
    let mut contents = Vec::new();
    let mut chunk = [0u8; 2];

    loop {
        let read = reader.read(&mut chunk).expect("read compressed chunk");
        if read == 0 {
            break;
        }
        contents.extend_from_slice(&chunk[..read]);
    }

    assert_eq!(contents, b"archivefs");
}

#[test]
fn large_compressed_entry_streams_across_chunks_and_restarts_on_seek() {
    let file = tempfile::NamedTempFile::new().expect("create temporary archive");
    let mut writer = zip::ZipWriter::new(file.reopen().expect("reopen temporary archive"));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer
        .start_file("large.bin", options)
        .expect("start compressed entry");
    let contents: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
    writer.write_all(&contents).expect("write compressed entry");
    writer.finish().expect("finish archive");

    let vfs = ZipArchiveVfs::open(file.path()).expect("open archive");
    let mut reader = vfs
        .open(std::path::Path::new("large.bin"))
        .expect("open compressed entry");
    let mut actual = Vec::with_capacity(contents.len());
    let mut chunk = [0u8; 4093];
    loop {
        let read = reader.read(&mut chunk).expect("read compressed chunk");
        if read == 0 {
            break;
        }
        actual.extend_from_slice(&chunk[..read]);
    }
    assert_eq!(
        actual, contents,
        "streamed chunks must reproduce full entry"
    );

    reader
        .seek(SeekFrom::Start(700_001))
        .expect("seek to later position");
    let mut seeked = [0u8; 64];
    reader
        .read_exact(&mut seeked)
        .expect("read after restarting compressed stream");
    assert_eq!(
        &seeked[..],
        &contents[700_001..700_065],
        "seek must restart at the correct uncompressed position"
    );
    assert_eq!(vfs.cache_len().expect("read cache length"), 0);
}

#[test]
fn stored_entry_reads_directly_without_using_cache() {
    let file = tempfile::NamedTempFile::new().expect("create temporary archive");
    let mut writer = zip::ZipWriter::new(file.reopen().expect("reopen temporary archive"));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer
        .start_file("stored.txt", options)
        .expect("start stored file");
    writer
        .write_all(b"direct stream")
        .expect("write stored file");
    writer.finish().expect("finish archive");

    let vfs = ZipArchiveVfs::open(file.path()).expect("open archive");
    let mut reader = vfs
        .open(std::path::Path::new("stored.txt"))
        .expect("open stored entry");
    reader.seek(SeekFrom::End(-6)).expect("seek from end");
    let mut suffix = String::new();
    reader
        .read_to_string(&mut suffix)
        .expect("read stored suffix");

    assert_eq!(suffix, "stream");
    assert_eq!(vfs.cache_len().expect("read cache length"), 0);
}

#[test]
fn missing_entry_returns_not_found_without_panicking() {
    let archive = test_archive();
    let vfs = ZipArchiveVfs::open(archive.path()).expect("open archive");

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
    let writer = zip::ZipWriter::new(file.reopen().expect("reopen temporary archive"));
    writer.finish().expect("finish empty archive");
    let vfs = ZipArchiveVfs::open(file.path()).expect("open empty archive");

    assert!(vfs.entries().expect("list empty archive").is_empty());
}

#[test]
fn corrupt_archive_returns_an_error_without_panicking() {
    let mut file = tempfile::NamedTempFile::new().expect("create temporary archive");
    file.write_all(b"not a zip archive")
        .expect("write corrupt archive");

    let result = ZipArchiveVfs::open(file.path());

    assert!(result.is_err(), "corrupt ZIP must be rejected");
}
