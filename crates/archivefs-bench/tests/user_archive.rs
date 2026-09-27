use std::env;
use std::error::Error;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;

use lazy_archive::{detect_archive_kind, open_archive};

#[test]
#[ignore = "requires ARCHIVEFS_TEST_ARCHIVE"]
fn opens_and_reads_user_supplied_archive() -> Result<(), Box<dyn Error>> {
    let path = env::var_os("ARCHIVEFS_TEST_ARCHIVE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "set ARCHIVEFS_TEST_ARCHIVE to a supported archive path",
            )
        })?;
    let kind = detect_archive_kind(&path)?;
    let archive = open_archive(&path)?;
    let entries = archive.entries()?;
    eprintln!(
        "checking user archive {} ({kind:?}), {} entries",
        path.display(),
        entries.len()
    );

    let Some(entry) = entries.iter().find(|entry| !entry.is_directory()) else {
        eprintln!("archive listing passed; no regular file to read or seek");
        return Ok(());
    };

    let mut reader = archive.open(entry.path())?;
    let prefix_length = entry.size().min(32) as usize;
    let mut prefix = vec![0; prefix_length];
    reader.read_exact(&mut prefix)?;
    reader.seek(SeekFrom::Start(0))?;
    let mut reread = vec![0; prefix_length];
    reader.read_exact(&mut reread)?;
    assert_eq!(
        prefix, reread,
        "entry prefix must be stable after seeking to start"
    );
    if prefix_length > 0 {
        let current = reader.seek(SeekFrom::Current(-1))?;
        assert_eq!(
            current,
            prefix_length as u64 - 1,
            "relative seek must move one byte backward"
        );
    }

    if entry.size() > 0 {
        reader.seek(SeekFrom::End(-1))?;
        let mut last_byte = [0];
        reader.read_exact(&mut last_byte)?;
    }
    eprintln!("read and seek passed for {}", entry.path().display());
    Ok(())
}
