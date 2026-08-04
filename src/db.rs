//! SQLite index over sessions/chunks/transcripts, plus FTS5 search.
//!
//! The database is a derived index, never the source of truth: `meta.json` and
//! `chunk-NNN.json` on disk are authoritative, and `rebuild()` must be able to
//! fully repopulate this file from them. A write failure here must never abort
//! a recording — callers decide that, this module just returns `Result`.
//!
//! Two threads (writer + transcriber) each open their own `Db`; there is no
//! shared `Connection`. WAL + `busy_timeout` are set on every open so that's safe.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::{Path, PathBuf};

pub struct Db {
    conn: Connection,
}

use crate::types::recordings_dir;

/// Open once at startup, before spawning any worker thread, and drop it.
///
/// This is the structural fix for the migration race, not just a nicety: when
/// two threads open a *fresh* database simultaneously they both try to build the
/// schema, and the loser used to die with "table sessions already exists",
/// silently disabling indexing for that thread's whole session. `migrate()` is
/// now race-safe on its own (BEGIN IMMEDIATE), but doing it once here means the
/// concurrent case never arises, and a broken database is reported once, up
/// front, in plain stdout — rather than twice, from inside two threads, as
/// warnings competing with a TUI that has already taken the screen.
pub fn init() -> Result<()> {
    open().map(drop)
}

/// Opens (creating and migrating if needed) ~/.meetrs/meetrs.db
pub fn open() -> Result<Db> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let path = PathBuf::from(home).join(".meetrs").join("meetrs.db");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    open_at(&path)
}

/// Same, at an explicit path — for tests.
pub fn open_at(path: &Path) -> Result<Db> {
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    set_pragmas(&conn)?;
    migrate(&mut conn)?;
    Ok(Db { conn })
}

fn set_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    // BEGIN IMMEDIATE takes the write lock up front, so when two connections open
    // the same fresh database at once (the writer and transcriber threads do
    // exactly that) only one runs the DDL. The loser blocks on busy_timeout, then
    // re-reads user_version *inside* the transaction and finds the schema already
    // built. Reading user_version outside the transaction is the race: both see 0.
    // IF NOT EXISTS below is a second belt for a database created by an older build.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id           INTEGER PRIMARY KEY,
                dir          TEXT NOT NULL UNIQUE,
                started_at   TEXT NOT NULL,
                sample_rate  INTEGER NOT NULL,
                channels     INTEGER NOT NULL,
                sys0         INTEGER NOT NULL,
                sys1         INTEGER NOT NULL,
                mic0         INTEGER NOT NULL,
                mic1         INTEGER NOT NULL,
                detector     TEXT NOT NULL,
                ended_at     TEXT,
                total_chunks INTEGER,
                total_secs   REAL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                id            INTEGER PRIMARY KEY,
                session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                idx           INTEGER NOT NULL,
                file          TEXT NOT NULL,
                duration_secs REAL NOT NULL,
                offset_secs   REAL NOT NULL,
                bytes         INTEGER NOT NULL,
                status        TEXT NOT NULL DEFAULT 'recorded'
                              CHECK (status IN ('recorded', 'transcribed', 'failed')),
                error         TEXT,
                UNIQUE (session_id, idx)
            );

            CREATE TABLE IF NOT EXISTS segments (
                id             INTEGER PRIMARY KEY,
                chunk_id       INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
                leg            TEXT NOT NULL,
                start_secs     REAL NOT NULL,
                end_secs       REAL NOT NULL,
                text           TEXT NOT NULL,
                no_speech_prob REAL NOT NULL
            );

            -- External-content FTS5: text lives in `segments`, this table only
            -- indexes it. Kept in sync by the triggers below.
            CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts USING fts5(
                text,
                content='segments',
                content_rowid='id'
            );

            CREATE TRIGGER IF NOT EXISTS segments_ai AFTER INSERT ON segments BEGIN
                INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
            END;

            CREATE TRIGGER IF NOT EXISTS segments_ad AFTER DELETE ON segments BEGIN
                INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
            END;

            CREATE TRIGGER IF NOT EXISTS segments_au AFTER UPDATE ON segments BEGIN
                INSERT INTO segments_fts(segments_fts, rowid, text) VALUES ('delete', old.id, old.text);
                INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
            END;

            PRAGMA user_version = 1;
            "#,
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub struct SegmentIn {
    pub leg: String,
    pub start_secs: f64,
    pub end_secs: f64,
    pub text: String,
    pub no_speech_prob: f32,
}

pub struct Hit {
    pub session_dir: String,
    pub started_at: String,
    pub chunk_index: i64,
    pub leg: String,
    pub start_secs: f64,
    /// FTS5 snippet() with the matched terms marked.
    pub snippet: String,
}

impl Db {
    fn session_id(&self, dir: &Path) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE dir = ?1",
                params![dir_key(dir)],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn require_session_id(&self, dir: &Path) -> Result<i64> {
        self.session_id(dir)?
            .ok_or_else(|| anyhow::anyhow!("no session recorded for {}", dir.display()))
    }

    fn chunk_id(&self, session_id: i64, index: u32) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM chunks WHERE session_id = ?1 AND idx = ?2",
                params![session_id, index],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Idempotent: called at session start, and again on rebuild.
    pub fn start_session(
        &mut self,
        dir: &Path,
        info: &crate::types::CaptureInfo,
        detector: &str,
        started_rfc3339: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (dir, started_at, sample_rate, channels, sys0, sys1, mic0, mic1, detector)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(dir) DO UPDATE SET
                started_at = excluded.started_at,
                sample_rate = excluded.sample_rate,
                channels = excluded.channels,
                sys0 = excluded.sys0, sys1 = excluded.sys1,
                mic0 = excluded.mic0, mic1 = excluded.mic1,
                detector = excluded.detector",
            params![
                dir_key(dir),
                started_rfc3339,
                info.sample_rate,
                info.channels,
                info.system_channels.0,
                info.system_channels.1,
                info.mic_channels.0,
                info.mic_channels.1,
                detector,
            ],
        )?;
        Ok(())
    }

    pub fn finish_session(&mut self, dir: &Path, chunks: u32, total_secs: f64) -> Result<()> {
        let session_id = self.require_session_id(dir)?;
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1, total_chunks = ?2, total_secs = ?3 WHERE id = ?4",
            params![
                chrono::Utc::now().to_rfc3339(),
                chunks,
                total_secs,
                session_id
            ],
        )?;
        Ok(())
    }

    /// Idempotent per (dir, index).
    pub fn record_chunk(
        &mut self,
        dir: &Path,
        index: u32,
        file: &str,
        duration_secs: f64,
        offset_secs: f64,
        bytes: u64,
    ) -> Result<()> {
        let session_id = self.require_session_id(dir)?;
        self.conn.execute(
            "INSERT INTO chunks (session_id, idx, file, duration_secs, offset_secs, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, idx) DO UPDATE SET
                file = excluded.file,
                duration_secs = excluded.duration_secs,
                offset_secs = excluded.offset_secs,
                bytes = excluded.bytes",
            params![
                session_id,
                index,
                file,
                duration_secs,
                offset_secs,
                bytes as i64
            ],
        )?;
        Ok(())
    }

    /// Replaces any existing segments for this chunk, and marks it transcribed.
    pub fn record_segments(&mut self, dir: &Path, index: u32, segs: &[SegmentIn]) -> Result<()> {
        let session_id = self.require_session_id(dir)?;
        let chunk_id = self
            .chunk_id(session_id, index)?
            .ok_or_else(|| anyhow::anyhow!("no chunk {index} recorded for {}", dir.display()))?;

        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM segments WHERE chunk_id = ?1",
            params![chunk_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO segments (chunk_id, leg, start_secs, end_secs, text, no_speech_prob)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for s in segs {
                stmt.execute(params![
                    chunk_id,
                    s.leg,
                    s.start_secs,
                    s.end_secs,
                    s.text,
                    s.no_speech_prob,
                ])?;
            }
        }
        tx.execute(
            "UPDATE chunks SET status = 'transcribed', error = NULL WHERE id = ?1",
            params![chunk_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_chunk_failed(&mut self, dir: &Path, index: u32, err: &str) -> Result<()> {
        let session_id = self.require_session_id(dir)?;
        let chunk_id = self
            .chunk_id(session_id, index)?
            .ok_or_else(|| anyhow::anyhow!("no chunk {index} recorded for {}", dir.display()))?;
        self.conn.execute(
            "UPDATE chunks SET status = 'failed', error = ?1 WHERE id = ?2",
            params![err, chunk_id],
        )?;
        Ok(())
    }

    /// FTS5 query. Returns newest-first, capped at `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.dir, s.started_at, c.idx, seg.leg, seg.start_secs,
                    snippet(segments_fts, 0, '[', ']', '...', 10)
             FROM segments_fts
             JOIN segments seg ON seg.id = segments_fts.rowid
             JOIN chunks c ON c.id = seg.chunk_id
             JOIN sessions s ON s.id = c.session_id
             WHERE segments_fts MATCH ?1
             ORDER BY s.started_at DESC, c.idx ASC, seg.start_secs ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |r| {
            Ok(Hit {
                session_dir: r.get(0)?,
                started_at: r.get(1)?,
                chunk_index: r.get(2)?,
                leg: r.get(3)?,
                start_secs: r.get(4)?,
                snippet: r.get(5)?,
            })
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => bail!("invalid search query {query:?}: {e}"),
        };
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(h) => out.push(h),
                Err(e) => bail!("invalid search query {query:?}: {e}"),
            }
        }
        Ok(out)
    }

    /// Rescan ~/.meetrs/recordings and rebuild everything from meta.json +
    /// chunk-NNN.json. Returns the number of chunks indexed.
    pub fn rebuild(&mut self) -> Result<usize> {
        let base = recordings_dir()?;
        let mut indexed = 0usize;
        let entries = match std::fs::read_dir(&base) {
            Ok(e) => e,
            Err(_) => return Ok(0), // no recordings yet: nothing to rebuild
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(dir.join("meta.json")) else {
                continue; // no/unreadable meta.json: skip, keep going
            };
            let Ok(meta) = serde_json::from_str::<RebuildMeta>(&raw) else {
                continue; // corrupt meta.json: skip, keep going
            };

            let info = crate::types::CaptureInfo {
                channels: meta.channels,
                sample_rate: meta.sample_rate,
                system_channels: meta.system_channels,
                mic_channels: meta.mic_channels,
            };
            if self
                .start_session(&dir, &info, &meta.detector, &meta.started)
                .is_err()
            {
                continue;
            }

            for mc in &meta.chunks {
                let bytes = std::fs::metadata(dir.join(&mc.file))
                    .map(|m| m.len())
                    .unwrap_or(0);
                if self
                    .record_chunk(
                        &dir,
                        mc.index,
                        &mc.file,
                        mc.duration_secs,
                        mc.started_offset_secs,
                        bytes,
                    )
                    .is_err()
                {
                    continue;
                }
                indexed += 1;

                let transcript_path = dir.join(format!("chunk-{:03}.json", mc.index));
                if let Ok(raw) = std::fs::read_to_string(&transcript_path)
                    && let Ok(t) = serde_json::from_str::<RebuildTranscript>(&raw)
                {
                    let segs: Vec<SegmentIn> = t
                        .segments
                        .into_iter()
                        .map(|s| SegmentIn {
                            leg: s.leg,
                            start_secs: s.start_secs,
                            end_secs: s.end_secs,
                            text: s.text,
                            no_speech_prob: s.no_speech_prob,
                        })
                        .collect();
                    let _ = self.record_segments(&dir, mc.index, &segs);
                }
            }
        }
        Ok(indexed)
    }
}

fn dir_key(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

/// Mirrors chunk.rs's private `Meta`/`MetaChunk` JSON shape — kept as our own
/// Deserialize struct rather than importing, same as transcribe.rs does.
#[derive(serde::Deserialize)]
struct RebuildMeta {
    started: String,
    sample_rate: u32,
    channels: u16,
    system_channels: (u16, u16),
    mic_channels: (u16, u16),
    detector: String,
    chunks: Vec<RebuildChunk>,
}

#[derive(serde::Deserialize)]
struct RebuildChunk {
    index: u32,
    file: String,
    duration_secs: f64,
    started_offset_secs: f64,
}

/// Mirrors transcribe.rs's private `ChunkTranscript`/`SegmentOut` JSON shape.
#[derive(serde::Deserialize)]
struct RebuildTranscript {
    segments: Vec<RebuildSegment>,
}

#[derive(serde::Deserialize)]
struct RebuildSegment {
    leg: String,
    start_secs: f64,
    end_secs: f64,
    text: String,
    no_speech_prob: f32,
}

#[cfg(test)]
mod tests {

    /// Regression: the writer and transcriber threads open the same fresh
    /// database simultaneously. Reading user_version outside a write transaction
    /// let both see 0, both run the DDL, and the loser died with
    /// "table sessions already exists" — which silently disabled indexing for
    /// whichever thread lost. Sequential opens never catch this.
    #[test]
    fn concurrent_opens_on_a_fresh_db_all_succeed() {
        let dir = std::env::temp_dir().join(format!("meetrs-db-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("race.db");
        let _ = std::fs::remove_file(&path);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
        let errs: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    let path = path.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        open_at(&path).err().map(|e| format!("{e:#}"))
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect()
        });
        assert!(errs.is_empty(), "concurrent opens failed: {errs:?}");

        // And the schema is usable afterwards, not half-built.
        let db = open_at(&path).unwrap();
        assert!(db.search("anything", 1).is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }
    use super::*;
    use crate::types::CaptureInfo;

    fn tmp_db_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "meetrs-db-test-{}-{name}-{}",
            std::process::id(),
            name
        ))
    }

    fn info() -> CaptureInfo {
        CaptureInfo {
            channels: 4,
            sample_rate: 48_000,
            system_channels: (0, 1),
            mic_channels: (2, 3),
        }
    }

    fn seg(text: &str, start: f64) -> SegmentIn {
        SegmentIn {
            leg: "mic".into(),
            start_secs: start,
            end_secs: start + 1.0,
            text: text.into(),
            no_speech_prob: 0.01,
        }
    }

    #[test]
    fn migration_runs_once_and_is_idempotent() {
        let path = tmp_db_path("migrate");
        let _ = std::fs::remove_file(&path);
        let db = open_at(&path).unwrap();
        let v: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        drop(db);
        // Reopening must not error and must not bump/reset the version.
        let db2 = open_at(&path).unwrap();
        let v2: i64 = db2
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn start_session_and_record_chunk_are_idempotent() {
        let path = tmp_db_path("idempotent");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-a");

        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();

        let sessions: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        let chunks: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        assert_eq!(chunks, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_segments_replaces_not_duplicates() {
        let path = tmp_db_path("replace");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-b");
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();

        db.record_segments(&dir, 0, &[seg("hello there", 0.0)])
            .unwrap();
        db.record_segments(&dir, 0, &[seg("hello there", 0.0), seg("second pass", 1.0)])
            .unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "second record_segments call should replace, not append"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fts_search_finds_and_loses_deleted_segment() {
        let path = tmp_db_path("fts");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-c");
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();
        db.record_segments(&dir, 0, &[seg("the quick brown fox", 0.0)])
            .unwrap();

        let hits = db.search("quick", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected FTS to find the indexed segment");
        assert_eq!(hits[0].session_dir, "/tmp/session-c");

        // Replacing with segments that don't contain the word proves the
        // external-content DELETE trigger actually removed the old FTS row.
        db.record_segments(&dir, 0, &[seg("nothing relevant here", 0.0)])
            .unwrap();
        let hits2 = db.search("quick", 10).unwrap();
        assert!(
            hits2.is_empty(),
            "deleted segment should no longer be findable"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_query_returns_err_not_panic() {
        let path = tmp_db_path("malformed");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-malformed");
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();
        db.record_segments(&dir, 0, &[seg("some words here", 0.0)])
            .unwrap();
        for q in ["boguscol:hello", "NEAR(foo", "\"a\" \"b", "()", "OR OR"] {
            assert!(db.search(q, 10).is_err(), "expected {q:?} to be rejected");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mark_chunk_failed_records_error_and_status() {
        let path = tmp_db_path("failed");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-d");
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();
        db.mark_chunk_failed(&dir, 0, "whisper blew up").unwrap();

        let (status, err): (String, Option<String>) = db
            .conn
            .query_row("SELECT status, error FROM chunks WHERE idx = 0", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(err, Some("whisper blew up".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cascade_delete_removes_chunks_segments_and_fts() {
        let path = tmp_db_path("cascade");
        let _ = std::fs::remove_file(&path);
        let mut db = open_at(&path).unwrap();
        let dir = PathBuf::from("/tmp/session-e");
        db.start_session(&dir, &info(), "earshot", "2026-01-01T00:00:00Z")
            .unwrap();
        db.record_chunk(&dir, 0, "chunk-000.wav", 5.0, 0.0, 1234)
            .unwrap();
        db.record_segments(&dir, 0, &[seg("cascade candidate", 0.0)])
            .unwrap();

        db.conn
            .execute(
                "DELETE FROM sessions WHERE dir = ?1",
                params![dir_key(&dir)],
            )
            .unwrap();

        let chunks: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        let segs: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))
            .unwrap();
        let fts: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM segments_fts WHERE segments_fts MATCH 'cascade'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(chunks, 0);
        assert_eq!(segs, 0);
        assert_eq!(fts, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rebuild_tolerates_missing_meta_and_missing_transcripts() {
        let path = tmp_db_path("rebuild");
        let _ = std::fs::remove_file(&path);
        let db = open_at(&path).unwrap();
        // rebuild() scans the real ~/.meetrs/recordings dir; on a machine with
        // none (or a broken one), it must not error — it just indexes nothing.
        drop(db);
        let mut db = open_at(&path).unwrap();
        let result = db.rebuild();
        assert!(result.is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
