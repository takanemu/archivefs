use std::io::Write;
use std::process::{Command, Output};

use flate2::{Compression, write::GzEncoder};
use tar::{Builder, Header};
use tempfile::NamedTempFile;
use zip::write::SimpleFileOptions;

fn zip_fixture() -> NamedTempFile {
    let archive = NamedTempFile::new().expect("create ZIP fixture");
    let mut writer = zip::ZipWriter::new(archive.reopen().expect("reopen ZIP fixture"));
    writer
        .start_file("docs/readme.txt", SimpleFileOptions::default())
        .expect("start text entry");
    writer
        .write_all(b"archivefs benchmark sample")
        .expect("write text entry");
    writer
        .start_file("images/cover.png", SimpleFileOptions::default())
        .expect("start image candidate");
    writer
        .write_all(b"fake image bytes for retrieval timing")
        .expect("write image candidate");
    writer.finish().expect("finish ZIP fixture");
    archive
}

fn tar_gz_fixture() -> NamedTempFile {
    let archive = NamedTempFile::new().expect("create TAR.GZ fixture");
    let encoder = GzEncoder::new(
        archive.reopen().expect("reopen TAR.GZ fixture"),
        Compression::default(),
    );
    let mut builder = Builder::new(encoder);
    let contents = b"archivefs tar fixture";
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
    archive
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archivefs-bench"))
        .args(args)
        .output()
        .expect("run archivefs-bench")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("CLI output is UTF-8")
}

fn assert_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "CLI command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inspect_and_all_bench_scenarios_work_with_generated_zip() {
    let archive = zip_fixture();
    let path = archive.path().to_str().expect("fixture path is UTF-8");

    let inspect = run(&["inspect", path]);
    assert_succeeded(&inspect);
    assert!(stdout(&inspect).contains("format: Zip"));
    assert!(stdout(&inspect).contains("entries: 2"));

    let open = run(&["bench", path, "--warmup", "0", "--iterations", "2", "open"]);
    assert_succeeded(&open);
    assert!(stdout(&open).contains("archive open"));
    assert!(stdout(&open).contains("entry listing"));

    let scan = run(&[
        "bench",
        path,
        "--warmup",
        "0",
        "--iterations",
        "2",
        "scan",
        "--chunk-size",
        "7",
    ]);
    assert_succeeded(&scan);
    assert!(stdout(&scan).contains("sequential scan"));
    assert!(stdout(&scan).contains("bytes:"));
    assert!(stdout(&scan).contains("scan read buffer: 7 bytes"));
    assert!(stdout(&scan).contains("files read: 4 across 2 measured scans"));
    assert!(stdout(&scan).contains("Read calls: 24 total (20 returned data"));
    assert!(stdout(&scan).contains("average bytes per data Read call:"));

    let random = run(&[
        "bench",
        path,
        "--warmup",
        "0",
        "--iterations",
        "2",
        "random",
        "--seed",
        "123",
        "--requests",
        "3",
        "--max-entries",
        "1",
        "--read-size",
        "4",
        "--cache-state",
        "cold",
    ]);
    assert_succeeded(&random);
    assert!(stdout(&random).contains("random open/seek/read latency"));
    assert!(stdout(&random).contains("samples: 6"));
    assert!(stdout(&random).contains("distinct candidate files: 1"));

    let thumbnail = run(&[
        "bench",
        path,
        "--warmup",
        "0",
        "--iterations",
        "1",
        "thumbnail-read",
        "--max-images",
        "1",
        "--chunk-size",
        "8",
    ]);
    assert_succeeded(&thumbnail);
    assert!(stdout(&thumbnail).contains("thumbnail-read candidates: 1"));
}

#[test]
fn inspect_and_verify_work_with_generated_tar_gz() {
    let archive = tar_gz_fixture();
    let path = archive.path().to_str().expect("fixture path is UTF-8");

    let inspect = run(&["inspect", path]);
    assert_succeeded(&inspect);
    assert!(stdout(&inspect).contains("format: TarGzip"));

    let verify = run(&["verify", path]);
    assert_succeeded(&verify);
    assert!(stdout(&verify).contains("verification: listing, read, and seek passed"));
}

#[test]
fn unknown_archive_and_zero_iterations_are_reported() {
    let mut archive = NamedTempFile::new().expect("create unknown archive");
    archive
        .write_all(b"not an archive")
        .expect("write unknown archive");
    let path = archive.path().to_str().expect("fixture path is UTF-8");

    let unknown = run(&["inspect", path]);
    assert!(!unknown.status.success(), "unknown format must fail");
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unsupported archive format"));

    let valid_archive = zip_fixture();
    let valid_path = valid_archive
        .path()
        .to_str()
        .expect("fixture path is UTF-8");
    let zero_iterations = run(&["bench", valid_path, "--iterations", "0", "open"]);
    assert!(
        !zero_iterations.status.success(),
        "zero iterations must fail"
    );
    assert!(String::from_utf8_lossy(&zero_iterations.stderr).contains("iterations"));
}
