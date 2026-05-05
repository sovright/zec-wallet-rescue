use std::fs::{self, File, OpenOptions};
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use fs2::FileExt;
use prost::Message;
use rusqlite::{params, Connection};
use zcash_client_backend::{
    data_api::{
        chain::{error::Error as ChainError, BlockCache, BlockSource},
        scanning::ScanRange,
    },
    proto::compact_formats::CompactBlock,
};
use zcash_protocol::consensus::BlockHeight;

#[derive(Debug)]
pub enum CacheError {
    Db(rusqlite::Error),
    Decode(prost::DecodeError),
    MissingBlock(BlockHeight),
    Corrupted(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(err) => write!(f, "{err}"),
            Self::Decode(err) => write!(f, "{err}"),
            Self::MissingBlock(height) => write!(f, "missing compact block at height {height}"),
            Self::Corrupted(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<rusqlite::Error> for CacheError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Db(value)
    }
}

impl From<prost::DecodeError> for CacheError {
    fn from(value: prost::DecodeError) -> Self {
        Self::Decode(value)
    }
}

pub struct SqliteBlockCache(pub(crate) StdMutex<Connection>);

impl SqliteBlockCache {
    pub(crate) fn for_path(path: &std::path::Path) -> Result<Self, CacheError> {
        Ok(Self(StdMutex::new(Connection::open(path)?)))
    }

    pub(crate) fn set_journal_mode_wal(&self) -> Result<(), CacheError> {
        let conn = self.0.lock().expect("cache mutex poisoned");
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // synchronous=NORMAL: on crash, up to one in-flight batch may need
        // re-downloading. The cache is purely a download accelerator (no wallet
        // state lives here), so trading durability for write speed is fine.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }
}

impl BlockSource for SqliteBlockCache {
    type Error = CacheError;

    fn with_blocks<F, DbErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_row: F,
    ) -> Result<(), ChainError<DbErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<DbErrT, Self::Error>>,
    {
        fn to_chain_error<DbErrT>(err: CacheError) -> ChainError<DbErrT, CacheError> {
            ChainError::BlockSource(err)
        }

        let start_height = from_height.map_or(0u32, u32::from);
        let row_limit = limit
            .and_then(|limit| u32::try_from(limit).ok())
            .unwrap_or(u32::MAX);
        let guard = self
            .0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))
            .map_err(to_chain_error)?;
        let mut stmt = guard
            .prepare(
                "SELECT height, data FROM compactblocks
                 WHERE height >= ?
                 ORDER BY height ASC LIMIT ?",
            )
            .map_err(CacheError::from)
            .map_err(to_chain_error)?;
        let mut rows = stmt
            .query(params![start_height, row_limit])
            .map_err(CacheError::from)
            .map_err(to_chain_error)?;

        let mut expected = from_height;
        while let Some(row) = rows
            .next()
            .map_err(CacheError::from)
            .map_err(to_chain_error)?
        {
            let height = BlockHeight::from_u32(
                row.get::<_, u32>(0)
                    .map_err(CacheError::from)
                    .map_err(to_chain_error)?,
            );
            if let Some(expected_height) = expected {
                if height != expected_height {
                    return Err(to_chain_error(CacheError::MissingBlock(expected_height)));
                }
            }
            let data = row
                .get::<_, Vec<u8>>(1)
                .map_err(CacheError::from)
                .map_err(to_chain_error)?;
            let block = CompactBlock::decode(&data[..])
                .map_err(CacheError::from)
                .map_err(to_chain_error)?;
            if block.height() != height {
                return Err(to_chain_error(CacheError::Corrupted(format!(
                    "cached block height {} did not match row height {}",
                    block.height(),
                    height
                ))));
            }
            with_row(block)?;
            expected = expected.map(|height| height + 1);
        }

        if let Some(expected_height) = expected {
            if expected_height == from_height.unwrap_or(BlockHeight::from_u32(start_height)) {
                return Err(to_chain_error(CacheError::MissingBlock(expected_height)));
            }
        }

        Ok(())
    }
}

#[async_trait]
impl BlockCache for SqliteBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let (start_height, end_height) = range
            .map(|range: &ScanRange| {
                (
                    u32::from(range.block_range().start),
                    u32::from(range.block_range().end),
                )
            })
            .unwrap_or((0, u32::MAX));

        self.0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?
            .query_row(
                "SELECT MAX(height) FROM compactblocks WHERE height >= ? AND height < ?",
                params![start_height, end_height],
                |row| row.get::<_, Option<u32>>(0),
            )
            .map(|height| height.map(BlockHeight::from_u32))
            .map_err(CacheError::from)
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let mut blocks = Vec::new();
        let start = range.block_range().start;
        let end = range.block_range().end;
        let guard = self
            .0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?;
        let mut stmt = guard.prepare(
            "SELECT height, data FROM compactblocks
             WHERE height >= ? AND height < ?
             ORDER BY height ASC",
        )?;
        let mut rows = stmt.query(params![u32::from(start), u32::from(end)])?;
        let mut expected = start;

        while let Some(row) = rows.next()? {
            let height = BlockHeight::from_u32(row.get(0)?);
            if height != expected {
                if blocks.is_empty() {
                    return Err(CacheError::MissingBlock(expected));
                }
                break;
            }
            let data: Vec<u8> = row.get(1)?;
            let block = CompactBlock::decode(&data[..])?;
            blocks.push(block);
            expected = expected + 1;
        }

        Ok(blocks)
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let guard = self
            .0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?;
        let mut stmt = guard.prepare(
            "INSERT INTO compactblocks(height, data)
             VALUES (?, ?)
             ON CONFLICT(height) DO UPDATE SET data = excluded.data",
        )?;
        guard.execute("BEGIN IMMEDIATE", [])?;
        let result = compact_blocks.iter().try_for_each(|block| {
            stmt.execute(params![u32::from(block.height()), block.encode_to_vec()])?;
            Ok::<_, rusqlite::Error>(())
        });
        drop(stmt);
        match result {
            Ok(()) => {
                guard.execute("COMMIT", [])?;
                Ok(())
            }
            Err(err) => {
                let _ = guard.execute("ROLLBACK", []);
                Err(CacheError::from(err))
            }
        }
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        self.0
            .lock()
            .map_err(|_| CacheError::Corrupted("block cache mutex was poisoned".to_owned()))?
            .execute(
                "DELETE FROM compactblocks WHERE height >= ? AND height < ?",
                params![
                    u32::from(range.block_range().start),
                    u32::from(range.block_range().end)
                ],
            )?;
        Ok(())
    }
}

// ─── Shared cache open error ───────────────────────────────────────────────

#[derive(Debug)]
pub enum CacheOpenError {
    /// Another process holds the exclusive lock on the `.lock` file.
    Locked,
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Cache(CacheError),
}

impl std::fmt::Display for CacheOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked => write!(f, "block cache is locked by another process"),
            Self::Io(err) => write!(f, "I/O error opening block cache: {err}"),
            Self::Sqlite(err) => write!(f, "SQLite error opening block cache: {err}"),
            Self::Cache(err) => write!(f, "block cache error: {err}"),
        }
    }
}

impl std::error::Error for CacheOpenError {}

impl From<std::io::Error> for CacheOpenError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<rusqlite::Error> for CacheOpenError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<CacheError> for CacheOpenError {
    fn from(value: CacheError) -> Self {
        Self::Cache(value)
    }
}

// ─── Shared cache writer ───────────────────────────────────────────────────

/// Exclusive writer for a shared block cache.
///
/// Holds an OS-level advisory lock on a sibling `.lock` file for the lifetime
/// of the value.  Dropping `SharedCacheWriter` releases the lock and closes the
/// database connection.
pub struct SharedCacheWriter {
    pub(crate) cache: SqliteBlockCache,
    /// The lock is released when this `File` is dropped (RAII).
    _lock_file: File,
}

impl std::fmt::Debug for SharedCacheWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedCacheWriter").finish_non_exhaustive()
    }
}

impl SharedCacheWriter {
    /// Open (or create) the block-cache database at `db_path`, acquiring an
    /// exclusive advisory lock on `lock_path`.
    ///
    /// Returns `Err(CacheOpenError::Locked)` immediately if another process
    /// already holds the lock.
    pub fn open(
        db_path: &std::path::Path,
        lock_path: &std::path::Path,
    ) -> Result<Self, CacheOpenError> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;

        #[allow(deprecated)]
        match lock_file.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(CacheOpenError::Locked)
            }
            Err(e) => return Err(CacheOpenError::Io(e)),
        }

        let cache = SqliteBlockCache::for_path(db_path)?;
        cache.set_journal_mode_wal()?;
        Ok(Self {
            cache,
            _lock_file: lock_file,
        })
    }

    /// Borrow the underlying block cache.
    pub fn cache(&self) -> &SqliteBlockCache {
        &self.cache
    }
}

// ─── Shared cache reader ───────────────────────────────────────────────────

/// Read-only handle to a shared block cache.
///
/// Does not acquire any lock — multiple readers may coexist with the single
/// writer because SQLite WAL mode allows concurrent reads.
pub struct SharedCacheReader {
    pub(crate) cache: SqliteBlockCache,
}

impl SharedCacheReader {
    /// Open the block-cache database at `db_path` for reading.
    pub fn open(db_path: &std::path::Path) -> Result<Self, CacheOpenError> {
        let cache = SqliteBlockCache::for_path(db_path)?;
        Ok(Self { cache })
    }

    /// Borrow the underlying block cache.
    pub fn cache(&self) -> &SqliteBlockCache {
        &self.cache
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod shared_cache_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_shared_cache_creates_dir_and_db() {
        let dir = tempdir().unwrap();
        let db = dir
            .path()
            .join("cache")
            .join("mainnet")
            .join("blocks.sqlite");
        let lock = dir
            .path()
            .join("cache")
            .join("mainnet")
            .join("blocks.lock");
        let _writer = SharedCacheWriter::open(&db, &lock).unwrap();
        assert!(db.exists());
        assert!(lock.exists());
    }

    #[test]
    fn second_writer_fails_while_first_held() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("blocks.sqlite");
        let lock = dir.path().join("blocks.lock");
        let _w1 = SharedCacheWriter::open(&db, &lock).unwrap();
        let err = SharedCacheWriter::open(&db, &lock).unwrap_err();
        assert!(matches!(err, CacheOpenError::Locked));
    }

    #[test]
    fn second_writer_succeeds_after_first_dropped() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("blocks.sqlite");
        let lock = dir.path().join("blocks.lock");
        {
            let _w1 = SharedCacheWriter::open(&db, &lock).unwrap();
        }
        let _w2 = SharedCacheWriter::open(&db, &lock).unwrap();
    }

    #[test]
    fn reader_does_not_acquire_lock() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("blocks.sqlite");
        let lock = dir.path().join("blocks.lock");
        let _writer = SharedCacheWriter::open(&db, &lock).unwrap();
        let _reader = SharedCacheReader::open(&db).unwrap();
    }
}
