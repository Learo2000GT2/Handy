use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info, warn};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tauri::AppHandle;
use tauri_specta::Event;

use crate::file_transcription::types::{
    FileHistoryInfo, FileTranscript, TimingGrain, TranscriptSegment, TranscriptSpeakers,
    TranscriptStatus,
};

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
];

/// Side table for entries made by file transcription, keyed by
/// `transcription_history.id`. It is created idempotently after the
/// migrations instead of being one of them, so `user_version` stays where
/// upstream Handy expects it and either app can keep opening the database.
/// The trailing DELETE drops rows whose history entry was removed by a build
/// that does not know this table. Later columns must be added with a
/// `PRAGMA table_info` check and `ALTER TABLE`, never through `MIGRATIONS`.
pub(crate) const FILE_TRANSCRIPTION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS file_transcription (
    history_id INTEGER PRIMARY KEY NOT NULL,      -- = transcription_history.id
    source_path TEXT NOT NULL,                    -- user's original; NEVER deleted by Handy
    source_name TEXT NOT NULL,
    source_size_bytes INTEGER,
    source_modified_at INTEGER,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    model_id TEXT,
    model_name TEXT,
    language TEXT,
    status TEXT NOT NULL DEFAULT 'completed',     -- completed | partial
    status_detail TEXT,
    timing TEXT NOT NULL DEFAULT 'chunk',         -- chunk | segment
    paragraphs_json TEXT NOT NULL DEFAULT '[]',   -- [TranscriptSegment]
    segments_json TEXT NOT NULL DEFAULT '[]',     -- [TranscriptSegment]
    created_at INTEGER NOT NULL
);
DELETE FROM file_transcription
 WHERE history_id NOT IN (SELECT id FROM transcription_history);";

pub(crate) fn ensure_file_transcription_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FILE_TRANSCRIPTION_SCHEMA)?;
    // TranscriptSpeakers as JSON; NULL = no speaker labels.
    ensure_column(conn, "file_transcription", "speakers_json", "TEXT")
}

/// Adds `column` unless `PRAGMA table_info(table)` lists it. Never touches
/// `user_version`. `table`, `column` and `decl` are constants, never input.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> rusqlite::Result<()> {
    let exists = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

/// Stored speaker labels; NULL or unreadable JSON (logged) reads as none,
/// so the transcript stays readable.
fn speakers_from_sql(history_id: i64, json: Option<&str>) -> Option<TranscriptSpeakers> {
    serde_json::from_str(json?)
        .inspect_err(|e| {
            warn!(
                "Ignoring the invalid speaker labels of history entry {}: {}",
                history_id, e
            )
        })
        .ok()
}

/// Longest a History page waits to learn whether its entries' source files
/// still exist.
const SOURCE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Columns read by `map_history_entry`; every entry SELECT uses this list
/// with `ENTRY_FROM`, so a new column cannot be missed by one query.
const ENTRY_COLUMNS: &str = "h.id AS id, h.file_name AS file_name, h.timestamp AS timestamp, \
    h.saved AS saved, h.title AS title, h.transcription_text AS transcription_text, \
    h.post_processed_text AS post_processed_text, h.post_process_prompt AS post_process_prompt, \
    h.post_process_requested AS post_process_requested, f.history_id AS f_history_id, \
    f.source_path AS f_source_path, f.source_name AS f_source_name, \
    f.duration_ms AS f_duration_ms, f.model_id AS f_model_id, f.model_name AS f_model_name, \
    f.status AS f_status, f.speakers_json AS f_speakers_json";
const ENTRY_FROM: &str =
    "FROM transcription_history h LEFT JOIN file_transcription f ON f.history_id = h.id";

/// The path of a recording inside `dir`, or `None` unless `file_name` is a
/// single plain file name. File transcription entries store `""`, and a path
/// that is absolute or climbs out of `dir` would make `join` point at a file
/// Handy does not own, so neither may ever reach `fs::remove_file`.
fn safe_recording_path(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let mut components = Path::new(file_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => Some(dir.join(name)),
        _ => None,
    }
}

fn status_to_sql(status: TranscriptStatus) -> &'static str {
    match status {
        TranscriptStatus::Completed => "completed",
        TranscriptStatus::Partial => "partial",
    }
}

fn status_from_sql(status: Option<&str>) -> TranscriptStatus {
    match status {
        Some("partial") => TranscriptStatus::Partial,
        _ => TranscriptStatus::Completed,
    }
}

fn timing_to_sql(timing: TimingGrain) -> &'static str {
    match timing {
        TimingGrain::Chunk => "chunk",
        TimingGrain::Segment => "segment",
    }
}

fn timing_from_sql(timing: &str) -> TimingGrain {
    match timing {
        "segment" => TimingGrain::Segment,
        _ => TimingGrain::Chunk,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
    /// Set for entries made by file transcription.
    pub file: Option<FileHistoryInfo>,
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        // Handle migration from tauri-plugin-sql to rusqlite_migration
        // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
        self.migrate_from_tauri_plugin_sql(&conn)?;

        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(&mut conn)?;

        // Not a migration: see FILE_TRANSCRIPTION_SCHEMA.
        ensure_file_transcription_schema(&conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        let file = match row.get::<_, Option<i64>>("f_history_id")? {
            Some(history_id) => Some(FileHistoryInfo {
                source_path: row.get("f_source_path")?,
                source_name: row.get("f_source_name")?,
                duration_ms: row.get("f_duration_ms")?,
                model_id: row.get("f_model_id")?,
                model_name: row.get("f_model_name")?,
                status: status_from_sql(row.get::<_, Option<String>>("f_status")?.as_deref()),
                // Unknown here; `check_sources` looks without blocking the
                // async runtime on a slow file system.
                source_exists: true,
                speaker_count: speakers_from_sql(
                    history_id,
                    row.get::<_, Option<String>>("f_speakers_json")?.as_deref(),
                )
                .map_or(0, |speakers| speakers.speakers.len() as u32),
            }),
            None => None,
        };

        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
            file,
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    /// Save a new history entry to the database.
    /// The WAV file should already have been written to the recordings directory.
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let conn = self.get_connection()?;
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &file_name,
                timestamp,
                false,
                &title,
                &transcription_text,
                &post_processed_text,
                &post_process_prompt,
                post_process_requested,
            ],
        )?;

        let entry = HistoryEntry {
            id: conn.last_insert_rowid(),
            file_name,
            timestamp,
            saved: false,
            title,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            post_process_requested,
            file: None,
        };

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    /// Update an existing history entry with new transcription results (used by retry).
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3
             WHERE id = ?4",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                id
            ],
        )?;

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = conn.query_row(
            &format!("SELECT {ENTRY_COLUMNS} {ENTRY_FROM} WHERE h.id = ?1"),
            params![id],
            Self::map_history_entry,
        )?;

        debug!("Updated transcription for history entry {}", id);

        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                Ok(())
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                let limit = crate::settings::get_history_limit(&self.app_handle);
                self.cleanup_by_count(limit)
            }
            _ => {
                // Use time-based logic
                self.cleanup_by_time(retention_period)
            }
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name) in entries {
            // Delete database entry
            Self::delete_entry_rows(&conn, *id)?;

            // Delete WAV file. File transcription entries have no recording,
            // and their source file is never touched.
            let Some(file_path) = safe_recording_path(&self.recordings_dir, file_name) else {
                continue;
            };
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete WAV file {}: {}", file_name, e);
                } else {
                    debug!("Deleted old WAV file: {}", file_name);
                    deleted_count += 1;
                }
            }
        }

        Ok(deleted_count)
    }

    /// Delete one entry's rows. The side table has no foreign key (each
    /// connection runs without `PRAGMA foreign_keys`), so its row goes by hand.
    fn delete_entry_rows(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;
        conn.execute(
            "DELETE FROM file_transcription WHERE history_id = ?1",
            params![id],
        )?;
        Ok(())
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;
        let entries_to_delete = Self::cleanup_candidates_by_count(&conn, limit)?;
        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!("Cleaned up {} old history entries by count", deleted_count);
        }

        Ok(())
    }

    /// Unsaved entries beyond the newest `limit`, as `(id, file_name)`.
    /// Starred entries (including every file transcription unless the user
    /// unstarred it) are never candidates.
    fn cleanup_candidates_by_count(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries.push(row?);
        }

        Ok(entries.into_iter().skip(limit).collect())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries_to_delete: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} {ENTRY_FROM}
                     WHERE h.id < ?1
                     ORDER BY h.id DESC
                     LIMIT ?2"
                ))?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} {ENTRY_FROM}
                     ORDER BY h.id DESC
                     LIMIT ?1"
                ))?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} {ENTRY_FROM}
                     ORDER BY h.id DESC"
                ))?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }
        check_sources(&mut entries, SOURCE_CHECK_TIMEOUT, |path| path.exists()).await;

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} {ENTRY_FROM}
             ORDER BY h.timestamp DESC
             LIMIT 1"
        ))?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest dictation entry with non-empty transcription text.
    /// File transcriptions are left out: this feeds the tray's "Copy Last
    /// Transcript", which should not copy an hour-long transcript.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} {ENTRY_FROM}
             WHERE h.transcription_text != '' AND f.history_id IS NULL
             ORDER BY h.timestamp DESC
             LIMIT 1"
        ))?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub async fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get current saved status
        let current_saved: bool = conn.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        let new_saved = !current_saved;

        conn.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    /// The entry's recording, or `None` when it has none (file
    /// transcriptions) or the stored name is not a plain file name.
    pub fn get_audio_file_path(&self, file_name: &str) -> Option<PathBuf> {
        safe_recording_path(&self.recordings_dir, file_name)
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} {ENTRY_FROM}
             WHERE h.id = ?1"
        ))?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first. A file transcription has no
            // recording, and its source file is never deleted.
            if let Some(file_path) = self.get_audio_file_path(&entry.file_name) {
                if file_path.exists() {
                    if let Err(e) = fs::remove_file(&file_path) {
                        error!("Failed to delete audio file {}: {}", entry.file_name, e);
                        // Continue with database deletion even if file deletion fails
                    }
                }
            }
        }

        // Delete from database
        Self::delete_entry_rows(&conn, id)?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    /// Save a finished (or partial) file transcription. The entry is starred
    /// so retention never drops it, has no recording (`file_name` is empty),
    /// and is titled with the source file's name.
    pub fn save_file_entry(
        &self,
        transcript: &FileTranscript,
        source_size_bytes: Option<u64>,
        source_modified_at: Option<i64>,
    ) -> Result<HistoryEntry> {
        let mut conn = self.get_connection()?;
        let entry = Self::insert_file_entry_with_conn(
            &mut conn,
            transcript,
            source_size_bytes,
            source_modified_at,
            Utc::now().timestamp(),
        )?;

        debug!(
            "Saved file transcription of {} as history entry {}",
            transcript.source_name, entry.id
        );

        // The entry is already stored; a failed cleanup must not hide that.
        if let Err(e) = self.cleanup_old_entries() {
            error!("Failed to clean up old history entries: {}", e);
        }

        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    fn insert_file_entry_with_conn(
        conn: &mut Connection,
        transcript: &FileTranscript,
        source_size_bytes: Option<u64>,
        source_modified_at: Option<i64>,
        now_secs: i64,
    ) -> Result<HistoryEntry> {
        let paragraphs_json = serde_json::to_string(&transcript.paragraphs)?;
        let segments_json = serde_json::to_string(&transcript.segments)?;
        let speakers_json = transcript
            .speakers
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let model_id = Some(transcript.model_id.as_str()).filter(|id| !id.is_empty());

        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "",
                now_secs,
                true,
                &transcript.source_name,
                &transcript.text,
                Option::<String>::None,
                Option::<String>::None,
                false,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO file_transcription (
                history_id,
                source_path,
                source_name,
                source_size_bytes,
                source_modified_at,
                duration_ms,
                model_id,
                model_name,
                language,
                status,
                status_detail,
                timing,
                paragraphs_json,
                segments_json,
                created_at,
                speakers_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                id,
                &transcript.source_path,
                &transcript.source_name,
                source_size_bytes,
                source_modified_at,
                transcript.duration_ms,
                model_id,
                &transcript.model_name,
                &transcript.language,
                status_to_sql(transcript.status),
                &transcript.status_detail,
                timing_to_sql(transcript.timing),
                paragraphs_json,
                segments_json,
                transcript.created_at,
                speakers_json,
            ],
        )?;
        tx.commit()?;

        let entry = conn.query_row(
            &format!("SELECT {ENTRY_COLUMNS} {ENTRY_FROM} WHERE h.id = ?1"),
            params![id],
            Self::map_history_entry,
        )?;
        Ok(entry)
    }

    /// The full transcript of a file transcription entry; `None` when the
    /// entry does not exist or is a dictation.
    pub fn get_file_transcript(&self, history_id: i64) -> Result<Option<FileTranscript>> {
        let conn = self.get_connection()?;
        Self::get_file_transcript_with_conn(&conn, history_id)
    }

    fn get_file_transcript_with_conn(
        conn: &Connection,
        history_id: i64,
    ) -> Result<Option<FileTranscript>> {
        let row = conn
            .query_row(
                "SELECT
                    h.transcription_text AS text,
                    f.source_path AS source_path,
                    f.source_name AS source_name,
                    f.duration_ms AS duration_ms,
                    f.model_id AS model_id,
                    f.model_name AS model_name,
                    f.language AS language,
                    f.status AS status,
                    f.status_detail AS status_detail,
                    f.timing AS timing,
                    f.paragraphs_json AS paragraphs_json,
                    f.segments_json AS segments_json,
                    f.created_at AS created_at,
                    f.speakers_json AS speakers_json
                 FROM file_transcription f
                 JOIN transcription_history h ON h.id = f.history_id
                 WHERE f.history_id = ?1",
                params![history_id],
                |row| {
                    let transcript = FileTranscript {
                        source_path: row.get("source_path")?,
                        source_name: row.get("source_name")?,
                        duration_ms: row.get("duration_ms")?,
                        model_id: row
                            .get::<_, Option<String>>("model_id")?
                            .unwrap_or_default(),
                        model_name: row.get("model_name")?,
                        language: row.get("language")?,
                        status: status_from_sql(row.get::<_, Option<String>>("status")?.as_deref()),
                        status_detail: row.get("status_detail")?,
                        timing: timing_from_sql(&row.get::<_, String>("timing")?),
                        text: row.get("text")?,
                        paragraphs: Vec::new(),
                        segments: Vec::new(),
                        created_at: row.get("created_at")?,
                        history_id: Some(history_id),
                        speakers: speakers_from_sql(
                            history_id,
                            row.get::<_, Option<String>>("speakers_json")?.as_deref(),
                        ),
                    };
                    let paragraphs_json: String = row.get("paragraphs_json")?;
                    let segments_json: String = row.get("segments_json")?;
                    Ok((transcript, paragraphs_json, segments_json))
                },
            )
            .optional()?;

        let Some((mut transcript, paragraphs_json, segments_json)) = row else {
            return Ok(None);
        };
        transcript.paragraphs = serde_json::from_str::<Vec<TranscriptSegment>>(&paragraphs_json)
            .map_err(|e| {
                anyhow!(
                    "Stored paragraphs of entry {} are invalid: {}",
                    history_id,
                    e
                )
            })?;
        transcript.segments = serde_json::from_str::<Vec<TranscriptSegment>>(&segments_json)
            .map_err(|e| anyhow!("Stored segments of entry {} are invalid: {}", history_id, e))?;
        Ok(Some(transcript))
    }

    /// Rewrites speakers_json and transcription_text (labels changed) of a
    /// file entry in one transaction; emits HistoryUpdatePayload::Updated.
    /// Err if not a file entry.
    pub fn update_file_speakers(
        &self,
        history_id: i64,
        transcript: &FileTranscript,
    ) -> Result<HistoryEntry> {
        let mut conn = self.get_connection()?;
        let entry = Self::update_file_speakers_with_conn(&mut conn, history_id, transcript)?;
        debug!("Updated the speaker labels of history entry {}", history_id);
        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }
        Ok(entry)
    }

    fn update_file_speakers_with_conn(
        conn: &mut Connection,
        history_id: i64,
        transcript: &FileTranscript,
    ) -> Result<HistoryEntry> {
        let speakers_json = transcript
            .speakers
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE file_transcription SET speakers_json = ?1 WHERE history_id = ?2",
            params![speakers_json, history_id],
        )?;
        if updated == 0 {
            return Err(anyhow!(
                "History entry {} is not a file transcription",
                history_id
            ));
        }
        tx.execute(
            "UPDATE transcription_history SET transcription_text = ?1 WHERE id = ?2",
            params![&transcript.text, history_id],
        )?;
        tx.commit()?;

        Ok(conn.query_row(
            &format!("SELECT {ENTRY_COLUMNS} {ENTRY_FROM} WHERE h.id = ?1"),
            params![history_id],
            Self::map_history_entry,
        )?)
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

/// Sets `source_exists` on the file entries, checking each source with
/// `exists` on a blocking thread: a source on an unreachable network share
/// can stall for a long time. A source not checked within `timeout` keeps
/// `true` (unknown).
async fn check_sources(
    entries: &mut [HistoryEntry],
    timeout: Duration,
    exists: impl Fn(&Path) -> bool + Send + 'static,
) {
    let paths: Vec<String> = entries
        .iter()
        .filter_map(|entry| Some(entry.file.as_ref()?.source_path.clone()))
        .collect();
    if paths.is_empty() {
        return;
    }
    let found = Arc::new(Mutex::new(Vec::with_capacity(paths.len())));
    let results = Arc::clone(&found);
    let checks = tauri::async_runtime::spawn_blocking(move || {
        for path in paths {
            let exists = exists(Path::new(&path));
            results
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(exists);
        }
    });
    if tokio::time::timeout(timeout, checks).await.is_err() {
        debug!("Gave up checking the source files of history entries");
    }

    let found = found.lock().unwrap_or_else(PoisonError::into_inner);
    let files = entries.iter_mut().filter_map(|entry| entry.file.as_mut());
    for (file, &exists) in files.zip(found.iter()) {
        file.source_exists = exists;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::speakers::{rename_speaker, render_text};
    use crate::file_transcription::types::TranscriptSpeaker;
    use rusqlite::{params, Connection};

    fn setup_conn() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .expect("run migrations");
        ensure_file_transcription_schema(&conn).expect("create file transcription table");
        conn
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("handy-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(start_ms, end_ms, text)
    }

    fn file_transcript(source_path: &str) -> FileTranscript {
        FileTranscript {
            source_path: source_path.to_string(),
            source_name: "interview.m4a".to_string(),
            duration_ms: 61_000,
            model_id: "parakeet".to_string(),
            model_name: Some("Parakeet V3".to_string()),
            language: Some("en".to_string()),
            status: TranscriptStatus::Partial,
            status_detail: Some("Decoding stopped early at 00:01:01: bad packet".to_string()),
            timing: TimingGrain::Segment,
            text: "Hello there.\n\nSecond part.".to_string(),
            paragraphs: vec![
                segment(0, 28_000, "Hello there."),
                segment(28_000, 61_000, "Second part."),
            ],
            segments: vec![
                segment(500, 1_800, "Hello"),
                segment(1_800, 2_400, "there."),
                segment(28_300, 30_000, "Second part."),
            ],
            created_at: 1_700_000_000,
            history_id: None,
            speakers: None,
        }
    }

    fn insert_file(conn: &mut Connection, transcript: &FileTranscript, now: i64) -> HistoryEntry {
        HistoryManager::insert_file_entry_with_conn(
            conn,
            transcript,
            Some(1_234),
            Some(1_600_000_000),
            now,
        )
        .expect("insert file entry")
    }

    fn file_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM file_transcription", [], |row| {
            row.get(0)
        })
        .expect("count file rows")
    }

    #[test]
    fn file_transcription_schema_is_idempotent() {
        let mut conn = setup_conn();
        insert_file(&mut conn, &file_transcript("/missing/interview.m4a"), 100);

        ensure_file_transcription_schema(&conn).expect("second run");
        ensure_file_transcription_schema(&conn).expect("third run");

        assert_eq!(file_rows(&conn), 1);
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version as usize, MIGRATIONS.len());
    }

    #[test]
    fn schema_drops_orphaned_file_rows() {
        let mut conn = setup_conn();
        let kept = insert_file(&mut conn, &file_transcript("/missing/a.m4a"), 100);
        let orphaned = insert_file(&mut conn, &file_transcript("/missing/b.m4a"), 200);
        // What a build without the side table does on delete.
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![orphaned.id],
        )
        .expect("delete history row");

        ensure_file_transcription_schema(&conn).expect("rerun schema");

        let remaining: Vec<i64> = conn
            .prepare("SELECT history_id FROM file_transcription")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert_eq!(remaining, vec![kept.id]);
    }

    #[test]
    fn inserted_file_entry_maps_file_info() {
        let mut conn = setup_conn();
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("interview.m4a");
        fs::write(&source, b"audio").expect("write source");
        let transcript = file_transcript(&source.to_string_lossy());

        let entry = insert_file(&mut conn, &transcript, 1_700_000_100);

        assert_eq!(entry.file_name, "");
        assert!(entry.saved);
        assert_eq!(entry.timestamp, 1_700_000_100);
        assert_eq!(entry.title, "interview.m4a");
        assert_eq!(entry.transcription_text, transcript.text);
        assert_eq!(entry.post_processed_text, None);
        assert!(!entry.post_process_requested);
        let file = entry.file.expect("file info");
        assert_eq!(file.source_path, transcript.source_path);
        assert_eq!(file.source_name, "interview.m4a");
        assert_eq!(file.duration_ms, 61_000);
        assert_eq!(file.model_id.as_deref(), Some("parakeet"));
        assert_eq!(file.model_name.as_deref(), Some("Parakeet V3"));
        assert_eq!(file.status, TranscriptStatus::Partial);
        assert!(file.source_exists);
    }

    fn source_exists(entry: &HistoryEntry) -> bool {
        entry.file.as_ref().expect("file info").source_exists
    }

    #[tokio::test]
    async fn check_sources_marks_missing_source_files() {
        let mut conn = setup_conn();
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("interview.m4a");
        fs::write(&source, b"audio").expect("write source");
        insert_file(&mut conn, &file_transcript(&source.to_string_lossy()), 100);
        insert_entry(&conn, 200, "dictated", None);
        let read = |conn: &Connection| {
            conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} {ENTRY_FROM} ORDER BY h.id DESC"
            ))
            .expect("prepare")
            .query_map([], HistoryManager::map_history_entry)
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect")
        };

        let mut entries = read(&conn);
        check_sources(&mut entries, SOURCE_CHECK_TIMEOUT, |path| path.exists()).await;
        assert!(entries[0].file.is_none());
        assert!(source_exists(&entries[1]));

        fs::remove_file(&source).expect("remove source");
        let mut entries = read(&conn);
        // Reading rows never touches the file system.
        assert!(source_exists(&entries[1]));
        check_sources(&mut entries, SOURCE_CHECK_TIMEOUT, |path| path.exists()).await;
        assert!(!source_exists(&entries[1]));
    }

    #[tokio::test]
    async fn check_sources_treats_stalled_checks_as_present() {
        let file_entry = |id: i64, source_path: &str| HistoryEntry {
            id,
            file_name: String::new(),
            timestamp: id,
            saved: true,
            title: source_path.to_string(),
            transcription_text: "text".to_string(),
            post_processed_text: None,
            post_process_prompt: None,
            post_process_requested: false,
            file: Some(FileHistoryInfo {
                source_path: source_path.to_string(),
                source_name: source_path.to_string(),
                duration_ms: 1_000,
                model_id: None,
                model_name: None,
                status: TranscriptStatus::Completed,
                source_exists: true,
                speaker_count: 0,
            }),
        };
        let mut entries = vec![
            file_entry(4, "/local/gone.m4a"),
            file_entry(3, "/nas/stalled.m4a"),
            file_entry(2, "/local/after.m4a"),
        ];
        // Stands in for a stat on a hung network mount, until released.
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let exists = move |path: &Path| {
            if path.starts_with("/nas") {
                let _ = stalled.recv();
            }
            false
        };

        let started = std::time::Instant::now();
        check_sources(&mut entries, Duration::from_secs(1), exists).await;

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!source_exists(&entries[0]));
        assert!(source_exists(&entries[1]), "unknown counts as present");
        assert!(source_exists(&entries[2]), "not reached in time");
        drop(release);
    }

    #[test]
    fn dictation_entries_have_no_file_info() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "dictated", None);

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch")
            .expect("entry exists");

        assert!(entry.file.is_none());
        assert_eq!(entry.file_name, "handy-100.wav");
    }

    #[test]
    fn latest_completed_entry_skips_file_transcriptions() {
        let mut conn = setup_conn();
        insert_entry(&conn, 100, "dictated", None);
        insert_file(&mut conn, &file_transcript("/missing/a.m4a"), 200);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("dictation entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "dictated");
    }

    #[test]
    fn file_transcript_round_trips() {
        let mut conn = setup_conn();
        let transcript = file_transcript("/missing/interview.m4a");
        let entry = insert_file(&mut conn, &transcript, 100);

        let stored = HistoryManager::get_file_transcript_with_conn(&conn, entry.id)
            .expect("read transcript")
            .expect("file transcript exists");

        let expected = FileTranscript {
            history_id: Some(entry.id),
            ..transcript
        };
        assert_eq!(stored, expected);
    }

    /// `file_transcript` with two speakers, the second one named.
    fn labelled_transcript(source_path: &str) -> FileTranscript {
        let labelled = |start_ms, end_ms, text: &str, speaker| TranscriptSegment {
            speaker: Some(speaker),
            ..segment(start_ms, end_ms, text)
        };
        let speakers = TranscriptSpeakers {
            speakers: vec![
                TranscriptSpeaker { id: 1, name: None },
                TranscriptSpeaker {
                    id: 2,
                    name: Some("Doug".to_string()),
                },
            ],
            approximate: false,
            unassigned_segments: 0,
            model: "sortformer-4spk-v2.1-q8_0".to_string(),
        };
        let paragraphs = vec![
            labelled(0, 2_400, "Hello there.", 1),
            labelled(28_300, 30_000, "Second part.", 2),
        ];
        FileTranscript {
            text: render_text(&paragraphs, Some(&speakers)),
            paragraphs,
            segments: vec![
                labelled(500, 1_800, "Hello", 1),
                labelled(1_800, 2_400, "there.", 1),
                labelled(28_300, 30_000, "Second part.", 2),
            ],
            speakers: Some(speakers),
            ..file_transcript(source_path)
        }
    }

    fn columns(conn: &Connection) -> Vec<String> {
        conn.prepare("PRAGMA table_info(file_transcription)")
            .expect("prepare")
            .query_map([], |row| row.get(1))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect")
    }

    #[test]
    fn speakers_column_is_added_once_and_user_version_is_unchanged() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .expect("run migrations");
        // The table as a build before speaker labels made it.
        conn.execute_batch(FILE_TRANSCRIPTION_SCHEMA)
            .expect("old schema");
        assert!(!columns(&conn).contains(&"speakers_json".to_string()));

        for _ in 0..3 {
            ensure_file_transcription_schema(&conn).expect("ensure schema");
        }

        let speakers_columns = columns(&conn)
            .iter()
            .filter(|name| *name == "speakers_json")
            .count();
        assert_eq!(speakers_columns, 1);
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version as usize, MIGRATIONS.len());
    }

    #[test]
    fn legacy_row_without_speakers_loads() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .expect("run migrations");
        conn.execute_batch(FILE_TRANSCRIPTION_SCHEMA)
            .expect("old schema");
        // A row written by a build before speaker labels.
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, \
             transcription_text) VALUES ('', 100, 1, 'old.m4a', 'Old text.')",
            [],
        )
        .expect("insert history row");
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_transcription (history_id, source_path, source_name, \
             paragraphs_json, segments_json, created_at) VALUES (?1, '/old.m4a', 'old.m4a', \
             '[{\"start_ms\":0,\"end_ms\":900,\"text\":\"Old text.\"}]', '[]', 1)",
            params![id],
        )
        .expect("insert file row");

        ensure_file_transcription_schema(&conn).expect("add the column");

        let stored = HistoryManager::get_file_transcript_with_conn(&conn, id)
            .expect("read transcript")
            .expect("file transcript exists");
        assert_eq!(stored.speakers, None);
        assert_eq!(stored.text, "Old text.");
        assert_eq!(stored.paragraphs, vec![segment(0, 900, "Old text.")]);
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read entry")
            .expect("entry exists");
        assert_eq!(entry.file.expect("file info").speaker_count, 0);
    }

    #[test]
    fn file_transcript_with_speakers_round_trips() {
        let mut conn = setup_conn();
        let transcript = labelled_transcript("/missing/interview.m4a");
        let entry = insert_file(&mut conn, &transcript, 100);
        assert_eq!(
            entry.transcription_text,
            "Speaker 1: Hello there.\n\nDoug: Second part."
        );

        let stored = HistoryManager::get_file_transcript_with_conn(&conn, entry.id)
            .expect("read transcript")
            .expect("file transcript exists");
        assert_eq!(
            stored,
            FileTranscript {
                history_id: Some(entry.id),
                ..transcript
            }
        );
    }

    #[test]
    fn entry_reports_speaker_count() {
        let mut conn = setup_conn();
        let labelled = insert_file(&mut conn, &labelled_transcript("/missing/a.m4a"), 100);
        let plain = insert_file(&mut conn, &file_transcript("/missing/b.m4a"), 200);
        assert_eq!(labelled.file.expect("file info").speaker_count, 2);
        assert_eq!(plain.file.expect("file info").speaker_count, 0);
    }

    #[test]
    fn update_file_speakers_rewrites_text_and_names() {
        let mut conn = setup_conn();
        let entry = insert_file(&mut conn, &labelled_transcript("/missing/a.m4a"), 100);
        let mut transcript = HistoryManager::get_file_transcript_with_conn(&conn, entry.id)
            .expect("read transcript")
            .expect("file transcript exists");
        rename_speaker(&mut transcript, 1, Some("Ann")).expect("rename");

        let updated =
            HistoryManager::update_file_speakers_with_conn(&mut conn, entry.id, &transcript)
                .expect("update");
        assert_eq!(
            updated.transcription_text,
            "Ann: Hello there.\n\nDoug: Second part."
        );
        assert_eq!(updated.file.expect("file info").speaker_count, 2);
        let stored = HistoryManager::get_file_transcript_with_conn(&conn, entry.id)
            .expect("read transcript")
            .expect("file transcript exists");
        assert_eq!(stored, transcript);

        // Not a file entry.
        insert_entry(&conn, 300, "dictation", None);
        let dictation = conn.last_insert_rowid();
        assert!(
            HistoryManager::update_file_speakers_with_conn(&mut conn, dictation, &transcript)
                .is_err()
        );
        let text: String = conn
            .query_row(
                "SELECT transcription_text FROM transcription_history WHERE id = ?1",
                params![dictation],
                |row| row.get(0),
            )
            .expect("read text");
        assert_eq!(text, "dictation");
    }

    #[test]
    fn invalid_speakers_json_reads_as_none() {
        let mut conn = setup_conn();
        let entry = insert_file(&mut conn, &labelled_transcript("/missing/a.m4a"), 100);
        conn.execute(
            "UPDATE file_transcription SET speakers_json = '{not json' WHERE history_id = ?1",
            params![entry.id],
        )
        .expect("corrupt the labels");

        let stored = HistoryManager::get_file_transcript_with_conn(&conn, entry.id)
            .expect("the transcript stays readable")
            .expect("file transcript exists");
        assert_eq!(stored.speakers, None);
        assert_eq!(stored.text, "Speaker 1: Hello there.\n\nDoug: Second part.");
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read entry")
            .expect("entry exists");
        assert_eq!(entry.file.expect("file info").speaker_count, 0);
    }

    #[test]
    fn file_transcript_without_model_id_stores_null() {
        let mut conn = setup_conn();
        let transcript = FileTranscript {
            model_id: String::new(),
            model_name: None,
            language: None,
            status: TranscriptStatus::Completed,
            status_detail: None,
            timing: TimingGrain::Chunk,
            ..file_transcript("/missing/a.m4a")
        };
        let entry = insert_file(&mut conn, &transcript, 100);

        assert_eq!(entry.file.expect("file info").model_id, None);
        let stored = HistoryManager::get_file_transcript_with_conn(&conn, entry.id)
            .expect("read transcript")
            .expect("file transcript exists");
        assert_eq!(stored.model_id, "");
        assert_eq!(stored.status, TranscriptStatus::Completed);
        assert_eq!(stored.timing, TimingGrain::Chunk);
    }

    #[test]
    fn file_transcript_is_none_for_dictations_and_unknown_ids() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "dictated", None);
        let id = conn.last_insert_rowid();

        assert!(HistoryManager::get_file_transcript_with_conn(&conn, id)
            .expect("read dictation")
            .is_none());
        assert!(
            HistoryManager::get_file_transcript_with_conn(&conn, id + 100)
                .expect("read unknown")
                .is_none()
        );
    }

    #[test]
    fn deleting_rows_removes_the_file_row() {
        let mut conn = setup_conn();
        let entry = insert_file(&mut conn, &file_transcript("/missing/a.m4a"), 100);

        HistoryManager::delete_entry_rows(&conn, entry.id).expect("delete rows");

        assert_eq!(file_rows(&conn), 0);
        assert!(HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch")
            .is_none());
    }

    #[test]
    fn count_cleanup_never_selects_saved_file_entries() {
        let mut conn = setup_conn();
        let file_entry = insert_file(&mut conn, &file_transcript("/missing/a.m4a"), 50);
        for timestamp in 100..106 {
            insert_entry(&conn, timestamp, "dictated", None);
        }

        let candidates =
            HistoryManager::cleanup_candidates_by_count(&conn, 5).expect("cleanup candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1, "handy-100.wav");
        assert!(candidates.iter().all(|(id, _)| *id != file_entry.id));

        let all_unsaved =
            HistoryManager::cleanup_candidates_by_count(&conn, 0).expect("all unsaved");
        assert_eq!(all_unsaved.len(), 6);
        assert!(all_unsaved.iter().all(|(id, _)| *id != file_entry.id));
    }

    #[test]
    fn safe_recording_path_accepts_only_plain_file_names() {
        let dir = Path::new("/data/recordings");

        assert_eq!(
            safe_recording_path(dir, "handy-1.wav"),
            Some(dir.join("handy-1.wav"))
        );
        for unsafe_name in ["", ".", "..", "../x", "a/b", "./handy-1.wav", "/etc/passwd"] {
            assert_eq!(
                safe_recording_path(dir, unsafe_name),
                None,
                "{unsafe_name:?}"
            );
        }
        #[cfg(windows)]
        for unsafe_name in [r"C:\x", r"C:x", r"\\server\share\x", r"a\b"] {
            assert_eq!(
                safe_recording_path(dir, unsafe_name),
                None,
                "{unsafe_name:?}"
            );
        }
    }
}
