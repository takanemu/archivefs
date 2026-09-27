use std::io::{Read, Write};
use std::num::NonZeroUsize;

use flate2::{Compression, write::GzEncoder};
use lazy_archive::{
    ArchiveKind, EntryCache, detect_archive_kind, open_archive, open_archive_with_cache,
};
use tar::{Builder, Header};
use tempfile::NamedTempFile;
use zip::write::SimpleFileOptions;

fn zip_archive() -> NamedTempFile {
    let file = NamedTempFile::new().expect("create ZIP archive");
    let mut writer = zip::ZipWriter::new(file.reopen().expect("reopen ZIP archive"));
    writer
        .start_file("readme.txt", SimpleFileOptions::default())
        .expect("start ZIP entry");
    writer
        .write_all(b"unified archive")
        .expect("write ZIP entry");
    writer.finish().expect("finish ZIP archive");
    file
}

fn tar_gz_archive() -> NamedTempFile {
    let file = NamedTempFile::new().expect("create TAR.GZ archive");
    let encoder = GzEncoder::new(
        file.reopen().expect("reopen TAR.GZ archive"),
        Compression::default(),
    );
    let mut builder = Builder::new(encoder);
    let contents = b"configured archive cache";
    let mut header = Header::new_gnu();
    header.set_path("readme.txt").expect("set TAR path");
    header.set_size(contents.len() as u64);
    header.set_cksum();
    builder
        .append(&header, &contents[..])
        .expect("append TAR entry");
    builder
        .into_inner()
        .expect("finish TAR archive")
        .finish()
        .expect("finish gzip stream");
    file
}

#[test]
fn detects_and_opens_zip_through_common_api() {
    let archive = zip_archive();

    assert_eq!(
        detect_archive_kind(archive.path()).expect("detect ZIP"),
        ArchiveKind::Zip
    );
    let vfs = open_archive(archive.path()).expect("open ZIP through common API");
    assert_eq!(vfs.entries().expect("list ZIP entries").len(), 1);
}

#[test]
fn opens_archive_with_a_caller_configured_cache() {
    let archive = tar_gz_archive();
    let cache = EntryCache::with_limits(
        NonZeroUsize::new(3).expect("cache capacity is non-zero"),
        8,
        1024,
    );
    assert_eq!(cache.capacity(), 3);

    let vfs =
        open_archive_with_cache(archive.path(), cache).expect("open archive with configured cache");
    let mut reader = vfs
        .open(std::path::Path::new("readme.txt"))
        .expect("open TAR.GZ entry");
    let mut contents = String::new();
    reader
        .read_to_string(&mut contents)
        .expect("read cached TAR.GZ entry");

    assert_eq!(contents, "configured archive cache");
}

#[test]
fn detects_tar_variants_from_extensions_when_signature_is_ambiguous() {
    let archive = NamedTempFile::new().expect("create TAR placeholder");
    let path = archive.path().with_extension("tar.xz");
    std::fs::copy(archive.path(), &path).expect("copy TAR placeholder");

    assert_eq!(
        detect_archive_kind(&path).expect("detect TAR.XZ by extension"),
        ArchiveKind::TarXz
    );
}

#[test]
fn rejects_unknown_archive_format() {
    let mut archive = NamedTempFile::new().expect("create unknown archive");
    archive
        .write_all(b"not an archive")
        .expect("write unknown archive");

    let result = open_archive(archive.path());

    assert!(result.is_err(), "unknown format must be rejected");
}
