//! Fuzzy ranking for the query pane's completion popup (db-studio#11),
//! over the same table/column names the schema tree inspector renders
//! (`db-core#310`'s `Engine::tables()`) -- one catalog fetch, not a
//! second query against the engine.

use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Builds the flat candidate list completion ranks against: every table
/// name plus every column name, in no particular order (ranking sorts
/// by score, not source order). Callers merge this with [`keywords`]
/// for the full popup candidate set (db-studio#48) -- kept separate
/// here so a single file's schema can still be ranked on its own
/// wherever that's useful (e.g. tests).
pub fn candidates(tables: &[db_core::engine::TableInfo]) -> Vec<String> {
    let mut names = Vec::new();
    for table in tables {
        names.push(table.name.clone());
        for col in &table.columns {
            names.push(col.name.clone());
        }
    }
    names
}

/// Static SQL keywords and built-in function names (db-studio#48,
/// "step one" of the completion popup -- language, not schema). Every
/// single-token keyword and every scalar function name comes straight
/// from `db-core`'s own tables (`parser::row::tokenizer::
/// keyword_names`, `functions::SCALAR_FUNCTION_NAMES`) rather than a
/// hand-maintained copy here -- exactly the drift `db-cli`'s own
/// `editor.rs` keyword list already has relative to db-core's real
/// grammar, which this is meant not to repeat.
///
/// Two things `db-core` has no single flat list for, so they're
/// hand-kept here instead:
/// - multi-keyword clause phrases (`GROUP BY`, `ORDER BY`, `INNER
///   JOIN`, ...) -- the tokenizer's table is one entry per single
///   keyword (`GROUP`, `BY` separately), and completing the whole
///   phrase in one go is what's actually useful to type
/// - aggregate/window function names (`COUNT`, `SUM`, `ROW_NUMBER`,
///   ...) -- recognized by each planner's own validator, not
///   `functions::SCALAR_FUNCTION_NAMES`'s scalar registry
pub fn keywords() -> Vec<String> {
    const CLAUSE_PHRASES: &[&str] = &[
        "GROUP BY",
        "ORDER BY",
        "INNER JOIN",
        "LEFT JOIN",
        "UNION ALL",
        "IS NOT",
    ];
    const AGGREGATE_AND_WINDOW_NAMES: &[&str] =
        &["COUNT", "SUM", "AVG", "MIN", "MAX", "ROW_NUMBER", "RANK"];

    db_core::parser::row::tokenizer::keyword_names()
        .map(str::to_string)
        .chain(
            db_core::functions::SCALAR_FUNCTION_NAMES
                .iter()
                .map(|f| (*f).to_string()),
        )
        .chain(CLAUSE_PHRASES.iter().map(|w| (*w).to_string()))
        .chain(AGGREGATE_AND_WINDOW_NAMES.iter().map(|w| (*w).to_string()))
        .collect()
}

/// Ranks `candidates` against `needle`, best match first, truncated to
/// `limit`. Empty for an empty needle -- there is nothing to complete
/// against yet.
///
/// Both sides are lowercased before reaching `nucleo_matcher` rather
/// than relying on `Config::DEFAULT`'s own `ignore_case` (db-studio#48):
/// an all-uppercase haystack (SQL keywords render `SELECT`-style, not
/// `select`) hits a real panic inside `nucleo-matcher` 0.3.1's optimal
/// matcher (`fuzzy_optimal.rs`'s "should have been caught by prefilter"
/// assertion) for specific needle/haystack shapes -- e.g. needle `"err"`
/// against haystack `"DEFERRED"` -- that its own ASCII prefilter and
/// `ignore_case` path disagree about. Matching two already-lowercase,
/// already-ASCII strings never reaches that code path. Case is display-
/// only here (candidates keep their original casing; only the copies
/// fed to the matcher are folded), so this changes nothing about what
/// ranks or how, only what `nucleo_matcher` internally compares.
pub fn rank(candidates: &[String], needle: &str, limit: usize) -> Vec<String> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let needle_lower = needle.to_lowercase();
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(&needle_lower, &mut needle_buf);

    let mut scored: Vec<(u16, &String)> = candidates
        .iter()
        .filter_map(|candidate| {
            let lower = candidate.to_lowercase();
            let mut buf = Vec::new();
            let haystack = Utf32Str::new(&lower, &mut buf);
            matcher
                .fuzzy_match(haystack, needle)
                .map(|score| (score, candidate))
        })
        .collect();
    scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, c)| c.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use db_core::engine::{ColumnInfo, TableInfo};

    fn sample_tables() -> Vec<TableInfo> {
        vec![TableInfo {
            name: "items".to_string(),
            columns: vec![
                ColumnInfo {
                    name: "id".to_string(),
                    type_name: "INTEGER".to_string(),
                },
                ColumnInfo {
                    name: "price".to_string(),
                    type_name: "REAL".to_string(),
                },
            ],
        }]
    }

    #[test]
    fn candidates_includes_table_and_column_names() {
        let names = candidates(&sample_tables());
        assert_eq!(names, vec!["items", "id", "price"]);
    }

    #[test]
    fn rank_finds_a_fuzzy_subsequence_match() {
        let names = candidates(&sample_tables());
        let ranked = rank(&names, "prc", 5);
        assert_eq!(ranked, vec!["price"]);
    }

    #[test]
    fn rank_prefers_a_closer_match_first() {
        let names = vec!["price".to_string(), "priceless".to_string()];
        let ranked = rank(&names, "price", 5);
        assert_eq!(ranked.first(), Some(&"price".to_string()));
    }

    #[test]
    fn rank_is_empty_for_an_empty_needle() {
        let names = candidates(&sample_tables());
        assert!(rank(&names, "", 5).is_empty());
    }

    #[test]
    fn rank_respects_the_limit() {
        let names = vec!["ab".to_string(), "abc".to_string(), "abcd".to_string()];
        assert_eq!(rank(&names, "a", 2).len(), 2);
    }

    #[test]
    fn keywords_includes_clauses_functions_and_phrases() {
        let words = keywords();
        assert!(words.contains(&"SELECT".to_string()));
        assert!(words.contains(&"WHERE".to_string()));
        assert!(words.contains(&"GROUP BY".to_string()));
        assert!(words.contains(&"COUNT".to_string()));
        assert!(words.iter().any(|w| w == "like" || w == "regexp_extract"));
    }

    /// db-studio#48: reproduces the exact scenario that used to panic
    /// inside `nucleo_matcher` before `rank` started lowercasing both
    /// sides -- an all-uppercase keyword candidate (`DEFERRED`, from
    /// `keywords()`) ranked against a lowercase-typed needle. Regression
    /// test for that fix, not a general fuzz of the matcher.
    #[test]
    fn ranking_keywords_against_typed_text_does_not_panic() {
        let words = keywords();
        for needle in ["s", "se", "sel", "e", "er", "err", "erro", "error"] {
            let _ = rank(&words, needle, 8);
        }
    }
}
