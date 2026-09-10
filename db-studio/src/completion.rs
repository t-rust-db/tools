//! Fuzzy ranking for the query pane's completion popup (db-studio#11),
//! over the same table/column names the schema tree inspector renders
//! (`db-core#310`'s `Engine::tables()`) -- one catalog fetch, not a
//! second query against the engine.

use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Builds the flat candidate list completion ranks against: every table
/// name plus every column name, in no particular order (ranking sorts
/// by score, not source order).
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

/// Ranks `candidates` against `needle`, best match first, truncated to
/// `limit`. Empty for an empty needle -- there is nothing to complete
/// against yet.
pub fn rank(candidates: &[String], needle: &str, limit: usize) -> Vec<String> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(needle, &mut needle_buf);

    let mut scored: Vec<(u16, &String)> = candidates
        .iter()
        .filter_map(|candidate| {
            let mut buf = Vec::new();
            let haystack = Utf32Str::new(candidate, &mut buf);
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
}
