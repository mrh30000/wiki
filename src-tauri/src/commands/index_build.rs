use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

use walkdir::WalkDir;

use crate::commands::index_db::{IndexDb, IndexPage};

pub struct BuildStats {
    pub pages: usize,
    pub links: usize,
    // Part of the stats contract reported to callers; nothing skips a page yet.
    #[allow(dead_code)]
    pub skipped: usize,
}

fn content_hash(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn mtime_secs(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Collect every Markdown page under wiki/, relative to the project root.
pub fn collect_pages(project_path: &str) -> Result<Vec<IndexPage>, String> {
    let wiki = Path::new(project_path).join("wiki");
    if !wiki.exists() {
        return Ok(Vec::new());
    }
    let mut pages = Vec::new();
    for entry in WalkDir::new(&wiki).follow_links(false) {
        let entry = entry.map_err(|e| format!("枚举 wiki 页面失败: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if !entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let Ok(body_raw) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(Path::new(project_path))
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let title = crate::commands::search::extract_title(
            &body_raw,
            entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default(),
        );
        // Reuse the one frontmatter parser the existing index rebuilder uses;
        // a second YAML-ish parser in this file would drift from it.
        let page_type = crate::commands::project_maintenance::frontmatter_value(&body_raw, "type")
            .unwrap_or_else(|| "other".to_string());
        pages.push(IndexPage {
            path: relative,
            title,
            page_type,
            hash: content_hash(&body_raw),
            mtime: mtime_secs(entry.path()),
            links: crate::commands::search::extract_graph_links(&body_raw),
            body_raw,
        });
    }
    Ok(pages)
}

pub fn build_index(db: &mut IndexDb, project_path: &str) -> Result<BuildStats, String> {
    let pages = collect_pages(project_path)?;
    let link_count = pages.iter().map(|p| p.links.len()).sum();
    let total = pages.len();
    db.replace_all(&pages)?;
    Ok(BuildStats {
        pages: total,
        links: link_count,
        skipped: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_project() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("llm-wiki-indexbuild-test-{id}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("wiki/concepts")).unwrap();
        path
    }

    fn write_page(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn builds_pages_and_links_from_disk() {
        let root = tmp_project();
        write_page(
            &root,
            "wiki/concepts/orion.md",
            "---\ntitle: ORION-3310\ntype: concept\n---\n\n灾备集群见 [[wiki/concepts/dr|灾备]]。",
        );
        write_page(&root, "wiki/concepts/dr.md", "# 灾备\n\nRPO 15 分钟。");

        let mut db = IndexDb::open_in_memory().unwrap();
        let stats = build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();

        assert_eq!(stats.pages, 2);
        assert!(stats.links >= 1);
        let hits = db
            .search_candidates(
                &crate::commands::segment::build_fts_match_query(&["灾备".to_string()]),
                50,
            )
            .unwrap();
        assert_eq!(hits.len(), 2, "both pages mention 灾备");
    }

    #[test]
    fn rebuild_is_idempotent_and_drops_deleted_pages() {
        let root = tmp_project();
        write_page(&root, "wiki/a.md", "# A\n\n灾备");
        write_page(&root, "wiki/b.md", "# B\n\n巡检");

        let mut db = IndexDb::open_in_memory().unwrap();
        build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(db.page_count().unwrap(), 2);

        fs::remove_file(root.join("wiki/b.md")).unwrap();
        build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();

        assert_eq!(db.page_count().unwrap(), 1, "stale rows must be gone");
        let stale = db
            .search_candidates(
                &crate::commands::segment::build_fts_match_query(&["巡检".to_string()]),
                50,
            )
            .unwrap();
        assert!(stale.is_empty(), "deleted page must not be searchable");
    }

    #[test]
    fn missing_wiki_dir_is_not_an_error() {
        let root = tmp_project();
        fs::remove_dir_all(root.join("wiki")).unwrap();
        let mut db = IndexDb::open_in_memory().unwrap();
        let stats = build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(stats.pages, 0);
    }
}