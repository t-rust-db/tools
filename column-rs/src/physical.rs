//! `.physical` (tools#86): how a table directory or Parquet file is laid
//! out on disk -- the manifest, the WAL, the files and each file's Parquet
//! column tree. It reads the manifest, the WAL and the Parquet footers;
//! the files are memory-mapped, so only their footer pages are touched.

use db_core::storage::ingest::layout::date_string;
use db_core::storage::ingest::{Area, KeyBound, LayoutKind, Snapshot, WAL_DIR};
use db_core::storage::parquet::footer::{
    parse_footer, Codec, ConvertedType, FileMetaData, LogicalType, PhysicalType, Repetition,
    RowGroup, SchemaElement,
};
use db_core::storage::parquet::page::Encoding;
use db_core::storage::parquet::schema_tree::{build_schema_tree, SchemaNode};
use std::collections::HashMap;
use std::path::Path;

/// What `.physical` adds to the default structure.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Bytes per file and, per column, stored and raw bytes and share.
    pub sizes: bool,
    /// The column section per row group instead of added up.
    pub rowgroups: bool,
}

/// The report for table `name` at `path`: a table directory or a Parquet
/// file.
pub fn describe(name: &str, path: &Path, options: Options) -> Result<Vec<String>, String> {
    if path.is_dir() {
        let snapshot = Snapshot::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        table_dir(name, path, &snapshot, options)
    } else {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let meta = parse_footer(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut lines = vec![format!("{name}  (Parquet file {})", path.display())];
        lines.push(String::new());
        if options.rowgroups {
            lines.extend(per_row_group(&meta, None, options));
        } else {
            lines.extend(row_groups(&meta));
            lines.push(String::new());
            lines.extend(columns(&[&meta], options));
        }
        Ok(lines)
    }
}

fn table_dir(
    name: &str,
    path: &Path,
    snapshot: &Snapshot,
    options: Options,
) -> Result<Vec<String>, String> {
    let manifest = snapshot.manifest();
    let mut footers = Vec::new();
    for (entry, bytes) in snapshot.files() {
        footers.push(parse_footer(bytes).map_err(|e| format!("{}: {e}", entry.path))?);
    }
    let in_files: u64 = manifest.files.iter().map(|f| f.rows).sum();
    let pending: usize = snapshot.pages().iter().map(|p| p.len()).sum();
    // The sort key isn't recorded; the key ranges name its columns.
    let sort_key = manifest
        .files
        .iter()
        .map(|f| f.keys.iter().map(|k| k.column.as_str()).collect::<Vec<_>>())
        .max_by_key(Vec::len)
        .unwrap_or_default();

    let mut layout = format!(
        "{}, manifest v{}",
        manifest.layout.as_str(),
        manifest.version()
    );
    if !sort_key.is_empty() {
        layout.push_str(&format!(", sort key ({})", sort_key.join(", ")));
    }
    let mut lines = vec![
        format!("{name}  (table directory {})", path.display()),
        format!("  layout     {layout}"),
        format!(
            "  rows       {in_files} in files, {pending} pending in WAL (flushed_lsn {})",
            manifest.flushed_lsn
        ),
        format!("  wal        {}", wal(&path.join(WAL_DIR), options)),
        format!(
            "  files      {}",
            file_counts(manifest.layout, &manifest.files)
        ),
    ];

    if !manifest.files.is_empty() {
        lines.push(String::new());
        lines.push("files".to_string());
        let mut header = ["area", "partition", "lsn", "rows", "row groups"]
            .map(String::from)
            .to_vec();
        if options.sizes {
            header.push("bytes".to_string());
        }
        header.push("arrived (UTC)".to_string());
        let mut rows = vec![header];
        for (entry, meta) in manifest.files.iter().zip(&footers) {
            let mut row = vec![
                entry.area.as_str().to_string(),
                partition(&entry.path),
                format!("{}-{}", entry.lsn_lo, entry.lsn_hi),
                entry.rows.to_string(),
                meta.row_groups.len().to_string(),
            ];
            if options.sizes {
                row.push(human_bytes(entry.bytes));
            }
            row.push(entry.observed_ts.map_or_else(|| "-".to_string(), arrived));
            rows.push(row);
        }
        let right = if options.sizes { &[3, 5][..] } else { &[3][..] };
        let mut table = align(&rows, right).into_iter().map(|l| format!("  {l}"));
        let indent = " ".repeat(2 + rows.iter().map(|r| r[0].len()).max().unwrap_or(0) + 2);
        lines.extend(table.next());
        for (entry, line) in manifest.files.iter().zip(table) {
            lines.push(line);
            let width = entry.keys.iter().map(|k| k.column.len()).max().unwrap_or(0);
            for (i, key) in entry.keys.iter().enumerate() {
                let label = if i == 0 { "keys" } else { "    " };
                lines.push(format!(
                    "{indent}{label} {:width$} {} … {}",
                    key.column,
                    bound(&key.min),
                    bound(&key.max)
                ));
            }
        }
    }

    if !footers.is_empty() {
        lines.push(String::new());
        let metas: Vec<&FileMetaData> = footers.iter().collect();
        if options.rowgroups {
            for (entry, meta) in manifest.files.iter().zip(&footers) {
                lines.extend(per_row_group(meta, Some(&entry.path), options));
            }
        } else {
            lines.extend(columns(&metas, options));
        }
    }
    Ok(lines)
}

/// The WAL directory's segments, by name.
fn wal(dir: &Path, options: Options) -> String {
    let mut segments: Vec<(String, u64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let size = e.metadata().map_or(0, |m| m.len());
            name.ends_with(".wal").then_some((name, size))
        })
        .collect();
    segments.sort();
    let names = match segments.as_slice() {
        [] => return "no segments".to_string(),
        [(only, _)] => format!("1 segment, {only}"),
        [(first, _), .., (last, _)] => format!("{} segments, {first} … {last}", segments.len()),
    };
    if options.sizes {
        let total = segments.iter().map(|(_, size)| size).sum();
        format!("{names}, {}", human_bytes(total))
    } else {
        names
    }
}

fn file_counts(layout: LayoutKind, files: &[db_core::storage::ingest::FileEntry]) -> String {
    let count = |area| files.iter().filter(|f| f.area == area).count();
    match layout {
        LayoutKind::Flat => files.len().to_string(),
        LayoutKind::Tiered => {
            let mut parts = vec![format!("live {}", count(Area::Live))];
            if count(Area::Staging) > 0 {
                parts.push(format!("staging {}", count(Area::Staging)));
            }
            parts.push(format!("archive {}", count(Area::Archive)));
            format!("{} ({})", files.len(), parts.join(", "))
        }
    }
}

/// The `key=value` directories between a file's area and its name, or
/// `-` for a flat table's `data/` file.
fn partition(path: &str) -> String {
    let dirs: Vec<&str> = path.split('/').filter(|p| p.contains('=')).collect();
    if dirs.is_empty() {
        "-".to_string()
    } else {
        dirs.join("/")
    }
}

/// First and last arrival, `MM-DD HH:MM:SS`; the date once if both fall
/// on the same day.
fn arrived((lo, hi): (i64, i64)) -> String {
    let (lo_day, lo_time) = utc(lo);
    let (hi_day, hi_time) = utc(hi);
    if lo_day == hi_day {
        format!("{lo_day} {lo_time} – {hi_time}")
    } else {
        format!("{lo_day} {lo_time} – {hi_day} {hi_time}")
    }
}

/// `(MM-DD, HH:MM:SS)` of nanoseconds since the epoch, in UTC.
fn utc(ns: i64) -> (String, String) {
    let secs = ns.div_euclid(1_000_000_000);
    let day = secs.div_euclid(86_400);
    let s = secs.rem_euclid(86_400);
    let date = date_string(day);
    let month_day = date.get(5..).unwrap_or(&date).to_string();
    (
        month_day,
        format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
    )
}

fn bound(b: &KeyBound) -> String {
    match b {
        KeyBound::Int(i) => i.to_string(),
        KeyBound::Str(s) => s.clone(),
    }
}

/// A single file's row groups.
fn row_groups(meta: &FileMetaData) -> Vec<String> {
    let mut rows = vec![["row group", "rows", "stored", "raw"]
        .map(String::from)
        .to_vec()];
    for (i, rg) in meta.row_groups.iter().enumerate() {
        let (stored, raw) = chunk_sizes(rg);
        rows.push(vec![
            i.to_string(),
            rg.num_rows.to_string(),
            human_bytes(stored),
            human_bytes(raw),
        ]);
    }
    let mut lines = vec![format!("row groups  {}", meta.row_groups.len())];
    lines.extend(
        align(&rows, &[0, 1, 2, 3])
            .into_iter()
            .map(|l| format!("  {l}")),
    );
    lines
}

/// Each row group's column section, headed by its number and rows.
fn per_row_group(meta: &FileMetaData, file: Option<&str>, options: Options) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, rg) in meta.row_groups.iter().enumerate() {
        let of = file.map(|f| format!(" of {f}")).unwrap_or_default();
        lines.push(format!("row group {i}{of}  ({} rows)", rg.num_rows));
        lines.extend(column_tree(meta, &stats([rg]), options));
        lines.push(String::new());
    }
    lines.pop();
    lines
}

fn chunk_sizes(rg: &RowGroup) -> (u64, u64) {
    let size = |v: i64| u64::try_from(v).unwrap_or(0);
    rg.columns
        .iter()
        .filter_map(|c| c.meta_data.as_ref())
        .fold((0, 0), |(s, r), m| {
            (
                s + size(m.total_compressed_size),
                r + size(m.total_uncompressed_size),
            )
        })
}

/// The column tree of the newest file, with each leaf's chunks added up
/// over every file's row groups.
fn columns(metas: &[&FileMetaData], options: Options) -> Vec<String> {
    let Some(newest) = metas.last() else {
        return Vec::new();
    };
    let groups = metas.iter().flat_map(|m| &m.row_groups);
    column_tree(newest, &stats(groups), options)
}

/// One leaf's chunks, added up.
#[derive(Default)]
struct Chunks {
    encodings: Vec<Encoding>,
    codec: Option<Codec>,
    stored: u64,
    raw: u64,
}

fn stats<'a>(groups: impl IntoIterator<Item = &'a RowGroup>) -> HashMap<Vec<String>, Chunks> {
    let mut out: HashMap<Vec<String>, Chunks> = HashMap::new();
    let chunks = groups.into_iter().flat_map(|g| &g.columns);
    for meta in chunks.filter_map(|c| c.meta_data.as_ref()) {
        let chunks = out.entry(meta.path_in_schema.clone()).or_default();
        for e in &meta.encodings {
            if !chunks.encodings.contains(e) {
                chunks.encodings.push(*e);
            }
        }
        chunks.codec.get_or_insert(meta.codec);
        chunks.stored += u64::try_from(meta.total_compressed_size).unwrap_or(0);
        chunks.raw += u64::try_from(meta.total_uncompressed_size).unwrap_or(0);
    }
    out
}

fn column_tree(
    meta: &FileMetaData,
    stats: &HashMap<Vec<String>, Chunks>,
    options: Options,
) -> Vec<String> {
    let mut header = [
        "columns",
        "physical",
        "rep.",
        "logical",
        "encoding",
        "compression",
    ]
    .map(String::from)
    .to_vec();
    if options.sizes {
        header.extend(["stored", "raw", "share"].map(String::from));
    }
    let total_stored: u64 = stats.values().map(|c| c.stored).sum();
    let total_raw: u64 = stats.values().map(|c| c.raw).sum();
    let mut rows = vec![header];
    let root = build_schema_tree(&meta.schema);
    for child in &root.children {
        tree_rows(
            child,
            1,
            &mut Vec::new(),
            stats,
            total_stored,
            options,
            &mut rows,
        );
    }
    if options.sizes {
        let mut total = vec![String::new(); 9];
        total[0] = "  total".to_string();
        total[6] = human_bytes(total_stored);
        total[7] = human_bytes(total_raw);
        rows.push(total);
    }
    let right = if options.sizes {
        &[6, 7, 8][..]
    } else {
        &[][..]
    };
    align(&rows, right)
}

fn tree_rows(
    node: &SchemaNode,
    depth: usize,
    path: &mut Vec<String>,
    stats: &HashMap<Vec<String>, Chunks>,
    total_stored: u64,
    options: Options,
    rows: &mut Vec<Vec<String>>,
) {
    path.push(node.name.clone());
    let e = &node.element;
    let mut row = vec![
        format!("{}{}", "  ".repeat(depth), node.name),
        physical(e),
        e.repetition.map_or("", repetition).to_string(),
        logical(e),
    ];
    match stats.get(path.as_slice()).filter(|_| node.is_leaf()) {
        Some(chunks) => {
            row.push(encoding(&chunks.encodings).to_string());
            row.push(chunks.codec.map_or_else(String::new, codec));
            if options.sizes {
                row.push(human_bytes(chunks.stored));
                row.push(human_bytes(chunks.raw));
                row.push(share(chunks.stored, total_stored));
            }
        }
        None => row.extend(vec![String::new(); if options.sizes { 5 } else { 2 }]),
    }
    rows.push(row);
    for child in &node.children {
        tree_rows(child, depth + 1, path, stats, total_stored, options, rows);
    }
    path.pop();
}

fn physical(e: &SchemaElement) -> String {
    let Some(t) = e.physical_type else {
        return "group".to_string();
    };
    match t {
        PhysicalType::Boolean => "BOOLEAN".to_string(),
        PhysicalType::Int32 => "INT32".to_string(),
        PhysicalType::Int64 => "INT64".to_string(),
        PhysicalType::Int96 => "INT96".to_string(),
        PhysicalType::Float => "FLOAT".to_string(),
        PhysicalType::Double => "DOUBLE".to_string(),
        PhysicalType::ByteArray => "BYTE_ARRAY".to_string(),
        PhysicalType::FixedLenByteArray => {
            format!("FIXED_LEN_BYTE_ARRAY({})", e.type_length.unwrap_or(0))
        }
        PhysicalType::Unknown(n) => format!("type {n}"),
    }
}

fn repetition(r: Repetition) -> &'static str {
    match r {
        Repetition::Required => "required",
        Repetition::Optional => "optional",
        Repetition::Repeated => "repeated",
        Repetition::Unknown(_) => "?",
    }
}

fn unit(units: Option<i64>) -> &'static str {
    match units {
        Some(1_000) => "ms",
        Some(1_000_000) => "us",
        Some(1_000_000_000) => "ns",
        _ => "?",
    }
}

/// The `LogicalType` annotation, or the old `ConvertedType` one when the
/// writer set only that. A timestamp not adjusted to UTC is a local
/// wall-clock time (DuckDB reads it as `TIMESTAMP`, not `TIMESTAMPTZ`).
fn logical(e: &SchemaElement) -> String {
    let local = |utc: bool| if utc { "" } else { ", local" };
    if let Some(l) = e.logical_type {
        return match l {
            LogicalType::String => "STRING".to_string(),
            LogicalType::Map => "MAP".to_string(),
            LogicalType::List => "LIST".to_string(),
            LogicalType::Enum => "ENUM".to_string(),
            LogicalType::Decimal { scale, precision } => format!("DECIMAL({precision},{scale})"),
            LogicalType::Date => "DATE".to_string(),
            LogicalType::Time { units, utc } => format!("TIME({}{})", unit(units), local(utc)),
            LogicalType::Timestamp { units, utc } => {
                format!("TIMESTAMP({}{})", unit(units), local(utc))
            }
            LogicalType::Integer { bit_width, signed } => {
                format!("{}INT{bit_width}", if signed { "" } else { "U" })
            }
            LogicalType::Unknown => "UNKNOWN".to_string(),
            LogicalType::Json => "JSON".to_string(),
            LogicalType::Bson => "BSON".to_string(),
            LogicalType::Uuid => "UUID".to_string(),
            LogicalType::Float16 => "FLOAT16".to_string(),
            LogicalType::Other(n) => format!("logical {n}"),
        };
    }
    match e.converted_type {
        None => String::new(),
        Some(ConvertedType::Decimal) => format!(
            "DECIMAL({},{})",
            e.precision.unwrap_or(0),
            e.scale.unwrap_or(0)
        ),
        Some(ConvertedType::TimestampMillis) => "TIMESTAMP(ms)".to_string(),
        Some(ConvertedType::TimestampMicros) => "TIMESTAMP(us)".to_string(),
        Some(ConvertedType::Uint8) => "UINT8".to_string(),
        Some(ConvertedType::Uint16) => "UINT16".to_string(),
        Some(ConvertedType::Uint32) => "UINT32".to_string(),
        Some(ConvertedType::Uint64) => "UINT64".to_string(),
        // parquet.thrift's ConvertedType numbering.
        Some(ConvertedType::Other(n)) => match n {
            0 => "STRING",
            1 | 2 => "MAP",
            3 => "LIST",
            4 => "ENUM",
            6 => "DATE",
            7 => "TIME(ms)",
            8 => "TIME(us)",
            15 => "INT8",
            16 => "INT16",
            17 => "INT32",
            18 => "INT64",
            19 => "JSON",
            20 => "BSON",
            21 => "INTERVAL",
            _ => "?",
        }
        .to_string(),
    }
}

/// How a leaf's values are encoded: `dict` when any page is
/// dictionary-encoded, else its value encoding (RLE alone encodes levels).
fn encoding(encodings: &[Encoding]) -> &'static str {
    let dictionary =
        |e: &Encoding| matches!(e, Encoding::PlainDictionary | Encoding::RleDictionary);
    if encodings.iter().any(dictionary) {
        return "dict";
    }
    let value = encodings.iter().find(|e| **e != Encoding::Rle);
    match value.or(encodings.first()) {
        None => "",
        Some(Encoding::Plain) => "plain",
        Some(Encoding::Rle) => "rle",
        Some(Encoding::DeltaBinaryPacked) => "delta",
        Some(Encoding::Other(6)) => "delta_length",
        Some(Encoding::Other(7)) => "delta_bytes",
        Some(Encoding::Other(9)) => "byte_stream_split",
        Some(_) => "?",
    }
}

fn codec(c: Codec) -> String {
    match c {
        Codec::Uncompressed => "none".to_string(),
        Codec::Snappy => "SNAPPY".to_string(),
        Codec::Zstd => "ZSTD".to_string(),
        Codec::Other(2) => "GZIP".to_string(),
        Codec::Other(3) => "LZO".to_string(),
        Codec::Other(4) => "BROTLI".to_string(),
        Codec::Other(5) => "LZ4".to_string(),
        Codec::Other(7) => "LZ4_RAW".to_string(),
        Codec::Other(n) => format!("codec {n}"),
    }
}

fn share(part: u64, total: u64) -> String {
    if total == 0 {
        return String::new();
    }
    format!("{}%", (part * 100 + total / 2) / total)
}

/// Bytes with three significant digits in decimal units: `512 B`,
/// `24.8 KB`, `510 KB`, `1.20 MB`.
pub fn human_bytes(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1000.0;
    for unit in ["KB", "MB", "GB"] {
        // Compare before rounding, so 999.7 KB shows as 1.00 MB.
        if v < 9.995 {
            return format!("{v:.2} {unit}");
        } else if v < 99.95 {
            return format!("{v:.1} {unit}");
        } else if v < 999.5 {
            return format!("{v:.0} {unit}");
        }
        v /= 1000.0;
    }
    format!("{v:.2} TB")
}

/// Rows aligned in columns two spaces apart; the columns in `right` are
/// right-aligned. Trailing blanks are trimmed.
fn align(rows: &[Vec<String>], right: &[usize]) -> Vec<String> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(c, (cell, &w))| {
                    if right.contains(&c) {
                        format!("{cell:>w$}")
                    } else {
                        format!("{cell:w$}")
                    }
                })
                .collect();
            cells.join("  ").trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_keeps_three_significant_digits() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1.00 KB");
        assert_eq!(human_bytes(24_800), "24.8 KB");
        assert_eq!(human_bytes(510_400), "510 KB");
        assert_eq!(human_bytes(999_700), "1.00 MB");
        assert_eq!(human_bytes(1_200_000), "1.20 MB");
    }

    #[test]
    fn partition_is_the_key_value_directories() {
        assert_eq!(
            partition("archive/date=2026-10-24/00000001.parquet"),
            "date=2026-10-24"
        );
        assert_eq!(
            partition("live/date=2026-10-24/hour=15/00000001.parquet"),
            "date=2026-10-24/hour=15"
        );
        assert_eq!(partition("data/00000001.parquet"), "-");
    }

    #[test]
    fn arrived_names_the_date_once_on_one_day() {
        // 2026-10-24 15:53:29 UTC.
        let t = 1_792_857_209_000_000_000;
        assert_eq!(arrived((t, t + 1_000_000_000)), "10-24 15:53:29 – 15:53:30");
        assert_eq!(
            arrived((t, t + 86_400_000_000_000)),
            "10-24 15:53:29 – 10-25 15:53:29"
        );
    }

    #[test]
    fn align_pads_left_and_right_aligned_columns() {
        let rows = vec![
            vec!["a".to_string(), "rows".to_string(), "x".to_string()],
            vec!["long".to_string(), "7".to_string(), String::new()],
        ];
        assert_eq!(align(&rows, &[1]), ["a     rows  x", "long     7"]);
    }
}
