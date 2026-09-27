use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use bzip2::{Compression as BzipCompression, write::BzEncoder};
use flate2::{Compression, write::GzEncoder};
use lazy_archive::{TarArchiveVfs, TarGzArchiveVfs};
use tar::{Builder, Header};
use vfs_archive::ArchiveVfs;
use xz2::write::XzEncoder;

fn append_file<W: Write>(writer: W) -> W {
    let mut builder = Builder::new(writer);
    let contents = b"tar format stream";
    let mut header = Header::new_gnu();
    header.set_path("docs/readme.txt").expect("set TAR path");
    header.set_size(contents.len() as u64);
    header.set_cksum();
    builder
        .append(&header, &contents[..])
        .expect("append TAR file");
    builder.into_inner().expect("finish TAR")
}

fn archive_file() -> tempfile::NamedTempFile {
    tempfile::NamedTempFile::new().expect("create TAR archive")
}

fn assert_readable(vfs: &dyn ArchiveVfs) {
    let entries = vfs.entries().expect("list TAR entries");
    assert_eq!(entries.len(), 1, "archive must contain one file");
    assert_eq!(entries[0].path(), Path::new("docs/readme.txt"));

    let mut reader = vfs
        .open(Path::new("docs/readme.txt"))
        .expect("open TAR entry");
    reader.seek(SeekFrom::Start(4)).expect("seek TAR entry");
    let mut contents = String::new();
    reader
        .read_to_string(&mut contents)
        .expect("read TAR entry");
    assert_eq!(contents, "format stream");
}

#[test]
fn reads_plain_tar() {
    let archive = archive_file();
    append_file(archive.reopen().expect("reopen TAR archive"));
    let vfs = TarArchiveVfs::open(archive.path()).expect("open plain TAR");

    assert_readable(&vfs);
}

#[test]
fn reads_tar_gzip() {
    let archive = archive_file();
    let encoder = GzEncoder::new(
        archive.reopen().expect("reopen TAR.GZ"),
        Compression::default(),
    );
    append_file(encoder).finish().expect("finish TAR.GZ");
    let vfs = TarGzArchiveVfs::open(archive.path()).expect("open TAR.GZ");

    assert_readable(&vfs);
}

#[test]
fn reads_tar_bzip2() {
    let archive = archive_file();
    let encoder = BzEncoder::new(
        archive.reopen().expect("reopen TAR.BZ2"),
        BzipCompression::default(),
    );
    let encoder = append_file(encoder);
    encoder.finish().expect("finish TAR.BZ2");
    let vfs = TarArchiveVfs::open_bzip2(archive.path()).expect("open TAR.BZ2");

    assert_readable(&vfs);
}

#[test]
fn reads_tar_xz() {
    let archive = archive_file();
    let encoder = XzEncoder::new(archive.reopen().expect("reopen TAR.XZ"), 6);
    let encoder = append_file(encoder);
    encoder.finish().expect("finish TAR.XZ");
    let vfs = TarArchiveVfs::open_xz(archive.path()).expect("open TAR.XZ");

    assert_readable(&vfs);
}

#[test]
fn reads_tar_zstd() {
    let archive = archive_file();
    let encoder = zstd::stream::write::Encoder::new(archive.reopen().expect("reopen TAR.ZSTD"), 0)
        .expect("create TAR.ZSTD encoder");
    let encoder = append_file(encoder);
    encoder.finish().expect("finish TAR.ZSTD");
    let vfs = TarArchiveVfs::open_zstd(archive.path()).expect("open TAR.ZSTD");

    assert_readable(&vfs);
}
