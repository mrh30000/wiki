#[cfg(test)]
use crate::commands::search::tokenize_query;

/// Insert a space after every CJK ideograph so unicode61 emits one token per
/// character. Non-CJK characters pass through untouched.
///
/// The write side and the query side MUST both go through this function;
/// asymmetric use fails silently (no match, no error).
pub fn segment_for_fts(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + input.len() / 2);
    for ch in input.chars() {
        if ('\u{3400}'..='\u{9fff}').contains(&ch) {
            out.push(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out.trim_end().to_string()
}

/// Escape a string for use inside an FTS5 double-quoted phrase.
/// FTS5 escapes a literal `"` by doubling it.
///
/// Private on purpose: nothing outside this module needs it, and a second
/// escape implementation elsewhere is how FTS5 injection creeps back in.
fn fts_escape_phrase(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// Build an FTS5 MATCH expression from already-tokenized query terms.
///
/// Each term becomes its own quoted phrase so that ordering inside a term is
/// preserved (灾备 matches, 备灾 does not), and terms are joined with OR to
/// keep the existing bag-of-tokens recall semantics of `score_file`.
pub fn build_fts_match_query(tokens: &[String]) -> String {
    let parts: Vec<String> = tokens
        .iter()
        .map(|t| segment_for_fts(t))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{}\"", fts_escape_phrase(&s)))
        .collect();
    parts.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cjk_per_character_and_preserves_ascii() {
        assert_eq!(segment_for_fts("灾备集群"), "灾 备 集 群");
        assert_eq!(segment_for_fts("ORION-3310"), "ORION-3310");
        assert_eq!(segment_for_fts("华东2区"), "华 东 2区");
        assert_eq!(segment_for_fts("RPO 15 分钟"), "RPO 15 分 钟");
    }

    #[test]
    fn segments_query_terms_and_preserves_expected_shape() {
        let raw = "灾备集群部署在华东2区";
        let indexed = segment_for_fts(raw);
        // A 2-character CJK query must survive the round trip (Review Focus #1).
        let q = build_fts_match_query(&["巡检".to_string(), "灾备".to_string()]);
        assert_eq!(q, "\"巡 检\" OR \"灾 备\"");
        assert!(indexed.contains("灾 备"));
    }

    #[test]
    fn fts_syntax_characters_are_treated_as_literals() {
        // Review Focus #3: user typing FTS5 syntax must not break or inject.
        for raw in ["NEAR", "AND", "OR", "a*b", "(x)", "col:val", "he said \"hi\""] {
            let q = build_fts_match_query(&[raw.to_string()]);
            // Every term is wrapped in quotes, so no bare operator survives.
            assert!(q.starts_with('"') && q.ends_with('"'), "unquoted: {q}");
            assert_eq!(q.matches('"').count() % 2, 0, "unbalanced quotes: {q}");
        }

        // The two structural assertions above CANNOT detect a broken escaper:
        // an unescaped `he said "hi"` yields `"he said "hi""` — 4 quotes
        // (even) and still starts/ends with a quote. So pin the exact escaped
        // output too, or removing the escaping would pass this test silently.
        assert_eq!(
            build_fts_match_query(&["he said \"hi\"".to_string()]),
            "\"he said \"\"hi\"\"\""
        );
        assert_eq!(build_fts_match_query(&["\"".to_string()]), "\"\"\"\"");
    }

    #[test]
    fn empty_and_whitespace_tokens_are_dropped() {
        assert_eq!(build_fts_match_query(&[]), "");
        assert_eq!(build_fts_match_query(&["  ".to_string()]), "");
    }

    #[test]
    fn reuses_existing_query_tokenizer_for_cjk_bigrams() {
        // Guards Global Constraint: no second CJK tokenizer.
        let tokens = tokenize_query("默会知识");
        assert!(tokens.contains(&"默会".to_string()));
        let q = build_fts_match_query(&tokens);
        assert!(q.contains("\"默 会\""));
    }
}
