//! Lazy archive readers.
//!
//! Format-specific readers and lazy archive access.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak, mpsc};
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
const DEFAULT_MAX_BLOCK_CACHE_BYTES: usize = 256 * 1024 * 1024;

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

enum BlockStorage {
    Memory(Vec<u8>),
    Temporary {
        _file: tempfile::NamedTempFile,
        map: Mmap,
    },
}

impl BlockStorage {
    fn from_entry(entry: CachedEntry) -> io::Result<Self> {
        match entry {
            CachedEntry::Memory(contents) => Ok(Self::Memory(contents)),
            CachedEntry::Temporary(file) => {
                if file.as_file().metadata()?.len() == 0 {
                    return Ok(Self::Memory(Vec::new()));
                }
                let map = unsafe { Mmap::map(file.as_file())? };
                Ok(Self::Temporary { _file: file, map })
            }
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Memory(contents) => contents,
            Self::Temporary { map, .. } => map,
        }
    }

    fn len(&self) -> usize {
        self.bytes().len()
    }
}

struct DecodedBlock {
    storage: BlockStorage,
    ranges: HashMap<PathBuf, Range<usize>>,
}

impl DecodedBlock {
    fn reader(self: &Arc<Self>, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        let range = self.ranges.get(path).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "decoded 7z block does not contain entry: {}",
                    path.display()
                ),
            )
        })?;
        Ok(Box::new(BlockRangeReader {
            block: Arc::clone(self),
            range,
            position: 0,
        }))
    }
}

struct BlockRangeReader {
    block: Arc<DecodedBlock>,
    range: Range<usize>,
    position: usize,
}

impl Read for BlockRangeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position >= self.range.len() {
            return Ok(0);
        }

        let read_length = (self.range.len() - self.position).min(buffer.len());
        let start = self
            .range
            .start
            .checked_add(self.position)
            .ok_or_else(|| io::Error::other("7z block range overflowed"))?;
        let end = start
            .checked_add(read_length)
            .ok_or_else(|| io::Error::other("7z block read range overflowed"))?;
        buffer[..read_length].copy_from_slice(&self.block.storage.bytes()[start..end]);
        self.position += read_length;
        Ok(read_length)
    }
}

impl Seek for BlockRangeReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let current = i128::try_from(self.position).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "reader position overflowed")
        })?;
        let length = i128::try_from(self.range.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "entry length overflowed"))?;
        let next = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => current + i128::from(offset),
            SeekFrom::End(offset) => length + i128::from(offset),
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "7z block range seek moved before the beginning",
            ));
        }
        self.position = usize::try_from(next).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "7z block range seek overflowed",
            )
        })?;
        u64::try_from(self.position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek position overflowed"))
    }
}

struct BlockCacheState {
    blocks: LruCache<usize, Arc<DecodedBlock>>,
    current_bytes: usize,
}

struct BlockCache {
    state: Mutex<BlockCacheState>,
    loading: Mutex<HashMap<usize, Weak<Mutex<()>>>>,
    capacity: NonZeroUsize,
    max_bytes: usize,
}

impl BlockCache {
    fn new(capacity: NonZeroUsize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(BlockCacheState {
                blocks: LruCache::new(capacity),
                current_bytes: 0,
            }),
            loading: Mutex::new(HashMap::new()),
            capacity,
            max_bytes,
        }
    }

    fn get_or_load(
        &self,
        block_index: usize,
        load: impl FnOnce() -> io::Result<DecodedBlock>,
    ) -> io::Result<Arc<DecodedBlock>> {
        if let Some(block) = self.get(block_index)? {
            return Ok(block);
        }

        let block_lock = {
            let mut loading = self
                .loading
                .lock()
                .map_err(|_| io::Error::other("7z block load map was poisoned"))?;
            loading.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = loading.get(&block_index).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                loading.insert(block_index, Arc::downgrade(&lock));
                lock
            }
        };
        let _load_guard = block_lock
            .lock()
            .map_err(|_| io::Error::other("7z block load lock was poisoned"))?;

        if let Some(block) = self.get(block_index)? {
            return Ok(block);
        }

        let block = Arc::new(load()?);
        let block_bytes = block.storage.len();
        if block_bytes <= self.max_bytes {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("7z block cache was poisoned"))?;
            while state.blocks.len() >= self.capacity.get()
                || state.current_bytes.saturating_add(block_bytes) > self.max_bytes
            {
                let Some((_, evicted)) = state.blocks.pop_lru() else {
                    break;
                };
                state.current_bytes = state.current_bytes.saturating_sub(evicted.storage.len());
            }
            if state.current_bytes.saturating_add(block_bytes) <= self.max_bytes {
                state.current_bytes = state.current_bytes.saturating_add(block_bytes);
                state.blocks.put(block_index, Arc::clone(&block));
            }
        }
        Ok(block)
    }

    fn get(&self, block_index: usize) -> io::Result<Option<Arc<DecodedBlock>>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("7z block cache was poisoned"))?;
        Ok(state.blocks.get(&block_index).cloned())
    }

    fn cached_entry_count(&self) -> io::Result<usize> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("7z block cache was poisoned"))?;
        Ok(state
            .blocks
            .iter()
            .map(|(_, block)| block.ranges.len())
            .sum())
    }
}

struct Materializer {
    memory_threshold: usize,
    memory: Vec<u8>,
    temporary: Option<tempfile::NamedTempFile>,
    bytes_written: usize,
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
            bytes_written: 0,
        }
    }

    fn len(&self) -> usize {
        self.bytes_written
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
        let new_len = self
            .bytes_written
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("materialized archive data length overflowed"))?;
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
        self.bytes_written = new_len;
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
    max_block_bytes: usize,
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
        Self::with_cache_limits(capacity, memory_threshold, max_bytes, max_bytes)
    }

    fn with_cache_limits(
        capacity: NonZeroUsize,
        memory_threshold: usize,
        max_bytes: usize,
        max_block_bytes: usize,
    ) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(capacity)),
            capacity,
            memory_threshold,
            max_bytes,
            max_block_bytes,
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
        Self::with_cache_limits(
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).expect("default cache capacity is non-zero"),
            DEFAULT_MEMORY_THRESHOLD,
            DEFAULT_MAX_CACHE_BYTES,
            DEFAULT_MAX_BLOCK_CACHE_BYTES,
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

#[derive(Clone, Copy)]
struct SevenZEntryLocation {
    file_index: usize,
    block_index: Option<usize>,
    is_directory: bool,
}

/// A 7z archive with lazy solid-block extraction and block caching.
pub struct SevenZArchiveVfs {
    path: PathBuf,
    archive: sevenz_rust2::Archive,
    entries: HashMap<PathBuf, SevenZEntryLocation>,
    cache: BlockCache,
    memory_threshold: usize,
}

impl SevenZArchiveVfs {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_cache(path, EntryCache::default())
    }

    pub fn with_cache(path: impl Into<PathBuf>, cache: EntryCache) -> Result<Self> {
        let path = path.into();
        let archive = sevenz_rust2::Archive::open(&path).map_err(sevenz_error)?;
        let mut entries = HashMap::with_capacity(archive.files.len());
        for (file_index, entry) in archive.files.iter().enumerate() {
            let path = PathBuf::from(entry.name());
            let block_index = archive
                .stream_map
                .file_block_index
                .get(file_index)
                .copied()
                .flatten();
            entries.entry(path).or_insert(SevenZEntryLocation {
                file_index,
                block_index,
                is_directory: entry.is_directory(),
            });
        }

        Ok(Self {
            path,
            archive,
            entries,
            cache: BlockCache::new(cache.capacity, cache.max_block_bytes),
            memory_threshold: cache.memory_threshold,
        })
    }

    /// Returns the number of entries represented by cached decoded blocks.
    pub fn cache_len(&self) -> io::Result<usize> {
        self.cache.cached_entry_count()
    }

    fn materialize_block(&self, block_index: usize) -> io::Result<DecodedBlock> {
        let mut source = File::open(&self.path)?;
        let thread_count =
            thread::available_parallelism().map_or(1, |count| count.get().min(256) as u32);
        let password = sevenz_rust2::Password::empty();
        let decoder = sevenz_rust2::BlockDecoder::new(
            thread_count,
            block_index,
            &self.archive,
            &password,
            &mut source,
        );
        let first_file_index = *self
            .archive
            .stream_map
            .block_first_file_index
            .get(block_index)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "7z block index is invalid")
            })?;
        let file_count = decoder.entry_count();
        let block_end = first_file_index.checked_add(file_count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "7z block file range overflowed")
        })?;
        let mut file_index = first_file_index;
        let mut too_many_entries = false;
        let mut materializer = Materializer::new(self.memory_threshold);
        let mut ranges = HashMap::with_capacity(file_count);

        decoder
            .for_each_entries(&mut |entry, reader| {
                if file_index >= block_end {
                    too_many_entries = true;
                    return Ok(false);
                }
                let start = materializer.len();
                io::copy(reader, &mut materializer)?;
                let end = materializer.len();
                let actual_size = u64::try_from(end - start).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "7z entry size overflowed")
                })?;
                if actual_size != entry.size() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("decoded size mismatch for 7z entry: {}", entry.name()),
                    )
                    .into());
                }
                let path = PathBuf::from(entry.name());
                if self.entries.get(&path).is_some_and(|location| {
                    location.file_index == file_index && location.block_index == Some(block_index)
                }) {
                    ranges.insert(path, start..end);
                }
                file_index += 1;
                Ok(true)
            })
            .map_err(sevenz_error)?;

        if too_many_entries || file_index != block_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "7z block returned fewer entries than declared",
            ));
        }

        Ok(DecodedBlock {
            storage: BlockStorage::from_entry(materializer.finish()?)?,
            ranges,
        })
    }
}

impl ArchiveVfs for SevenZArchiveVfs {
    fn entries(&self) -> io::Result<Vec<ArchiveEntry>> {
        Ok(self
            .archive
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
        let location = self.entries.get(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive entry was not found: {}", path.display()),
            )
        })?;
        if location.is_directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("archive entry is a directory: {}", path.display()),
            ));
        }

        let Some(block_index) = location.block_index else {
            return Ok(Box::new(io::Cursor::new(Vec::new())));
        };
        let block = self
            .cache
            .get_or_load(block_index, || self.materialize_block(block_index))?;
        block.reader(path)
    }
}

/// A RAR archive backed by the pure-Rust `rars` reader.
pub struct RarArchiveVfs {
    path: PathBuf,
    cache: EntryCache,
    materialization_lock: Mutex<()>,
}

impl RarArchiveVfs {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_cache(path, EntryCache::default())
    }

    pub fn with_cache(path: impl Into<PathBuf>, cache: EntryCache) -> Result<Self> {
        let path = path.into();
        rars::ArchiveReader::read_path(&path).map_err(rars_error)?;
        Ok(Self {
            path,
            cache,
            materialization_lock: Mutex::new(()),
        })
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

        let _materialization_guard = self
            .materialization_lock
            .lock()
            .map_err(|_| io::Error::other("RAR materialization was poisoned"))?;
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

        let materializers: HashMap<_, _> = archive
            .members()
            .filter(|member| !member.meta.is_directory)
            .map(|member| {
                (
                    member.meta.name.clone(),
                    std::sync::Arc::new(Mutex::new(Materializer::new(self.cache.memory_threshold))),
                )
            })
            .collect();
        let targets = materializers.clone();
        archive
            .extract_to(None, |meta| {
                if let Some(materializer) = targets.get(meta.name_bytes()) {
                    Ok(Box::new(SharedMaterializerWriter {
                        materializer: std::sync::Arc::clone(materializer),
                    }) as Box<dyn Write>)
                } else {
                    Ok(Box::new(io::sink()) as Box<dyn Write>)
                }
            })
            .map_err(rars_error)?;

        drop(targets);
        let mut target_reader = None;
        for (entry_name, materializer) in materializers {
            let materializer = std::sync::Arc::try_unwrap(materializer)
                .map_err(|_| io::Error::other("RAR materializer is still in use"))?
                .into_inner()
                .map_err(|_| io::Error::other("RAR materializer was poisoned"))?;
            let entry_path = PathBuf::from(String::from_utf8_lossy(&entry_name).into_owned());
            let reader = self
                .cache
                .insert_entry(entry_path.clone(), materializer.finish()?)?;
            if entry_path == path {
                target_reader = Some(reader);
            }
        }

        target_reader.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("archive entry was not materialized: {}", path.display()),
            )
        })
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
    use super::{
        BlockCache, BlockStorage, CachedEntry, DEFAULT_MAX_CACHE_BYTES, DEFAULT_MEMORY_THRESHOLD,
        DecodedBlock, EntryCache,
    };
    use std::collections::HashMap;
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    use std::num::NonZeroUsize;
    use std::ops::Range;
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    fn memory_block(path: &str, contents: &[u8]) -> DecodedBlock {
        let mut ranges = HashMap::<PathBuf, Range<usize>>::new();
        ranges.insert(PathBuf::from(path), 0..contents.len());
        DecodedBlock {
            storage: BlockStorage::Memory(contents.to_vec()),
            ranges,
        }
    }

    #[test]
    fn block_range_reader_supports_start_current_and_end_seek() {
        let mut block = memory_block("entry", b"__abcde");
        block.ranges.insert(PathBuf::from("entry"), 2..7);
        let block = Arc::new(block);
        let mut reader = block
            .reader(Path::new("entry"))
            .expect("create block range reader");

        assert_eq!(reader.seek(SeekFrom::Start(1)).expect("seek from start"), 1);
        let mut pair = [0; 2];
        reader.read_exact(&mut pair).expect("read after start seek");
        assert_eq!(&pair, b"bc");

        assert_eq!(
            reader
                .seek(SeekFrom::Current(-1))
                .expect("seek from current"),
            2
        );
        let mut byte = [0];
        reader
            .read_exact(&mut byte)
            .expect("read after current seek");
        assert_eq!(&byte, b"c");

        assert_eq!(reader.seek(SeekFrom::End(-2)).expect("seek from end"), 3);
        let mut suffix = [0; 2];
        reader.read_exact(&mut suffix).expect("read after end seek");
        assert_eq!(&suffix, b"de");
    }

    #[test]
    fn block_cache_evicts_by_decoded_bytes() {
        let cache = BlockCache::new(NonZeroUsize::new(4).expect("non-zero capacity"), 4);
        cache
            .get_or_load(0, || Ok(memory_block("first", b"abc")))
            .expect("cache first block");
        cache
            .get_or_load(1, || Ok(memory_block("second", b"def")))
            .expect("cache second block");
        assert_eq!(
            cache.cached_entry_count().expect("count cached entries"),
            1,
            "the byte budget must evict the least-recently-used block"
        );

        let mut loads = 0;
        cache
            .get_or_load(0, || {
                loads += 1;
                Ok(memory_block("first", b"abc"))
            })
            .expect("reload evicted block");
        assert_eq!(loads, 1, "an evicted block must be decoded again");
    }

    #[test]
    fn concurrent_block_cache_misses_only_load_once() {
        let cache = Arc::new(BlockCache::new(
            NonZeroUsize::new(4).expect("non-zero capacity"),
            16,
        ));
        let barrier = Arc::new(Barrier::new(3));
        let loads = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                let loads = Arc::clone(&loads);
                std::thread::spawn(move || {
                    barrier.wait();
                    cache
                        .get_or_load(0, || {
                            loads.fetch_add(1, Ordering::SeqCst);
                            Ok(memory_block("entry", b"block data"))
                        })
                        .expect("load shared block")
                })
            })
            .collect();
        barrier.wait();

        for worker in workers {
            worker.join().expect("join block cache worker");
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "concurrent misses must share one block materialization"
        );
    }

    #[test]
    fn temporary_block_storage_lives_until_the_last_reader_drops() {
        let mut file = tempfile::NamedTempFile::new().expect("create block tempfile");
        file.write_all(b"__abcde").expect("write block tempfile");
        let path = file.path().to_owned();
        let storage = BlockStorage::from_entry(CachedEntry::Temporary(file))
            .expect("map temporary block storage");
        let block = Arc::new(DecodedBlock {
            storage,
            ranges: HashMap::from([(PathBuf::from("entry"), 2..7)]),
        });
        let reader = block
            .reader(Path::new("entry"))
            .expect("create temporary-backed reader");

        drop(block);
        assert!(
            path.exists(),
            "open readers must retain their block backing"
        );
        drop(reader);
        assert!(
            !path.exists(),
            "temporary block storage must be removed after its final reader drops"
        );
    }

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
