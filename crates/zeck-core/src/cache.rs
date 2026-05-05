use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
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
pub(crate) enum CacheError {
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

pub(crate) struct SqliteBlockCache(pub(crate) StdMutex<Connection>);

impl SqliteBlockCache {
    pub(crate) fn for_path(path: &std::path::Path) -> Result<Self, CacheError> {
        Ok(Self(StdMutex::new(Connection::open(path)?)))
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
