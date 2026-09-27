//! Lazy archive readers.
//!
//! Format-specific readers and lazy archive access.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::thread;

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use lru::LruCache;
use memmap2::Mmap;
use tar::Archive;
use vfs_archive::{ArchiveEntry, ArchiveVfs, ReadSeek};
use xz2::read::XzDecoder;

/// Errors returned by lazy archive implementations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("archive I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("archive entry is not a regular file: {0}")]
    NotAFile(String),
    #[error("archive entry was not found: {0}")]
    NotFound(String),
    #[error("unsupported archive format: {0}")]
    UnsupportedFormat(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

/// An archive format supported by the unified opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Tar,
    TarGzip,
    TarBzip2,
    TarXz,
    TarZstd,
    SevenZ,
    Rar,
}

/// Detects an archive format using its signature and, for TAR variants, its extension.
pub fn detect_archive_kind(path: impl AsRef<Path>) -> Result<ArchiveKind> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let mut signature = [0u8; 8];
    let length = file.read(&mut signature)?;
    let signature = &signature[..length];

    if signature.starts_with(b"PK\x03\x04")
        || signature.starts_with(b"PK\x05\x06")
        || signature.starts_with(b"PK\x07\x08")
    {
        return Ok(ArchiveKind::Zip);
    }
    if signature.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return Ok(ArchiveKind::SevenZ);
    }
    if signature.starts_with(b"Rar!\x1A\x07") {
        return Ok(ArchiveKind::Rar);
    }
    if signature.starts_with(&[0x1F, 0x8B]) {
        return Ok(ArchiveKind::TarGzip);
    }
    if signature.starts_with(b"BZh") {
        return Ok(ArchiveKind::TarBzip2);
    }
    if signature.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        return Ok(ArchiveKind::TarXz);
    }
    if signature.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return Ok(ArchiveKind::TarZstd);
    }

    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Ok(ArchiveKind::TarGzip);
    }
    if name.ends_with(".tar.bz2") || name.ends_with(".tbz") || name.ends_with(".tbz2") {
        return Ok(ArchiveKind::TarBzip2);
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return Ok(ArchiveKind::TarXz);
    }
    if name.ends_with(".tar.zst") || name.ends_with(".tar.zstd") {
        return Ok(ArchiveKind::TarZstd);
    }
    if name.ends_with(".tar") {
        return Ok(ArchiveKind::Tar);
    }

    Err(Error::UnsupportedFormat(path.to_owned()))
}

/// Opens any supported archive behind the common VFS interface.
pub fn open_archive(path: impl Into<PathBuf>) -> Result<Box<dyn ArchiveVfs>> {
    open_archive_with_cache(path, EntryCache::default())
}

/// Opens any supported archive with a caller-configured entry cache.
///
/// The cache is used by formats that materialize decoded entries. ZIP entries
/// are streamed directly and do not currently populate this cache.
pub fn open_archive_with_cache(
    path: impl Into<PathBuf>,
    cache: EntryCache,
) -> Result<Box<dyn ArchiveVfs>> {
    let path = path.into();
    match detect_archive_kind(&path)? {
        ArchiveKind::Zip => Ok(Box::new(ZipArchiveVfs::with_cache(path, cache)?)),
        ArchiveKind::Tar => Ok(Box::new(TarArchiveVfs::with_compression(
            path,
            TarCompression::Plain,
            cache,
        )?)),
        ArchiveKind::TarGzip => Ok(Box::new(TarGzArchiveVfs::with_cache(path, cache)?)),
        ArchiveKind::TarBzip2 => Ok(Box::new(TarArchiveVfs::with_compression(
            path,
            TarCompression::Bzip2,
            cache,
        )?)),
        ArchiveKind::TarXz => Ok(Box::new(TarArchiveVfs::with_compression(
            path,
            TarCompression::Xz,
            cache,
        )?)),
        ArchiveKind::TarZstd => Ok(Box::new(TarArchiveVfs::with_compression(
            path,
            TarCompression::Zstd,
            cache,
        )?)),
        ArchiveKind::SevenZ => Ok(Box::new(SevenZArchiveVfs::with_cache(path, cache)?)),
        ArchiveKind::Rar => Ok(Box::new(RarArchiveVfs::with_cache(path, cache)?)),
    }
}

const DEFAULT_MEMORY_THRESHOLD: usize = 1024 * 1024;
const DEFAULT_MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_CACHE_CAPACITY: usize = 256;

enum CachedEntry {
    Memory(Vec<u8>),
    Temporary(tempfile::NamedTempFile),
}

struct ZipStoredReader {
    file: File,
    data_start: u64,
    length: u64,
    position: u64,
}

struct ZipCompressedReader {
    stream: Mutex<ZipCompressedStream>,
    path: PathBuf,
    name: String,
    length: u64,
    position: u64,
}

enum ZipStreamMessage {
    Data(Vec<u8>),
    Error(io::ErrorKind, String),
    End,
}

struct ZipCompressedStream {
    receiver: mpsc::Receiver<ZipStreamMessage>,
    pending: Vec<u8>,
    pending_offset: usize,
    ended: bool,
    deferred_error: Option<(io::ErrorKind, String)>,
}

struct MmapReader {
    map: Mmap,
    position: usize,
}

impl Read for MmapReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.map.len().saturating_sub(self.position);
        let read_length = remaining.min(buffer.len());
        buffer[..read_length]
            .copy_from_slice(&self.map[self.position..self.position + read_length]);
        self.position += read_length;
        Ok(read_length)
    }
}

impl Seek for MmapReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let current = i128::try_from(self.position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mmap position overflowed"))?;
        let length = i128::try_from(self.map.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mmap length overflowed"))?;
        let next = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => current + i128::from(offset),
            SeekFrom::End(offset) => length + i128::from(offset),
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mmap seek moved before the beginning",
            ));
        }
        self.position = usize::try_from(next)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mmap seek overflowed"))?;
        Ok(self.position as u64)
    }
}

impl Read for ZipStoredReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.length || buffer.is_empty() {
            return Ok(0);
        }

        let available = self.length - self.position;
        let read_length = available.min(buffer.len() as u64) as usize;
        self.file
            .seek(SeekFrom::Start(self.data_start + self.position))?;
        let read = self.file.read(&mut buffer[..read_length])?;
        self.position = self.position.saturating_add(read as u64);
        Ok(read)
    }
}

impl Seek for ZipStoredReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.length) + i128::from(offset),
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZIP entry seek moved before the beginning",
            ));
        }
        self.position = u64::try_from(next).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry seek overflowed")
        })?;
        Ok(self.position)
    }
}

impl ZipCompressedReader {
    fn open(path: PathBuf, name: String, length: u64) -> io::Result<Self> {
        let stream = ZipCompressedStream::start(&path, &name, 0)?;
        Ok(Self {
            stream: Mutex::new(stream),
            path,
            name,
            length,
            position: 0,
        })
    }
}

impl ZipCompressedStream {
    const CHUNK_SIZE: usize = 64 * 1024;

    fn start(path: &Path, name: &str, offset: u64) -> io::Result<Self> {
        let worker_path = path.to_owned();
        let worker_name = name.to_owned();
        let (data_sender, receiver) = mpsc::sync_channel(2);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);

        thread::Builder::new()
            .name("archivefs-zip-reader".to_owned())
            .spawn(move || {
                let file = match File::open(&worker_path) {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = ready_sender.send(Err((error.kind(), error.to_string())));
                        return;
                    }
                };
                let mut archive = match zip::ZipArchive::new(file).map_err(zip_error) {
                    Ok(archive) => archive,
                    Err(error) => {
                        let _ = ready_sender.send(Err((error.kind(), error.to_string())));
                        return;
                    }
                };
                let mut entry = match archive.by_name(&worker_name).map_err(zip_error) {
                    Ok(entry) => entry,
                    Err(error) => {
                        let _ = ready_sender.send(Err((error.kind(), error.to_string())));
                        return;
                    }
                };

                let mut remaining = offset;
                let mut skip_buffer = [0u8; 8192];
                while remaining > 0 {
                    let read_length = remaining.min(skip_buffer.len() as u64) as usize;
                    match entry.read(&mut skip_buffer[..read_length]) {
                        Ok(0) => {
                            let _ = ready_sender.send(Err((
                                io::ErrorKind::UnexpectedEof,
                                "ZIP entry ended before the seek target".to_owned(),
                            )));
                            return;
                        }
                        Ok(read) => remaining -= read as u64,
                        Err(error) => {
                            let _ = ready_sender.send(Err((error.kind(), error.to_string())));
                            return;
                        }
                    }
                }

                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                loop {
                    let mut chunk = vec![0; Self::CHUNK_SIZE];
                    match entry.read(&mut chunk) {
                        Ok(0) => {
                            let _ = data_sender.send(ZipStreamMessage::End);
                            break;
                        }
                        Ok(read) => {
                            chunk.truncate(read);
                            if data_sender.send(ZipStreamMessage::Data(chunk)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = data_sender
                                .send(ZipStreamMessage::Error(error.kind(), error.to_string()));
                            break;
                        }
                    }
                }
            })?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                receiver,
                pending: Vec::new(),
                pending_offset: 0,
                ended: false,
                deferred_error: None,
            }),
            Ok(Err((kind, message))) => Err(io::Error::new(kind, message)),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ZIP reader worker exited during initialization",
            )),
        }
    }

    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if let Some((kind, message)) = self.deferred_error.take() {
            return Err(io::Error::new(kind, message));
        }

        let mut written = 0;
        loop {
            if self.pending_offset < self.pending.len() {
                let available = self.pending.len() - self.pending_offset;
                let count = available.min(output.len() - written);
                output[written..written + count].copy_from_slice(
                    &self.pending[self.pending_offset..self.pending_offset + count],
                );
                self.pending_offset += count;
                written += count;
                if written == output.len() {
                    return Ok(written);
                }
                continue;
            }

            if self.ended {
                return Ok(written);
            }

            self.pending.clear();
            self.pending_offset = 0;
            match self.receiver.recv() {
                Ok(ZipStreamMessage::Data(chunk)) => self.pending = chunk,
                Ok(ZipStreamMessage::End) => self.ended = true,
                Ok(ZipStreamMessage::Error(kind, message)) => {
                    if written == 0 {
                        return Err(io::Error::new(kind, message));
                    }
                    self.deferred_error = Some((kind, message));
                    return Ok(written);
                }
                Err(_) => {
                    self.ended = true;
                    if written == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "ZIP reader worker exited unexpectedly",
                        ));
                    }
                    return Ok(written);
                }
            }
        }
    }
}

impl Read for ZipCompressedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.length || buffer.is_empty() {
            return Ok(0);
        }
        let read_length = (self.length - self.position).min(buffer.len() as u64) as usize;
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| io::Error::other("ZIP reader was poisoned"))?;
        let read = stream.read(&mut buffer[..read_length])?;
        self.position = self.position.saturating_add(read as u64);
        Ok(read)
    }
}

impl Seek for ZipCompressedReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.length) + i128::from(offset),
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZIP compressed entry seek moved before the beginning",
            ));
        }
        let next = u64::try_from(next).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZIP compressed entry seek overflowed",
            )
        })?;
        let next = next.min(self.length);
        let stream = ZipCompressedStream::start(&self.path, &self.name, next)?;
        *self
            .stream
            .lock()
            .map_err(|_| io::Error::other("ZIP reader was poisoned"))? = stream;
        self.position = next;
        Ok(self.position)
    }
}

impl CachedEntry {
    fn size(&self) -> usize {
        match self {
            Self::Memory(contents) => contents.len(),
            Self::Temporary(file) => file.as_file().metadata().map_or(0, |metadata| {
                usize::try_from(metadata.len()).unwrap_or(usize::MAX)
            }),
        }
    }

    fn reader(&self) -> io::Result<Box<dyn ReadSeek>> {
        match self {
            Self::Memory(contents) => Ok(Box::new(io::Cursor::new(contents.clone()))),
            Self::Temporary(file) => {
                // The cache owns the NamedTempFile; the returned map owns the read view.
                let map = unsafe { Mmap::map(file.as_file())? };
                Ok(Box::new(MmapReader { map, position: 0 }))
            }
        }
    }
}

struct Materializer {
    memory_threshold: usize,
    memory: Vec<u8>,
    temporary: Option<tempfile::NamedTempFile>,
}

struct SharedMaterializerWriter {
    materializer: std::sync::Arc<Mutex<Materializer>>,
}

impl Write for SharedMaterializerWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.materializer
            .lock()
            .map_err(|_| io::Error::other("RAR materializer was poisoned"))?
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.materializer
            .lock()
            .map_err(|_| io::Error::other("RAR materializer was poisoned"))?
            .flush()
    }
}

impl Materializer {
    fn new(memory_threshold: usize) -> Self {
        Self {
            memory_threshold,
            memory: Vec::new(),
            temporary: None,
        }
    }

    fn finish(self) -> io::Result<CachedEntry> {
        if let Some(mut file) = self.temporary {
            file.flush()?;
            Ok(CachedEntry::Temporary(file))
        } else {
            Ok(CachedEntry::Memory(self.memory))
        }
    }
}

impl Write for Materializer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.temporary.is_none()
            && self.memory.len().saturating_add(buffer.len()) > self.memory_threshold
        {
            let mut file = tempfile::NamedTempFile::new()?;
            file.write_all(&self.memory)?;
            self.memory.clear();
            self.temporary = Some(file);
        }

        if let Some(file) = self.temporary.as_mut() {
            file.write_all(buffer)?;
        } else {
            self.memory.extend_from_slice(buffer);
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.temporary.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

/// A bounded cache of fully materialized archive entries.
pub struct EntryCache {
    entries: Mutex<LruCache<PathBuf, CachedEntry>>,
    capacity: NonZeroUsize,
    memory_threshold: usize,
    max_bytes: usize,
    current_bytes: Mutex<usize>,
}

impl EntryCache {
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self::with_memory_threshold(capacity, DEFAULT_MEMORY_THRESHOLD)
    }

    pub fn with_memory_threshold(capacity: NonZeroUsize, memory_threshold: usize) -> Self {
        Self::with_limits(capacity, memory_threshold, DEFAULT_MAX_CACHE_BYTES)
    }

    pub fn with_limits(capacity: NonZeroUsize, memory_threshold: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(capacity)),
            capacity,
            memory_threshold,
            max_bytes,
            current_bytes: Mutex::new(0),
        }
    }

    fn get(&self, path: &Path) -> io::Result<Option<Box<dyn ReadSeek>>> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::other("archive entry cache was poisoned"))?;
        let Some(entry) = entries.get(path) else {
            return Ok(None);
        };

        Ok(Some(entry.reader()?))
    }

    fn insert_entry(&self, path: PathBuf, entry: CachedEntry) -> io::Result<Box<dyn ReadSeek>> {
        let entry_size = entry.size();
        if entry_size > self.max_bytes {
            return entry.reader();
        }

        let mut entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::other("archive entry cache was poisoned"))?;
        let mut current_bytes = self
            .current_bytes
            .lock()
            .map_err(|_| io::Error::other("archive byte counter was poisoned"))?;

        if let Some(previous) = entries.pop(&path) {
            *current_bytes = current_bytes.saturating_sub(previous.size());
        }
        while current_bytes.saturating_add(entry_size) > self.max_bytes {
            let Some((_, evicted)) = entries.pop_lru() else {
                break;
            };
            *current_bytes = current_bytes.saturating_sub(evicted.size());
        }

        *current_bytes = current_bytes.saturating_add(entry_size);
        entries.put(path.clone(), entry);
        entries.get(&path).expect("entry was inserted").reader()
    }

    fn insert_reader<R: Read>(
        &self,
        path: PathBuf,
        reader: &mut R,
    ) -> io::Result<Box<dyn ReadSeek>> {
        let mut materializer = Materializer::new(self.memory_threshold);
        io::copy(reader, &mut materializer)?;
        self.insert_entry(path, materializer.finish()?)
    }

    pub fn len(&self) -> io::Result<usize> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| io::Error::other("archive entry cache was poisoned"))?
            .len())
    }

    /// Returns the maximum number of entries this cache can retain.
    pub fn capacity(&self) -> usize {
        self.capacity.get()
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| io::Error::other("archive entry cache was poisoned"))?
            .is_empty())
    }

    pub fn bytes(&self) -> io::Result<usize> {
        Ok(*self
            .current_bytes
            .lock()
            .map_err(|_| io::Error::other("archive byte counter was poisoned"))?)
    }
}

impl Default for EntryCache {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).expect("default cache capacity is non-zero"),
        )
    }
}

/// A ZIP archive with entry metadata read on demand.
pub struct ZipArchiveVfs {
    path: PathBuf,
    cache: EntryCache,
}

impl ZipArchiveVfs {
    /// Opens a ZIP archive and validates its central directory.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = File::open(&path)?;
        zip::ZipArchive::new(file).map_err(zip_error)?;

        Ok(Self {
            path,
            cache: EntryCache::default(),
        })
    }

    pub fn with_cache(path: impl Into<PathBuf>, cache: EntryCache) -> Result<Self> {
        let path = path.into();
        let file = File::open(&path)?;
        zip::ZipArchive::new(file).map_err(zip_error)?;

        Ok(Self { path, cache })
    }

    pub fn cache_len(&self) -> io::Result<usize> {
        self.cache.len()
    }
}

impl ArchiveVfs for ZipArchiveVfs {
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
        let mut entries = Vec::with_capacity(archive.len());

        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(zip_error)?;
            let path = PathBuf::from(entry.name());
            if entry.is_dir() {
                entries.push(ArchiveEntry::directory(path));
            } else {
                entries.push(ArchiveEntry::file(path, entry.size()));
            }
        }

        Ok(entries)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        if let Some(reader) = self.cache.get(path)? {
            return Ok(reader);
        }

        let file = File::open(&self.path)?;
        let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
        let entry_name = path.to_string_lossy();
        let entry = archive.by_name(entry_name.as_ref()).map_err(zip_error)?;

        if entry.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("archive entry is a directory: {entry_name}"),
            ));
        }

        if entry.compression() == zip::CompressionMethod::Stored {
            let data_start = entry.data_start();
            let length = entry.size();
            drop(entry);
            drop(archive);
            return Ok(Box::new(ZipStoredReader {
                file: File::open(&self.path)?,
                data_start,
                length,
                position: 0,
            }));
        }

        let length = entry.size();
        drop(entry);
        drop(archive);
        Ok(Box::new(ZipCompressedReader::open(
            self.path.clone(),
            entry_name.into_owned(),
            length,
        )?))
    }
}

/// A GZIP-compressed TAR archive with lazy entry extraction.
pub struct TarGzArchiveVfs {
    path: PathBuf,
    cache: EntryCache,
}

impl TarGzArchiveVfs {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_cache(path, EntryCache::default())
    }

    pub fn with_cache(path: impl Into<PathBuf>, cache: EntryCache) -> Result<Self> {
        let path = path.into();
        File::open(&path)?;
        Ok(Self { path, cache })
    }

    pub fn cache_len(&self) -> io::Result<usize> {
        self.cache.len()
    }
}

impl ArchiveVfs for TarGzArchiveVfs {
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>> {
        let file = File::open(&self.path)?;
        let mut archive = Archive::new(GzDecoder::new(file));
        let mut entries = Vec::new();

        for entry in archive.entries()? {
            let entry = entry?;
            let path = entry.path()?.into_owned();
            if entry.header().entry_type().is_dir() {
                entries.push(ArchiveEntry::directory(path));
            } else {
                entries.push(ArchiveEntry::file(path, entry.size()));
            }
        }

        Ok(entries)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        if let Some(reader) = self.cache.get(path)? {
            return Ok(reader);
        }

        let file = File::open(&self.path)?;
        let mut archive = Archive::new(GzDecoder::new(file));
        let mut reader = None;

        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()?.as_ref() != path {
                continue;
            }
            if entry.header().entry_type().is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("archive entry is a directory: {}", path.display()),
                ));
            }

            reader = Some(entry);
            break;
        }

        let mut entry = reader.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive entry was not found: {}", path.display()),
            )
        })?;
        self.cache.insert_reader(path.to_owned(), &mut entry)
    }
}

#[derive(Debug, Clone, Copy)]
enum TarCompression {
    Plain,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

/// A TAR archive with selectable compression.
pub struct TarArchiveVfs {
    path: PathBuf,
    compression: TarCompression,
    cache: EntryCache,
}

impl TarArchiveVfs {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_compression(path, TarCompression::Plain, EntryCache::default())
    }

    pub fn open_bzip2(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_compression(path, TarCompression::Bzip2, EntryCache::default())
    }

    pub fn open_gzip(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_compression(path, TarCompression::Gzip, EntryCache::default())
    }

    pub fn open_xz(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_compression(path, TarCompression::Xz, EntryCache::default())
    }

    pub fn open_zstd(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_compression(path, TarCompression::Zstd, EntryCache::default())
    }

    fn with_compression(
        path: impl Into<PathBuf>,
        compression: TarCompression,
        cache: EntryCache,
    ) -> Result<Self> {
        let path = path.into();
        File::open(&path)?;
        Ok(Self {
            path,
            compression,
            cache,
        })
    }
}

impl ArchiveVfs for TarArchiveVfs {
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>> {
        match self.compression {
            TarCompression::Plain => tar_entries(File::open(&self.path)?),
            TarCompression::Gzip => tar_entries(GzDecoder::new(File::open(&self.path)?)),
            TarCompression::Bzip2 => tar_entries(BzDecoder::new(File::open(&self.path)?)),
            TarCompression::Xz => tar_entries(XzDecoder::new(File::open(&self.path)?)),
            TarCompression::Zstd => {
                tar_entries(zstd::stream::read::Decoder::new(File::open(&self.path)?)?)
            }
        }
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        if let Some(reader) = self.cache.get(path)? {
            return Ok(reader);
        }

        match self.compression {
            TarCompression::Plain => tar_open(File::open(&self.path)?, path, &self.cache),
            TarCompression::Gzip => {
                tar_open(GzDecoder::new(File::open(&self.path)?), path, &self.cache)
            }
            TarCompression::Bzip2 => {
                tar_open(BzDecoder::new(File::open(&self.path)?), path, &self.cache)
            }
            TarCompression::Xz => {
                tar_open(XzDecoder::new(File::open(&self.path)?), path, &self.cache)
            }
            TarCompression::Zstd => tar_open(
                zstd::stream::read::Decoder::new(File::open(&self.path)?)?,
                path,
                &self.cache,
            ),
        }
    }
}

fn tar_entries<R: Read>(reader: R) -> io::Result<Vec<ArchiveEntry>> {
    let mut archive = Archive::new(reader);
    let mut entries = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        if entry.header().entry_type().is_dir() {
            entries.push(ArchiveEntry::directory(path));
        } else {
            entries.push(ArchiveEntry::file(path, entry.size()));
        }
    }
    Ok(entries)
}

fn tar_open<R: Read>(reader: R, path: &Path, cache: &EntryCache) -> io::Result<Box<dyn ReadSeek>> {
    let mut archive = Archive::new(reader);
    let mut selected = None;
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() == path {
            selected = Some(entry);
            break;
        }
    }
    let mut entry = selected.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("archive entry was not found: {}", path.display()),
        )
    })?;
    if entry.header().entry_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("archive entry is a directory: {}", path.display()),
        ));
    }
    cache.insert_reader(path.to_owned(), &mut entry)
}

/// A 7z archive with lazy solid-block extraction and entry caching.
pub struct SevenZArchiveVfs {
    path: PathBuf,
    cache: EntryCache,
}

impl SevenZArchiveVfs {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_cache(path, EntryCache::default())
    }

    pub fn with_cache(path: impl Into<PathBuf>, cache: EntryCache) -> Result<Self> {
        let path = path.into();
        sevenz_rust2::Archive::open(&path).map_err(sevenz_error)?;
        Ok(Self { path, cache })
    }

    pub fn cache_len(&self) -> io::Result<usize> {
        self.cache.len()
    }
}

impl ArchiveVfs for SevenZArchiveVfs {
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>> {
        let archive = sevenz_rust2::Archive::open(&self.path).map_err(sevenz_error)?;
        Ok(archive
            .files
            .iter()
            .map(|entry| {
                let path = PathBuf::from(entry.name());
                if entry.is_directory() {
                    ArchiveEntry::directory(path)
                } else {
                    ArchiveEntry::file(path, entry.size())
                }
            })
            .collect())
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        if let Some(reader) = self.cache.get(path)? {
            return Ok(reader);
        }

        let target_name = {
            let archive = sevenz_rust2::Archive::open(&self.path).map_err(sevenz_error)?;
            let entry = archive
                .files
                .iter()
                .find(|entry| Path::new(entry.name()) == path)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("archive entry was not found: {}", path.display()),
                    )
                })?;
            if entry.is_directory() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("archive entry is a directory: {}", path.display()),
                ));
            }
            entry.name().to_owned()
        };

        let mut reader =
            sevenz_rust2::ArchiveReader::open(&self.path, sevenz_rust2::Password::empty())
                .map_err(sevenz_error)?;
        let mut materializer = Materializer::new(self.cache.memory_threshold);
        let mut found = false;
        reader
            .for_each_entries(
                &mut |entry: &sevenz_rust2::ArchiveEntry, source: &mut dyn Read| {
                    if entry.name() == target_name {
                        io::copy(source, &mut materializer)?;
                        found = true;
                        Ok(false)
                    } else {
                        io::copy(source, &mut io::sink())?;
                        Ok(true)
                    }
                },
            )
            .map_err(sevenz_error)?;

        if !found {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive entry was not found: {}", path.display()),
            ));
        }
        self.cache
            .insert_entry(path.to_owned(), materializer.finish()?)
    }
}

/// A RAR archive backed by the pure-Rust `rars` reader.
pub struct RarArchiveVfs {
    path: PathBuf,
    cache: EntryCache,
}

impl RarArchiveVfs {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_cache(path, EntryCache::default())
    }

    pub fn with_cache(path: impl Into<PathBuf>, cache: EntryCache) -> Result<Self> {
        let path = path.into();
        rars::ArchiveReader::read_path(&path).map_err(rars_error)?;
        Ok(Self { path, cache })
    }

    pub fn cache_len(&self) -> io::Result<usize> {
        self.cache.len()
    }
}

impl ArchiveVfs for RarArchiveVfs {
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>> {
        let archive = rars::ArchiveReader::read_path(&self.path).map_err(rars_error)?;
        Ok(archive
            .members()
            .map(|member| {
                let path = PathBuf::from(String::from_utf8_lossy(&member.meta.name).into_owned());
                if member.meta.is_directory {
                    ArchiveEntry::directory(path)
                } else {
                    ArchiveEntry::file(path, member.meta.unpacked_size)
                }
            })
            .collect())
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        if let Some(reader) = self.cache.get(path)? {
            return Ok(reader);
        }

        let name = path.to_string_lossy().into_owned().into_bytes();
        let archive = rars::ArchiveReader::read_path(&self.path).map_err(rars_error)?;
        let member = archive
            .members()
            .find(|member| member.meta.name == name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("archive entry was not found: {}", path.display()),
                )
            })?;
        if member.meta.is_directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("archive entry is a directory: {}", path.display()),
            ));
        }

        let materializer =
            std::sync::Arc::new(Mutex::new(Materializer::new(self.cache.memory_threshold)));
        let target = std::sync::Arc::clone(&materializer);
        archive
            .extract_to(None, |meta| {
                if meta.name_bytes() == name.as_slice() && !meta.is_directory {
                    Ok(Box::new(SharedMaterializerWriter {
                        materializer: std::sync::Arc::clone(&target),
                    }) as Box<dyn Write>)
                } else {
                    Ok(Box::new(io::sink()) as Box<dyn Write>)
                }
            })
            .map_err(rars_error)?;

        drop(target);
        let materializer = std::sync::Arc::try_unwrap(materializer)
            .map_err(|_| io::Error::other("RAR materializer is still in use"))?
            .into_inner()
            .map_err(|_| io::Error::other("RAR materializer was poisoned"))?;
        self.cache
            .insert_entry(path.to_owned(), materializer.finish()?)
    }
}

fn zip_error(error: zip::result::ZipError) -> io::Error {
    let kind = match error {
        zip::result::ZipError::FileNotFound => io::ErrorKind::NotFound,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, error)
}

fn sevenz_error(error: sevenz_rust2::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn rars_error(error: rars::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{CachedEntry, DEFAULT_MAX_CACHE_BYTES, DEFAULT_MEMORY_THRESHOLD, EntryCache};
    use std::io::{Cursor, Read};
    use std::num::NonZeroUsize;
    use std::path::Path;

    #[test]
    fn cache_evicts_least_recently_used_entry() {
        let cache = EntryCache::new(NonZeroUsize::new(2).expect("capacity is non-zero"));
        cache
            .insert_reader("first".into(), &mut Cursor::new(b"1".to_vec()))
            .expect("insert first entry");
        cache
            .insert_reader("second".into(), &mut Cursor::new(b"2".to_vec()))
            .expect("insert second entry");
        assert!(
            cache
                .get(Path::new("first"))
                .expect("get first entry")
                .is_some()
        );
        cache
            .insert_reader("third".into(), &mut Cursor::new(b"3".to_vec()))
            .expect("insert third entry");

        assert!(
            cache
                .get(Path::new("second"))
                .expect("get evicted entry")
                .is_none()
        );
        assert!(
            cache
                .get(Path::new("first"))
                .expect("get first entry")
                .is_some()
        );
        assert_eq!(cache.len().expect("read cache length"), 2);
    }

    #[test]
    fn new_cache_is_empty() {
        let cache = EntryCache::default();

        assert!(cache.is_empty().expect("read cache state"));
        assert_eq!(cache.capacity(), 256, "default cache holds 256 entries");
    }

    #[test]
    fn cache_capacity_is_configurable() {
        let cache = EntryCache::with_limits(
            NonZeroUsize::new(512).expect("capacity is non-zero"),
            DEFAULT_MEMORY_THRESHOLD,
            DEFAULT_MAX_CACHE_BYTES,
        );

        assert_eq!(cache.capacity(), 512, "configured entry limit is retained");
    }

    #[test]
    fn cache_evicts_entries_until_byte_limit_is_met() {
        let cache =
            EntryCache::with_limits(NonZeroUsize::new(4).expect("capacity is non-zero"), 1024, 5);
        cache
            .insert_reader("first".into(), &mut Cursor::new(b"1234".to_vec()))
            .expect("insert first entry");
        cache
            .insert_reader("second".into(), &mut Cursor::new(b"56".to_vec()))
            .expect("insert second entry");

        assert!(
            cache
                .get(Path::new("first"))
                .expect("get evicted entry")
                .is_none()
        );
        assert_eq!(cache.bytes().expect("read cached bytes"), 2);
    }

    #[test]
    fn large_entry_is_removed_when_cache_is_dropped() {
        let path = Path::new("large");
        let temporary_path = {
            let cache = EntryCache::with_memory_threshold(
                NonZeroUsize::new(1).expect("capacity is non-zero"),
                1,
            );
            cache
                .insert_reader(path.to_owned(), &mut Cursor::new(b"large".to_vec()))
                .expect("insert large entry");
            let entries = cache.entries.lock().expect("lock cache");
            let CachedEntry::Temporary(file) = entries.peek(path).expect("find cached entry")
            else {
                panic!("large entry must use temporary storage");
            };
            file.path().to_owned()
        };

        assert!(
            !temporary_path.exists(),
            "temporary cache file must be deleted after cache drop"
        );
    }

    #[test]
    fn large_entry_is_read_through_memory_map() {
        let cache = EntryCache::with_memory_threshold(
            NonZeroUsize::new(1).expect("capacity is non-zero"),
            1,
        );
        let mut reader = cache
            .insert_reader("large".into(), &mut Cursor::new(b"mapped data".to_vec()))
            .expect("insert large entry");
        let mut contents = String::new();
        reader
            .read_to_string(&mut contents)
            .expect("read mapped entry");

        assert_eq!(contents, "mapped data");
    }
}
