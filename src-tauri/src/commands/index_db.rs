use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Bump when the schema below changes in a way that requires a rebuild.
pub const INDEX_SCHEMA_VERSION: i64 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pages(
  id          TEXT PRIMARY KEY,
  path        TEXT UNIQUE NOT NULL,
  title       TEXT,
  type        TEXT,
  hash        TEXT,
  mtime       INTEGER,
  frontmatter TEXT,
  source_id   TEXT,
  deleted_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_pages_source ON pages(source_id);
CREATE INDEX IF NOT EXISTS idx_pages_type   ON pages(type);

CREATE TABLE IF NOT EXISTS sources(
  id           TEXT PRIMARY KEY,
  path         TEXT UNIQUE NOT NULL,
  hash         TEXT,
  size         INTEGER,
  mtime        INTEGER,
  parse_status TEXT NOT NULL DEFAULT 'pending',
  error        TEXT,
  created_at   INTEGER,
  processed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sources_status ON sources(parse_status);

CREATE TABLE IF NOT EXISTS chunks(
  id            TEXT PRIMARY KEY,
  page_id       TEXT NOT NULL,
  chunk_index   INTEGER NOT NULL,
  heading_path  TEXT,
  text          TEXT NOT NULL,
  content_hash  TEXT,
  parent_chunk_id TEXT,
  chunk_type    TEXT NOT NULL DEFAULT 'text'
);
CREATE INDEX IF NOT EXISTS idx_chunks_page ON chunks(page_id, chunk_index);

CREATE TABLE IF NOT EXISTS links(
  src_page_id TEXT NOT NULL,
  dst_page_id TEXT,
  dst_raw     TEXT,
  weight      REAL NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_links_src ON links(src_page_id);
CREATE INDEX IF NOT EXISTS idx_links_dst ON links(dst_page_id);

-- body/title hold the SEGMENTED text for matching; body_raw holds the original
-- for display. tokenize MUST be unicode61 (see Spec 13.1).
CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
  title, body, heading_path, page_id UNINDEXED, body_raw UNINDEXED,
  tokenize = 'unicode61'
);
"#;

pub struct IndexDb {
    conn: Connection,
}

// Index location and open/close are the entry points the ingest and query
// paths in later tasks call; nothing consumes them yet.
#[allow(dead_code)]
pub fn index_db_path(project_path: &str) -> PathBuf {
    Path::new(project_path).join(".llm-wiki").join("index.db")
}

impl IndexDb {
    #[allow(dead_code)]
    pub fn open(project_path: &str) -> Result<IndexDb, String> {
        let path = index_db_path(project_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("无法创建索引目录 {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| format!("无法打开索引库 {}: {e}", path.display()))?;
        let db = IndexDb { conn };
        db.apply_pragmas()?;
        db.ensure_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<IndexDb, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("无法创建内存索引库: {e}"))?;
        let db = IndexDb { conn };
        db.ensure_schema()?;
        Ok(db)
    }

    #[allow(dead_code)]
    fn apply_pragmas(&self) -> Result<(), String> {
        // WAL: readers never block the indexer (Review Focus #4).
        self.conn
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("设置 journal_mode 失败: {e}"))?;
        self.conn
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| format!("设置 synchronous 失败: {e}"))?;
        self.conn
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("设置 busy_timeout 失败: {e}"))?;
        Ok(())
    }

    pub fn ensure_schema(&self) -> Result<(), String> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("创建索引 schema 失败: {e}"))?;
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO NOTHING",
                [INDEX_SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| format!("写入 schema_version 失败: {e}"))?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, String> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .ok();
        Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Stamp the schema version. Called after a successful rebuild so the
    /// stale-version flag actually clears; `ensure_schema` deliberately does
    /// NOT overwrite an existing version.
    #[allow(dead_code)]
    pub(crate) fn set_schema_version(&self, version: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [version.to_string()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts5_is_available_in_bundled_sqlite() {
        let db = IndexDb::open_in_memory().expect("schema should apply");
        db.conn
            .execute(
                "INSERT INTO pages_fts(title, body, heading_path, page_id, body_raw)
                 VALUES('t', '灾 备 集 群', '', 'p1', '灾备集群')",
                [],
            )
            .expect("FTS5 insert should work");
        let hits: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH '\"灾 备\"'",
                [],
                |row| row.get(0),
            )
            .expect("FTS5 MATCH should work");
        assert_eq!(hits, 1);
    }

    #[test]
    fn schema_creation_is_idempotent_and_reports_version() {
        let db = IndexDb::open_in_memory().unwrap();
        db.ensure_schema().expect("second ensure_schema must not fail");
        assert_eq!(db.schema_version().unwrap(), INDEX_SCHEMA_VERSION);
    }
}
