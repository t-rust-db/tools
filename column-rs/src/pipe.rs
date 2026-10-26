//! `column-rs --pipe` (tools#39): queries from a parent process over stdin,
//! results back as typed JSON Lines, one process for many queries.
//!
//! Input is one request per line, `{"id": ..., "sql": "..."}`; `id` is any
//! JSON value and is echoed back. Output per request, one object per line:
//!
//! ```text
//! {"id":1,"columns":[{"name":"n","type":"BIGINT"}, ...]}
//! {"id":1,"row":[3, ...]}                          (one per row)
//! {"id":1,"done":true,"rows":1,"elapsed_ms":2}
//! ```
//!
//! or `{"id":1,"error":{"kind":"...","message":"..."}}` in place of the
//! header, rows and `done`. The process then reads the next request. End of
//! input exits.
//!
//! Values are typed by the column they come from. A bare column reference
//! takes its table's type: a Parquet file's schema, or a table directory's
//! (`Snapshot::schema`). An expression's type is inferred from its values.
//!
//! - Integers are JSON numbers up to 2^53, strings beyond, so no parent
//!   loses precision.
//! - DECIMAL is an exact string at the column's scale. db-core's engine
//!   carries a decimal as a float, so this is exact up to about 15
//!   significant digits.
//! - BLOB is base64, from the engine's hex.
//! - Timestamps are ISO 8601 UTC.
//! - A MAP is a JSON object, and NULL is `null`.

use std::io::{self, BufRead, Write};
use std::time::Instant;

use serde_json::{json, Map, Number, Value as Json};

use crate::query::{QueryEngine, QueryError};
use crate::vm::Value;
use db_core::engine::ErrorKind;

/// A result column's type, as the pipe encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    /// A signed integer.
    Int,
    /// An unsigned integer.
    UInt,
    /// A float.
    Double,
    /// A boolean.
    Bool,
    /// Text.
    Text,
    /// An exact decimal with `scale` digits after the point.
    Decimal {
        /// Total significant digits.
        precision: u8,
        /// Digits after the decimal point.
        scale: u8,
    },
    /// Bytes.
    Blob,
    /// A timestamp, counted in this unit since the epoch.
    Timestamp(TimeUnit),
    /// A `MAP<VARCHAR, VARCHAR>`.
    Map,
    /// Every value NULL: no type to tell.
    Null,
}

/// A timestamp's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    /// Milliseconds.
    Millis,
    /// Microseconds.
    Micros,
    /// Nanoseconds.
    Nanos,
}

impl ColType {
    /// The type's name in the header, as DuckDB names it.
    pub fn name(self) -> String {
        match self {
            ColType::Int => "BIGINT".into(),
            ColType::UInt => "UBIGINT".into(),
            ColType::Double => "DOUBLE".into(),
            ColType::Bool => "BOOLEAN".into(),
            ColType::Text => "VARCHAR".into(),
            ColType::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
            ColType::Blob => "BLOB".into(),
            ColType::Timestamp(_) => "TIMESTAMP".into(),
            ColType::Map => "MAP(VARCHAR, VARCHAR)".into(),
            ColType::Null => "NULL".into(),
        }
    }

    /// The type of a column whose values are `values`, for an expression.
    fn infer<'v>(mut values: impl Iterator<Item = &'v Value>) -> ColType {
        match values.find(|v| !matches!(v, Value::Null)) {
            Some(Value::Int(_)) => ColType::Int,
            Some(Value::Float(_)) => ColType::Double,
            Some(Value::Decimal { scale, .. }) => ColType::Decimal {
                precision: 38,
                scale: *scale,
            },
            Some(Value::Bool(_)) => ColType::Bool,
            Some(Value::Map(_)) => ColType::Map,
            Some(Value::Str(_)) => ColType::Text,
            Some(Value::Null) | None => ColType::Null,
        }
    }
}

/// Largest integer a JSON number carries exactly in every parser.
const EXACT: u64 = 1 << 53;

/// `value` as JSON, by `ty`.
pub fn encode(value: &Value, ty: ColType) -> Json {
    match (value, ty) {
        (Value::Null, _) => Json::Null,
        // db-core#632: exact, printed at its own scale.
        (Value::Decimal { .. }, _) => Json::String(value.to_string()),
        (Value::Int(i), ColType::Timestamp(unit)) => Json::String(iso8601(*i, unit)),
        (Value::Float(f), ColType::Decimal { scale, .. }) => {
            Json::String(format!("{f:.prec$}", prec = usize::from(scale)))
        }
        (Value::Int(i), ColType::Decimal { scale, .. }) => Json::String(
            format!("{i}.{:0>width$}", "", width = usize::from(scale))
                .trim_end_matches('.')
                .to_string(),
        ),
        (Value::Str(hex), ColType::Blob) => match from_hex(hex) {
            Some(bytes) => Json::String(base64(&bytes)),
            None => Json::String(hex.to_string()),
        },
        (Value::Int(i), _) => {
            if i.unsigned_abs() <= EXACT {
                Json::Number(Number::from(*i))
            } else {
                Json::String(i.to_string())
            }
        }
        (Value::Float(f), _) => {
            Number::from_f64(*f).map_or_else(|| Json::String(f.to_string()), Json::Number)
        }
        (Value::Bool(b), _) => Json::Bool(*b),
        (Value::Str(s), _) => Json::String(s.to_string()),
        (Value::Map(entries), _) => Json::Object(
            entries
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        v.as_ref()
                            .map_or(Json::Null, |v| Json::String(v.to_string())),
                    )
                })
                .collect::<Map<_, _>>(),
        ),
    }
}

/// `count` units since the epoch as `YYYY-MM-DDTHH:MM:SS[.f]Z`.
fn iso8601(count: i64, unit: TimeUnit) -> String {
    let (per_second, digits) = match unit {
        TimeUnit::Millis => (1_000, 3),
        TimeUnit::Micros => (1_000_000, 6),
        TimeUnit::Nanos => (1_000_000_000, 9),
    };
    let seconds = count.div_euclid(per_second);
    let fraction = count.rem_euclid(per_second);
    let (days, secs) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, min, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if fraction == 0 {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{fraction:0digits$}Z")
    }
}

fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The error kind a parent can branch on.
fn kind(e: &QueryError) -> &'static str {
    match e {
        QueryError::UnknownColumn(_) => "unknown_column",
        QueryError::UnknownTable(_) => "unknown_table",
        QueryError::UnsupportedSemiJoin(_)
        | QueryError::UnsupportedSelectItem(_)
        | QueryError::UnsupportedJoinKind(_)
        | QueryError::TooManyJoins(_)
        | QueryError::StarWithAggregation
        | QueryError::ScopeClauseUnsupported
        | QueryError::NotStreamable(_) => "unsupported",
        QueryError::TableDirectory(_) | QueryError::File(_) | QueryError::Io(_) => "storage",
        QueryError::Vm(_) => "execution",
        QueryError::Lookup(kind, _) => match kind {
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Open => "storage",
            ErrorKind::Parse | ErrorKind::Compile | ErrorKind::Execute => "execution",
        },
        QueryError::NoJoinClause
        | QueryError::PlannerInvariant(_)
        | QueryError::DuplicateTable(_) => "internal",
    }
}

fn emit(out: &mut impl Write, line: &Json) -> io::Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")
}

/// Answers every request on `input` over `engine` until end of input.
pub fn run(engine: &QueryEngine, input: impl BufRead, out: &mut impl Write) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Json = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                emit(
                    out,
                    &json!({"id": null, "error": {"kind": "request", "message": e.to_string()}}),
                )?;
                out.flush()?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Json::Null);
        let Some(sql) = request.get("sql").and_then(Json::as_str) else {
            emit(
                out,
                &json!({"id": id, "error": {"kind": "request", "message": "a request needs \"sql\": a string"}}),
            )?;
            out.flush()?;
            continue;
        };
        let started = Instant::now();
        match engine.execute_typed(sql) {
            Err(e) => emit(
                out,
                &json!({"id": id, "error": {"kind": kind(&e), "message": e.to_string()}}),
            )?,
            Ok((result, types)) => {
                let rows: Vec<Vec<Value>> = result.rows().collect();
                let types: Vec<ColType> = types
                    .into_iter()
                    .enumerate()
                    .map(|(c, ty)| {
                        ty.unwrap_or_else(|| ColType::infer(rows.iter().filter_map(|r| r.get(c))))
                    })
                    .collect();
                let columns: Vec<Json> = result
                    .columns
                    .iter()
                    .zip(&types)
                    .map(|(name, ty)| json!({"name": name, "type": ty.name()}))
                    .collect();
                emit(out, &json!({"id": id, "columns": columns}))?;
                for row in &rows {
                    let cells: Vec<Json> = row
                        .iter()
                        .zip(&types)
                        .map(|(v, ty)| encode(v, *ty))
                        .collect();
                    emit(out, &json!({"id": id, "row": cells}))?;
                }
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                emit(
                    out,
                    &json!({"id": id, "done": true, "rows": rows.len(), "elapsed_ms": elapsed_ms}),
                )?;
            }
        }
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_beyond_2_pow_53_are_strings() {
        assert_eq!(
            encode(&Value::Int(1 << 53), ColType::Int),
            json!(9_007_199_254_740_992_i64)
        );
        assert_eq!(
            encode(&Value::Int((1 << 53) + 1), ColType::UInt),
            json!("9007199254740993")
        );
        assert_eq!(
            encode(&Value::Int(-((1 << 53) + 1)), ColType::Int),
            json!("-9007199254740993")
        );
    }

    #[test]
    fn decimals_are_exact_strings_at_their_scale() {
        let ty = ColType::Decimal {
            precision: 12,
            scale: 2,
        };
        assert_eq!(encode(&Value::Float(1234.5), ty), json!("1234.50"));
        assert_eq!(encode(&Value::Float(-0.01), ty), json!("-0.01"));
        assert_eq!(encode(&Value::Int(7), ty), json!("7.00"));
        let whole = ColType::Decimal {
            precision: 5,
            scale: 0,
        };
        assert_eq!(encode(&Value::Int(7), whole), json!("7"));
        let exact = Value::Decimal {
            unscaled: 12_345_678_901_234_567_890,
            scale: 2,
        };
        assert_eq!(encode(&exact, ty), json!("123456789012345678.90"));
        assert_eq!(
            ColType::infer([Value::Null, exact].iter()),
            ColType::Decimal {
                precision: 38,
                scale: 2
            }
        );
    }

    #[test]
    fn blobs_are_base64() {
        assert_eq!(encode(&Value::Str("".into()), ColType::Blob), json!(""));
        assert_eq!(
            encode(&Value::Str("66".into()), ColType::Blob),
            json!("Zg==")
        );
        assert_eq!(
            encode(&Value::Str("666f6f".into()), ColType::Blob),
            json!("Zm9v")
        );
        assert_eq!(
            encode(&Value::Str("80ff00".into()), ColType::Blob),
            json!("gP8A")
        );
        assert_eq!(
            encode(&Value::Str("xyz".into()), ColType::Blob),
            json!("xyz"),
            "not hex: as is"
        );
    }

    #[test]
    fn timestamps_are_iso_8601_utc() {
        let micros = ColType::Timestamp(TimeUnit::Micros);
        assert_eq!(
            encode(&Value::Int(0), micros),
            json!("1970-01-01T00:00:00Z")
        );
        assert_eq!(
            encode(&Value::Int(1_791_676_800_000_001), micros),
            json!("2026-10-11T00:00:00.000001Z")
        );
        let nanos = ColType::Timestamp(TimeUnit::Nanos);
        assert_eq!(
            encode(&Value::Int(-1), nanos),
            json!("1969-12-31T23:59:59.999999999Z")
        );
        let millis = ColType::Timestamp(TimeUnit::Millis);
        assert_eq!(
            encode(&Value::Int(951_782_400_123), millis),
            json!("2000-02-29T00:00:00.123Z")
        );
    }

    #[test]
    fn maps_nulls_and_inferred_types() {
        let entries = std::sync::Arc::new(vec![("a".into(), Some("1".into())), ("b".into(), None)]);
        assert_eq!(
            encode(&Value::Map(entries), ColType::Map),
            json!({"a": "1", "b": null})
        );
        assert_eq!(encode(&Value::Null, ColType::Int), Json::Null);
        assert_eq!(
            ColType::infer([Value::Null, Value::Float(1.0)].iter()),
            ColType::Double
        );
        assert_eq!(ColType::infer([Value::Null].iter()), ColType::Null);
        assert_eq!(
            ColType::Decimal {
                precision: 9,
                scale: 2
            }
            .name(),
            "DECIMAL(9,2)"
        );
    }
}
