pub mod error;
pub mod types;

use std::vec;

use include_dir::{include_dir, Dir};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Transaction};
use rusqlite_migration::Migrations;
use slog::{trace, warn, Logger};
use types::{
    DbTransferType, IncomingPath, IncomingPathStateEvent, IncomingPathStateEventData, OutgoingPath,
    OutgoingPathStateEvent, OutgoingPathStateEventData, Transfer, TransferFiles,
    TransferIncomingPath, TransferOutgoingPath, TransferStateEvent,
};
use uuid::Uuid;

use crate::error::Error;
pub use crate::types::{Event, FileChecksum, TransferInfo, TransferType};

type Result<T> = std::result::Result<T, Error>;
type QueryResult<T> = std::result::Result<T, rusqlite::Error>;
// SQLite storage wrapper
pub struct Storage {
    pool: Pool<SqliteConnectionManager>,
    logger: Logger,
}

const MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

impl Storage {
    pub fn new(logger: Logger, path: &str) -> Result<Self> {
        let manager = match path {
            ":memory:" => SqliteConnectionManager::memory(),
            _ => SqliteConnectionManager::file(path),
        };
        let pool = Pool::new(manager)?;

        let mut conn = pool.get()?;
        Migrations::from_directory(&MIGRATIONS_DIR)
            .map_err(|e| {
                Error::InternalError(format!("Failed to gather migrations from directory: {e}"))
            })?
            .to_latest(&mut conn)
            .map_err(|e| Error::InternalError(format!("Failed to run migrations: {e}")))?;

        Ok(Self { logger, pool })
    }

    pub fn insert_transfer(&self, transfer: &TransferInfo) -> Result<()> {
        let transfer_type_int = match &transfer.files {
            TransferFiles::Incoming(_) => TransferType::Incoming as u32,
            TransferFiles::Outgoing(_) => TransferType::Outgoing as u32,
        };

        let tid = transfer.id.to_string();
        trace!(
            self.logger,
            "Inserting transfer";
            "transfer_id" => &tid,
            "transfer_type" => transfer_type_int,
        );

        let mut conn = self.pool.get()?;
        let conn = conn.transaction()?;

        conn.execute(
            "INSERT INTO transfers (id, peer, is_outgoing) VALUES (?1, ?2, ?3)",
            params![tid, transfer.peer, transfer_type_int],
        )?;

        match &transfer.files {
            TransferFiles::Incoming(files) => {
                trace!(
                    self.logger,
                    "Inserting transfer::Incoming files len {}",
                    files.len()
                );

                for file in files {
                    Self::insert_incoming_path(&conn, transfer.id, file)?;
                }
            }
            TransferFiles::Outgoing(files) => {
                trace!(
                    self.logger,
                    "Inserting transfer::Outgoing files len {}",
                    files.len()
                );

                for file in files {
                    Self::insert_outgoing_path(&conn, transfer.id, file)?;
                }
            }
        }

        conn.commit()?;

        Ok(())
    }

    fn insert_incoming_path(
        conn: &Transaction<'_>,
        transfer_id: Uuid,
        path: &TransferIncomingPath,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        conn.execute(
            "INSERT INTO incoming_paths (transfer_id, relative_path, path_hash, bytes)
            VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            params![tid, path.relative_path, path.file_id, path.size],
        )?;

        Ok(())
    }

    fn insert_outgoing_path(
        conn: &Transaction<'_>,
        transfer_id: Uuid,
        path: &TransferOutgoingPath,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        conn.execute(
            "INSERT INTO outgoing_paths (transfer_id, relative_path, path_hash, bytes, base_path)
            VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                tid,
                path.relative_path,
                path.file_id,
                path.size,
                path.base_path
            ],
        )?;

        Ok(())
    }

    pub fn save_checksum(&self, transfer_id: Uuid, file_id: &str, checksum: &[u8]) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Saving checksum";
            "transfer_id" => &tid,
            "file_id" => file_id,
        );

        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE incoming_paths SET checksum = ?3 WHERE transfer_id = ?1 AND path_hash = ?2",
            params![tid, file_id, checksum],
        )?;

        Ok(())
    }

    pub fn fetch_checksums(&self, transfer_id: Uuid) -> Result<Vec<FileChecksum>> {
        let tid = transfer_id.to_string();
        trace!(
            self.logger,
            "Fetching checksums";
            "transfer_id" => &tid);

        let conn = self.pool.get()?;
        let out = conn
            .prepare(
                "SELECT path_hash as file_id, checksum FROM incoming_paths WHERE transfer_id = ?1",
            )?
            .query_map(params![tid], |row| {
                Ok(FileChecksum {
                    file_id: row.get("file_id")?,
                    checksum: row.get("checksum")?,
                })
            })?
            .collect::<QueryResult<Vec<_>>>()?;

        Ok(out)
    }

    pub fn insert_transfer_active_state(&self, transfer_id: Uuid) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting transfer active state";
            "transfer_id" => &tid);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO transfer_active_states (transfer_id) VALUES (?1)",
            params![tid],
        )?;

        Ok(())
    }

    pub fn insert_transfer_failed_state(&self, transfer_id: Uuid, error: u32) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting transfer failed state";
            "transfer_id" => &tid,
            "error" => error);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO transfer_failed_states (transfer_id, status_code) VALUES (?1, ?2)",
            params![tid, error],
        )?;

        Ok(())
    }

    pub fn insert_transfer_cancel_state(&self, transfer_id: Uuid, by_peer: bool) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting transfer cancel state";
            "transfer_id" => &tid,
            "by_peer" => by_peer);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO transfer_cancel_states (transfer_id, by_peer) VALUES (?1, ?2)",
            params![tid, by_peer],
        )?;

        Ok(())
    }

    pub fn insert_outgoing_path_pending_state(
        &self,
        transfer_id: Uuid,
        file_id: &str,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting outgoing path pending state";
            "transfer_id" => &tid,
            "file_id" => file_id);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outgoing_path_pending_states (path_id) VALUES ((SELECT id FROM \
             outgoing_paths WHERE transfer_id = ?1 AND path_hash = ?2))",
            params![tid, file_id],
        )?;

        Ok(())
    }

    pub fn insert_incoming_path_pending_state(
        &self,
        transfer_id: Uuid,
        file_id: &str,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting incoming path pending state";
            "transfer_id" => &tid,
            "file_id" => file_id);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO incoming_path_pending_states (path_id) VALUES ((SELECT id FROM \
             incoming_paths WHERE transfer_id = ?1 AND path_hash = ?2))",
            params![tid, file_id],
        )?;

        Ok(())
    }

    pub fn insert_outgoing_path_started_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting outgoing path started state";
            "transfer_id" => &tid,
            "path_id" => path_id);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outgoing_path_started_states (path_id, bytes_sent) VALUES ((SELECT id \
             FROM outgoing_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3)",
            params![tid, path_id, 0],
        )?;

        Ok(())
    }

    pub fn insert_incoming_path_started_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        base_dir: &str,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting incoming path started state";
            "transfer_id" => &tid,
            "path_id" => path_id,
            "base_dir" => base_dir);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO incoming_path_started_states (path_id, base_dir, bytes_received) VALUES \
             ((SELECT id FROM incoming_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3, ?4)",
            params![tid, path_id, base_dir, 0],
        )?;

        Ok(())
    }

    pub fn insert_outgoing_path_cancel_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        by_peer: bool,
        bytes_sent: i64,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting outgoing path cancel state";
            "transfer_id" => &tid,
            "path_id" => path_id,
            "by_peer" => by_peer,
            "bytes_sent" => bytes_sent);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outgoing_path_cancel_states (path_id, by_peer, bytes_sent) VALUES \
             ((SELECT id FROM outgoing_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3, ?4)",
            params![tid, path_id, by_peer, bytes_sent],
        )?;

        Ok(())
    }

    pub fn insert_incoming_path_cancel_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        by_peer: bool,
        bytes_received: i64,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting incoming path cancel state";
            "transfer_id" => &tid,
            "path_id" => path_id,
            "by_peer" => by_peer,
            "bytes_received" => bytes_received);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO incoming_path_cancel_states (path_id, by_peer, bytes_received) VALUES \
             ((SELECT id FROM incoming_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3, ?4)",
            params![tid, path_id, by_peer, bytes_received],
        )?;

        Ok(())
    }

    pub fn insert_incoming_path_failed_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        error: u32,
        bytes_received: i64,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Inserting incoming path failed state";
            "transfer_id" => &tid,
            "path_id" => path_id,
            "error" => error,
            "bytes_received" => bytes_received);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO incoming_path_failed_states (path_id, status_code, bytes_received) \
             VALUES ((SELECT id FROM incoming_paths WHERE transfer_id = ?1 AND path_hash = ?2), \
             ?3, ?4)",
            params![tid, path_id, error, bytes_received],
        )?;

        Ok(())
    }

    pub fn insert_outgoing_path_failed_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        error: u32,
        bytes_sent: i64,
    ) -> Result<()> {
        let tid = transfer_id.to_string();
        trace!(
            self.logger,
            "Inserting outgoing path failed state";
            "transfer_id" => &tid,
            "path_id" => path_id,
            "error" => error,
            "bytes_sent" => bytes_sent);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outgoing_path_failed_states (path_id, status_code, bytes_sent) VALUES \
             ((SELECT id FROM outgoing_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3, ?4)",
            params![tid, path_id, error, bytes_sent],
        )?;

        Ok(())
    }

    pub fn insert_outgoing_path_completed_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
    ) -> Result<()> {
        let tid = transfer_id.to_string();
        trace!(
            self.logger,
            "Inserting outgoing path completed state";
            "transfer_id" => &tid,
            "path_id" => path_id);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outgoing_path_completed_states (path_id) VALUES ((SELECT id FROM \
             outgoing_paths WHERE transfer_id = ?1 AND path_hash = ?2))",
            params![tid, path_id],
        )?;

        Ok(())
    }

    pub fn insert_incoming_path_completed_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        final_path: &str,
    ) -> Result<()> {
        let tid = transfer_id.to_string();
        trace!(
            self.logger,
            "Inserting incoming path completed state";
            "transfer_id" => &tid,
            "path_id" => path_id,
            "final_path" => final_path);

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO incoming_path_completed_states (path_id, final_path) VALUES ((SELECT id \
             FROM incoming_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3)",
            params![tid, path_id, final_path],
        )?;

        Ok(())
    }

    pub fn insert_outgoing_path_reject_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        by_peer: bool,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outgoing_path_reject_states (path_id, by_peer) VALUES ((SELECT id FROM \
             outgoing_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3)",
            params![tid, path_id, by_peer],
        )?;

        Ok(())
    }

    pub fn insert_incoming_path_reject_state(
        &self,
        transfer_id: Uuid,
        path_id: &str,
        by_peer: bool,
    ) -> Result<()> {
        let tid = transfer_id.to_string();

        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO incoming_path_reject_states (path_id, by_peer) VALUES ((SELECT id FROM \
             incoming_paths WHERE transfer_id = ?1 AND path_hash = ?2), ?3)",
            params![tid, path_id, by_peer],
        )?;

        Ok(())
    }

    pub fn purge_transfers_until(&self, until_timestamp: i64) -> Result<()> {
        let conn = self.pool.get()?;

        trace!(
            self.logger,
            "Purging transfers until timestamp";
            "until_timestamp" => until_timestamp);

        conn.execute(
            "DELETE FROM transfers WHERE created_at < datetime(?1, 'unixepoch')",
            params![until_timestamp],
        )?;

        Ok(())
    }

    fn purge_transfer(&self, transfer_id: String) -> Result<()> {
        let conn = self.pool.get()?;

        trace!(
            self.logger,
            "Purging transfer";
            "transfer_id" => transfer_id.clone());

        conn.execute("DELETE FROM transfers WHERE id = ?1", params![transfer_id])?;

        Ok(())
    }

    pub fn purge_transfers(&self, transfer_ids: Vec<String>) -> Result<()> {
        trace!(
            self.logger,
            "Purging transfers";
            "transfer_ids" => format!("{:?}", transfer_ids));

        for id in transfer_ids {
            self.purge_transfer(id)?;
        }

        Ok(())
    }

    pub fn transfers_since(&self, since_timestamp: i64) -> Result<Vec<Transfer>> {
        let conn = self.pool.get()?;

        trace!(
            self.logger,
            "Fetching transfers since timestamp";
            "since_timestamp" => since_timestamp);

        let mut transfers = conn
            .prepare(
                r#"
                SELECT id, peer, created_at, is_outgoing FROM transfers
                WHERE created_at >= datetime(?1, 'unixepoch')
                "#,
            )?
            .query_map(params![since_timestamp], |row| {
                let transfer_type = match row.get::<_, u32>("is_outgoing")? {
                    0 => DbTransferType::Incoming(vec![]),
                    1 => DbTransferType::Outgoing(vec![]),
                    _ => unreachable!(),
                };

                let id: String = row.get("id")?;

                Ok(Transfer {
                    id: Uuid::parse_str(&id).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    peer_id: row.get("peer")?,
                    transfer_type,
                    created_at: row.get("created_at")?,
                    states: vec![],
                })
            })?
            .collect::<QueryResult<Vec<Transfer>>>()?;

        for transfer in &mut transfers {
            match transfer.transfer_type {
                DbTransferType::Incoming(_) => {
                    transfer.transfer_type =
                        DbTransferType::Incoming(self.get_incoming_paths(transfer.id)?)
                }
                DbTransferType::Outgoing(_) => {
                    transfer.transfer_type =
                        DbTransferType::Outgoing(self.get_outgoing_paths(transfer.id)?)
                }
            }

            let tid = transfer.id.to_string();

            transfer.states.extend(
                conn.prepare(
                    r#"
                    SELECT created_at FROM transfer_active_states WHERE transfer_id = ?1
                    "#,
                )?
                .query_map(params![tid], |row| {
                    Ok(TransferStateEvent {
                        transfer_id: transfer.id,
                        created_at: row.get("created_at")?,
                        data: types::TransferStateEventData::Active,
                    })
                })?
                .collect::<QueryResult<Vec<TransferStateEvent>>>()?,
            );

            transfer.states.extend(
                conn.prepare(
                    r#"
                    SELECT created_at, by_peer FROM transfer_cancel_states WHERE transfer_id = ?1
                    "#,
                )?
                .query_map(params![tid], |row| {
                    Ok(TransferStateEvent {
                        transfer_id: transfer.id,
                        created_at: row.get("created_at")?,
                        data: types::TransferStateEventData::Cancel {
                            by_peer: row.get("by_peer")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<TransferStateEvent>>>()?,
            );

            transfer.states.extend(
                conn.prepare(
                    r#"
                    SELECT created_at, status_code FROM transfer_failed_states WHERE transfer_id = ?1
                    "#,
                )?
                .query_map(params![tid], |row| {
                    Ok(TransferStateEvent {
                        transfer_id: transfer.id,
                        created_at: row.get("created_at")?,
                        data: types::TransferStateEventData::Failed {
                            status_code: row.get("status_code")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<TransferStateEvent>>>()?,
            );

            transfer
                .states
                .sort_by(|a, b| a.created_at.cmp(&b.created_at));
        }

        Ok(transfers)
    }

    pub fn remove_transfer_file(&self, transfer_id: Uuid, file_id: &str) -> Result<Option<()>> {
        let conn = self.pool.get()?;

        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Removing transfer file";
            "transfer_id" => &tid,
            "file_id" => file_id,
        );

        let mut count = 0;
        count += conn
            .prepare(
                r#"
                DELETE FROM outgoing_paths
                WHERE transfer_id = ?1
                    AND path_hash = ?2
                    AND id IN(SELECT path_id FROM outgoing_path_reject_states)
            "#,
            )?
            .execute(params![tid, file_id])?;
        count += conn
            .prepare(
                r#"
                DELETE FROM incoming_paths
                WHERE transfer_id = ?1
                    AND path_hash = ?2
                    AND id IN(SELECT path_id FROM incoming_path_reject_states)
            "#,
            )?
            .execute(params![tid, file_id])?;

        match count {
            0 => Ok(None),
            1 => Ok(Some(())),
            _ => {
                warn!(
                    self.logger,
                    "Deleted a file from both outgoing and incoming paths"
                );
                Ok(Some(()))
            }
        }
    }

    fn get_outgoing_paths(&self, transfer_id: Uuid) -> Result<Vec<OutgoingPath>> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Fetching outgoing paths for transfer";
            "transfer_id" => &tid
        );

        let conn = self.pool.get()?;
        let mut paths = conn
            .prepare(
                r#"
                SELECT * FROM outgoing_paths WHERE transfer_id = ?1
                "#,
            )?
            .query_map(params![tid], |row| {
                Ok(OutgoingPath {
                    id: row.get("id")?,
                    transfer_id,
                    base_path: row.get("base_path")?,
                    relative_path: row.get("relative_path")?,
                    file_id: row.get("path_hash")?,
                    bytes: row.get("bytes")?,
                    created_at: row.get("created_at")?,
                    states: vec![],
                })
            })?
            .collect::<QueryResult<Vec<OutgoingPath>>>()?;

        for path in &mut paths {
            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM outgoing_path_pending_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(OutgoingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: OutgoingPathStateEventData::Pending,
                    })
                })?
                .collect::<QueryResult<Vec<OutgoingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM outgoing_path_started_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(OutgoingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: OutgoingPathStateEventData::Started {
                            bytes_sent: row.get("bytes_sent")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<OutgoingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM outgoing_path_cancel_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(OutgoingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: OutgoingPathStateEventData::Cancel {
                            by_peer: row.get("by_peer")?,
                            bytes_sent: row.get("bytes_sent")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<OutgoingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM outgoing_path_failed_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(OutgoingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: OutgoingPathStateEventData::Failed {
                            status_code: row.get("status_code")?,
                            bytes_sent: row.get("bytes_sent")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<OutgoingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM outgoing_path_completed_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(OutgoingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: OutgoingPathStateEventData::Completed,
                    })
                })?
                .collect::<QueryResult<Vec<OutgoingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM outgoing_path_reject_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(OutgoingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: OutgoingPathStateEventData::Rejected {
                            by_peer: row.get("by_peer")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<OutgoingPathStateEvent>>>()?,
            );

            path.states.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        }

        Ok(paths)
    }

    fn get_incoming_paths(&self, transfer_id: Uuid) -> Result<Vec<IncomingPath>> {
        let tid = transfer_id.to_string();

        trace!(
            self.logger,
            "Fetching incoming paths for transfer";
            "transfer_id" => &tid);

        let conn = self.pool.get()?;
        let mut paths = conn
            .prepare(
                r#"
                SELECT * FROM incoming_paths WHERE transfer_id = ?1
                "#,
            )?
            .query_map(params![tid], |row| {
                Ok(IncomingPath {
                    id: row.get("id")?,
                    transfer_id,
                    relative_path: row.get("relative_path")?,
                    file_id: row.get("path_hash")?,
                    bytes: row.get("bytes")?,
                    created_at: row.get("created_at")?,
                    states: vec![],
                })
            })?
            .collect::<QueryResult<Vec<IncomingPath>>>()?;

        for path in &mut paths {
            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM incoming_path_pending_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(IncomingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: IncomingPathStateEventData::Pending,
                    })
                })?
                .collect::<QueryResult<Vec<IncomingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM incoming_path_started_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(IncomingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: IncomingPathStateEventData::Started {
                            bytes_received: row.get("bytes_received")?,
                            base_dir: row.get("base_dir")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<IncomingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM incoming_path_cancel_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(IncomingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: IncomingPathStateEventData::Cancel {
                            by_peer: row.get("by_peer")?,
                            bytes_received: row.get("bytes_received")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<IncomingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM incoming_path_failed_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(IncomingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: IncomingPathStateEventData::Failed {
                            status_code: row.get("status_code")?,
                            bytes_received: row.get("bytes_received")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<IncomingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM incoming_path_completed_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(IncomingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: IncomingPathStateEventData::Completed {
                            final_path: row.get("final_path")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<IncomingPathStateEvent>>>()?,
            );

            path.states.extend(
                conn.prepare(
                    r#"
                    SELECT * FROM incoming_path_reject_states WHERE path_id = ?1
                    "#,
                )?
                .query_map(params![path.id], |row| {
                    Ok(IncomingPathStateEvent {
                        path_id: row.get("path_id")?,
                        created_at: row.get("created_at")?,
                        data: IncomingPathStateEventData::Rejected {
                            by_peer: row.get("by_peer")?,
                        },
                    })
                })?
                .collect::<QueryResult<Vec<IncomingPathStateEvent>>>()?,
            );

            path.states.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        }

        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_transfer() {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let storage = Storage::new(logger, ":memory:").unwrap();

        let transfer_id_1: Uuid = "23e488a4-0521-11ee-be56-0242ac120002".parse().unwrap();
        let transfer_id_2: Uuid = "23e48d7c-0521-11ee-be56-0242ac120002".parse().unwrap();

        {
            let transfer = TransferInfo {
                id: transfer_id_1,
                peer: "1.2.3.4".to_string(),
                files: TransferFiles::Incoming(vec![
                    TransferIncomingPath {
                        file_id: "id1".to_string(),
                        relative_path: "1".to_string(),
                        size: 1024,
                    },
                    TransferIncomingPath {
                        file_id: "id2".to_string(),
                        relative_path: "2".to_string(),
                        size: 2048,
                    },
                ]),
            };

            storage.insert_transfer(&transfer).unwrap();
        }

        {
            let transfer = TransferInfo {
                id: transfer_id_2,
                peer: "5.6.7.8".to_string(),
                files: TransferFiles::Outgoing(vec![
                    TransferOutgoingPath {
                        file_id: "id3".to_string(),
                        size: 1024,
                        base_path: "/dir".to_string(),
                        relative_path: "3".to_string(),
                    },
                    TransferOutgoingPath {
                        file_id: "id4".to_string(),
                        relative_path: "4".to_string(),
                        base_path: "/dir".to_string(),
                        size: 2048,
                    },
                ]),
            };

            storage.insert_transfer(&transfer).unwrap();
        }

        {
            let transfers = storage.transfers_since(0).unwrap();
            assert_eq!(transfers.len(), 2);

            let incoming_transfer = &transfers[0];
            let outgoing_transfer = &transfers[1];

            assert_eq!(incoming_transfer.id, transfer_id_1);
            assert_eq!(outgoing_transfer.id, transfer_id_2);

            assert_eq!(incoming_transfer.peer_id, "1.2.3.4".to_string());
            assert_eq!(outgoing_transfer.peer_id, "5.6.7.8".to_string());
        }

        storage
            .purge_transfers(vec![transfer_id_1.to_string(), transfer_id_2.to_string()])
            .unwrap();

        let transfers = storage.transfers_since(0).unwrap();

        assert_eq!(transfers.len(), 0);
    }

    #[test]
    fn remove_outgoing_rejected_file() {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let storage = Storage::new(logger, ":memory:").unwrap();

        let transfer_id: Uuid = "23e488a4-0521-11ee-be56-0242ac120002".parse().unwrap();

        let transfer = TransferInfo {
            id: transfer_id,
            peer: "5.6.7.8".to_string(),
            files: TransferFiles::Outgoing(vec![
                TransferOutgoingPath {
                    file_id: "id3".to_string(),
                    size: 1024,
                    base_path: "/dir".to_string(),
                    relative_path: "3".to_string(),
                },
                TransferOutgoingPath {
                    file_id: "id4".to_string(),
                    relative_path: "4".to_string(),
                    base_path: "/dir".to_string(),
                    size: 2048,
                },
            ]),
        };

        storage.insert_transfer(&transfer).unwrap();
        storage
            .insert_outgoing_path_reject_state(transfer_id, "id3", false)
            .unwrap();

        let transfers = storage.transfers_since(0).unwrap();
        assert_eq!(transfers.len(), 1);

        let paths = match &transfers[0].transfer_type {
            DbTransferType::Outgoing(out) => out,
            _ => panic!("Unexpected transfer type"),
        };
        assert_eq!(paths.len(), 2);

        assert!(storage
            .remove_transfer_file(transfer_id, "id3")
            .unwrap()
            .is_some());
        assert!(storage
            .remove_transfer_file(transfer_id, "id4")
            .unwrap()
            .is_none());

        let transfers = storage.transfers_since(0).unwrap();
        assert_eq!(transfers.len(), 1);

        let paths = match &transfers[0].transfer_type {
            DbTransferType::Outgoing(out) => out,
            _ => panic!("Unexpected transfer type"),
        };
        assert_eq!(paths.len(), 1); // 1 since we removed one of them
        assert_eq!(paths[0].file_id, "id4");
    }

    #[test]
    fn remove_incoming_rejected_file() {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let storage = Storage::new(logger, ":memory:").unwrap();

        let transfer_id: Uuid = "23e488a4-0521-11ee-be56-0242ac120002".parse().unwrap();

        let transfer = TransferInfo {
            id: transfer_id,
            peer: "5.6.7.8".to_string(),
            files: TransferFiles::Incoming(vec![
                TransferIncomingPath {
                    file_id: "id3".to_string(),
                    size: 1024,
                    relative_path: "3".to_string(),
                },
                TransferIncomingPath {
                    file_id: "id4".to_string(),
                    relative_path: "4".to_string(),
                    size: 2048,
                },
            ]),
        };

        storage.insert_transfer(&transfer).unwrap();
        storage
            .insert_incoming_path_reject_state(transfer_id, "id3", false)
            .unwrap();

        let transfers = storage.transfers_since(0).unwrap();
        assert_eq!(transfers.len(), 1);

        let paths = match &transfers[0].transfer_type {
            DbTransferType::Incoming(inc) => inc,
            _ => panic!("Unexpected transfer type"),
        };
        assert_eq!(paths.len(), 2);

        assert!(storage
            .remove_transfer_file(transfer_id, "id3")
            .unwrap()
            .is_some());
        assert!(storage
            .remove_transfer_file(transfer_id, "id4")
            .unwrap()
            .is_none());

        let transfers = storage.transfers_since(0).unwrap();
        assert_eq!(transfers.len(), 1);

        let paths = match &transfers[0].transfer_type {
            DbTransferType::Incoming(inc) => inc,
            _ => panic!("Unexpected transfer type"),
        };

        assert_eq!(paths.len(), 1); // 1 since we removed one of them
        assert_eq!(paths[0].file_id, "id4");
    }
}
