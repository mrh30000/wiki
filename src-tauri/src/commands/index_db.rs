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

pub struct IndexPage {
    pub path: String,
    pub title: String,
    pub page_type: String,
    pub hash: String,
    pub mtime: i64,
    pub body_raw: String,
    pub links: Vec<String>,
}

pub struct IndexDb {
    conn: Connection,
}

pub fn index_db_path(project_path: &str) -> PathBuf {
    Path::new(project_path).join(".llm-wiki").join("index.db")
}

impl IndexDb {
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

    // Used by unit tests across index_db and index_build modules for isolated in-memory verification.
    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<IndexDb, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("无法创建内存索引库: {e}"))?;
        let db = IndexDb { conn };
        db.ensure_schema()?;
        Ok(db)
    }

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

    /// Replace the entire index in one transaction. Callers must have already
    /// written the Markdown to disk — the index is a derivative.
    pub fn replace_all(&mut self, pages: &[IndexPage]) -> Result<(), String> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| format!("无法开启索引事务: {e}"))?;
        tx.execute("DELETE FROM pages_fts", [])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM pages", []).map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM links", []).map_err(|e| e.to_string())?;
        for page in pages {
            tx.execute(
                "INSERT INTO pages(id, path, title, type, hash, mtime, frontmatter)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                rusqlite::params![
                    page.path,
                    page.path,
                    page.title,
                    page.page_type,
                    page.hash,
                    page.mtime
                ],
            )
            .map_err(|e| format!("写入 page 失败 ({}): {e}", page.path))?;
            // Indexed text goes through the same segmenter the query side uses;
            // asymmetric use fails silently (no match, no error).
            let segmented_title = crate::commands::segment::segment_for_fts(&page.title);
            let segmented_body = crate::commands::segment::segment_for_fts(&page.body_raw);
            tx.execute(
                "INSERT INTO pages_fts(title, body, heading_path, page_id, body_raw)
                 VALUES(?1, ?2, '', ?3, ?4)",
                rusqlite::params![
                    segmented_title,
                    segmented_body,
                    page.path,
                    page.body_raw
                ],
            )
            .map_err(|e| format!("写入 FTS 失败 ({}): {e}", page.path))?;
            for dst in &page.links {
                tx.execute(
                    "INSERT INTO links(src_page_id, dst_page_id, dst_raw, weight)
                     VALUES(?1, NULL, ?2, 1.0)",
                    rusqlite::params![page.path, dst],
                )
                .map_err(|e| format!("写入 link 失败: {e}"))?;
            }
        }
        tx.commit().map_err(|e| format!("提交索引事务失败: {e}"))?;
        Ok(())
    }

    /// FTS5 candidate generation. Returns project-relative page paths.
    pub fn search_candidates(&self, match_query: &str, limit: usize) -> Result<Vec<String>, String> {
        if match_query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT page_id FROM pages_fts
                 WHERE pages_fts MATCH ?1
                 ORDER BY bm25(pages_fts)
                 LIMIT ?2",
            )
            .map_err(|e| format!("准备候选查询失败: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params![match_query, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| format!("执行候选查询失败: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("读取候选失败: {e}"))?);
        }
        Ok(out)
    }

    pub fn page_count(&self) -> Result<usize, String> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM pages", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        Ok(n as usize)
    }

    /// Resolve 1-hop link references to page paths by matching against pages.
    pub fn resolve_neighbor_paths(&self, seed_links: &[String]) -> Result<Vec<String>, String> {
        if seed_links.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT path, title FROM pages")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?;

        let mut target_aliases = std::collections::HashSet::new();
        for link in seed_links {
            target_aliases.insert(crate::commands::search::normalize_graph_alias(link));
        }

        let mut matched_paths = Vec::new();
        for row in rows {
            let (path, title) = row.map_err(|e| e.to_string())?;
            let wiki_relative = path.strip_prefix("wiki/").unwrap_or(&path);
            let stem = std::path::Path::new(&path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let mut matches = false;
            for candidate in [
                path.as_str(),
                wiki_relative,
                stem,
                title.as_deref().unwrap_or_default(),
            ] {
                if target_aliases.contains(&crate::commands::search::normalize_graph_alias(candidate)) {
                    matches = true;
                    break;
                }
            }
            if matches {
                matched_paths.push(path);
            }
        }
        Ok(matched_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::segment::segment_for_fts;

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

    /// The write side must store the SEGMENTED text: unicode61 treats a run of
    /// CJK as one token, so an unsegmented body silently never matches the
    /// segmented query. Pinning the stored text guards that invariant.
    #[test]
    fn replace_all_stores_segmented_fts_text_and_raw_body() {
        let mut db = IndexDb::open_in_memory().unwrap();
        db.replace_all(&[IndexPage {
            path: "wiki/a.md".to_string(),
            title: "灾备".to_string(),
            page_type: "concept".to_string(),
            hash: "h".to_string(),
            mtime: 1,
            body_raw: "灾备集群".to_string(),
            links: vec!["wiki/b.md".to_string()],
        }])
        .unwrap();

        let (title, body, body_raw): (String, String, String) = db
            .conn
            .query_row(
                "SELECT title, body, body_raw FROM pages_fts WHERE page_id = 'wiki/a.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, segment_for_fts("灾备"));
        assert_eq!(body, segment_for_fts("灾备集群"));
        assert_eq!(body_raw, "灾备集群", "body_raw keeps the original for display");
        assert_eq!(db.page_count().unwrap(), 1);
        let link_count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(link_count, 1);
    }

    #[test]
    fn schema_creation_is_idempotent_and_reports_version() {
        let db = IndexDb::open_in_memory().unwrap();
        db.ensure_schema().expect("second ensure_schema must not fail");
        assert_eq!(db.schema_version().unwrap(), INDEX_SCHEMA_VERSION);
    }
}
