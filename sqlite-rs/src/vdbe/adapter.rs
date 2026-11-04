// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! db-storage behind db-core's `vm::row` hooks (t-rust-db/sqlite-rs#18):
//! the consumer-side adapter ADR 0008 places here rather than in either
//! shared crate. Semantics are those of this crate's former
//! `src/vdbe/cursor.rs` (Lab271/sqlite-rs), which db-core's dispatcher
//! now expects (db-core#134).
//!
//! This is the one place the VDBE meets the file format, so it is the one
//! `src/vdbe` file that names the pager — always by full path, never through
//! an import of the pager module (`tests/unit/layer_isolation.rs`).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use db_core::value::Collation;
use db_core::vm::row::cursor::{CursorError, CursorResult};
use db_core::vm::row::cursor_factory::{CursorFactory, CursorFactoryError};
use db_core::vm::row::schema_storage::{SchemaStorage, SchemaStorageError};
use db_core::vm::row::transaction::{Transaction, TransactionError};
use db_core::vm::row::{
    compare, AnalyzeTarget, Cursor, SortKeyColumn, JOURNAL_MODE_WAL, SYNCHRONOUS_NORMAL,
    SYNCHRONOUS_OFF,
};

use crate::btree::{self, IndexCursor, IndexRow, Payload, TableCursor};
use crate::header::{DatabaseHeader, JournalMode, SynchronousMode};
use crate::record::{
    decode_record, decode_serial_value, decode_varint, parse_header_into, RecordError, Value,
};
use crate::vfs::{PageError, PageSource};

type SharedPager = Rc<RefCell<crate::pager::Pager>>;

fn storage_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A b-tree or record failure as the VM's cursor error (db-core#873):
/// these used to read as "no row" / "end of table", so a corrupt file
/// returned `count(*)` = 0 or no rows instead of failing.
fn cursor_err(e: impl std::fmt::Display) -> CursorError {
    CursorError(e.to_string())
}

// ---------------------------------------------------------------- tables

/// A table b-tree cursor (`OpenRead`/`OpenWrite` with `p5 = 0`).
pub struct TableCursorAdapter {
    cursor: TableCursor<Rc<dyn PageSource>>,
    source: Rc<dyn PageSource>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
    root_page: u32,
    current_rowid: Option<i64>,
    /// The current row's payload, fetched on the first `column()` after
    /// positioning (`column` takes `&self`, hence the `RefCell`).
    payload: RefCell<Option<Payload>>,
    /// `(serial_type, body_offset)` per column of the current row, parsed
    /// once per row; `column()` decodes only the requested body. Kept apart
    /// from `payload` so the Vec's allocation is reused across rows
    /// (`parse_header_into` clears it) (db-core#533, tools#114).
    entries: RefCell<Vec<(u64, usize)>>,
}

impl TableCursorAdapter {
    fn new(
        source: Rc<dyn PageSource>,
        writer: Option<SharedPager>,
        header: DatabaseHeader,
        root_page: u32,
    ) -> Self {
        TableCursorAdapter {
            cursor: TableCursor::new(Rc::clone(&source), &header, root_page),
            source,
            writer,
            header,
            root_page,
            current_rowid: None,
            payload: RefCell::new(None),
            entries: RefCell::new(Vec::new()),
        }
    }

    fn position(&mut self, rowid: Result<Option<i64>, btree::BtreeError>) -> CursorResult<bool> {
        *self.payload.borrow_mut() = None;
        self.current_rowid = None;
        self.current_rowid = rowid.map_err(cursor_err)?;
        Ok(self.current_rowid.is_some())
    }

    /// Parses the current row's header at most once per positioned row.
    /// `Ok(false)` when there is no current row.
    fn ensure_cached(&self) -> CursorResult<bool> {
        if self.payload.borrow().is_some() {
            return Ok(true);
        }
        if self.current_rowid.is_none() {
            return Ok(false);
        }
        let payload = self.cursor.current_payload().map_err(cursor_err)?;
        parse_header_into(&payload, &mut self.entries.borrow_mut()).map_err(cursor_err)?;
        *self.payload.borrow_mut() = Some(payload);
        Ok(true)
    }

    fn max_rowid(&self) -> CursorResult<i64> {
        let mut probe = TableCursor::new(Rc::clone(&self.source), &self.header, self.root_page);
        Ok(probe.last().map_err(cursor_err)?.unwrap_or(0))
    }
}

impl Cursor for TableCursorAdapter {
    fn rewind(&mut self) -> CursorResult<bool> {
        let r = self.cursor.first();
        self.position(r)
    }

    fn next(&mut self) -> CursorResult<bool> {
        let r = self.cursor.next();
        self.position(r)
    }

    fn last(&mut self) -> CursorResult<bool> {
        let r = self.cursor.last();
        self.position(r)
    }

    fn prev(&mut self) -> CursorResult<bool> {
        // `prev` without a `last` first is the b-tree reporting a call
        // order it doesn't walk, not a damaged file: no row, as before.
        let r = match self.cursor.prev() {
            Err(btree::BtreeError::CursorNotPositioned { .. }) => Ok(None),
            r => r,
        };
        self.position(r)
    }

    fn seek(&mut self, rowid: i64) -> CursorResult<bool> {
        let r = self
            .cursor
            .seek(rowid)
            .map(|found| found.filter(|f| *f == rowid));
        self.position(r)
    }

    fn column(&self, col: usize) -> CursorResult<Option<Value>> {
        // `None` is "no current row" (db-core#231); a column index past the
        // record's end is `Some(Null)`, SQLite's short-record rule. A record
        // that doesn't decode is an error, not "no current row" (db-core#873).
        if !self.ensure_cached()? {
            return Ok(None);
        }
        let payload = self.payload.borrow();
        let Some(payload) = payload.as_ref() else {
            return Ok(None);
        };
        match self.entries.borrow().get(col) {
            Some(&(serial_type, offset)) => {
                let (value, _) =
                    decode_serial_value(serial_type, payload, offset, self.header.text_encoding)
                        .map_err(cursor_err)?;
                Ok(Some(value))
            }
            None => Ok(Some(Value::Null)),
        }
    }

    fn rowid(&self) -> Option<i64> {
        self.current_rowid
    }

    fn payload(&self) -> CursorResult<Option<Rc<[u8]>>> {
        if self.current_rowid.is_none() {
            return Ok(None);
        }
        let payload = self.cursor.current_payload().map_err(cursor_err)?;
        Ok(Some(Rc::from(&payload[..])))
    }

    fn insert_payload(&mut self, rowid: i64, payload: &Rc<[u8]>) -> CursorResult<Option<bool>> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(None);
        };
        let inserted = btree::insert_row(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            rowid,
            payload,
        );
        *self.payload.borrow_mut() = None;
        inserted.map_err(cursor_err)?;
        Ok(Some(true))
    }

    fn insert(&mut self, rowid: i64, values: Vec<Value>) -> CursorResult<bool> {
        let payload = crate::record::encode_record(&values, self.header.text_encoding);
        Ok(self
            .insert_payload(rowid, &Rc::from(payload))?
            .unwrap_or(false))
    }

    fn delete(&mut self) -> CursorResult<bool> {
        let (Some(writer), Some(rowid)) = (self.writer.as_ref(), self.current_rowid) else {
            return Ok(false);
        };
        let deleted = btree::delete_row(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            rowid,
        );
        self.current_rowid = None;
        *self.payload.borrow_mut() = None;
        deleted.map_err(cursor_err)?;
        Ok(true)
    }

    fn next_rowid(&self) -> CursorResult<i64> {
        Ok(self.max_rowid()?.saturating_add(1))
    }

    fn count(&self) -> CursorResult<Option<i64>> {
        btree::count_table_rows(&self.source, self.root_page)
            .map(Some)
            .map_err(cursor_err)
    }
}

// --------------------------------------------------------------- indexes

/// A secondary-index b-tree cursor (`OpenRead`/`OpenWrite` with `p5 = 1`).
pub struct IndexCursorAdapter {
    cursor: IndexCursor<Rc<dyn PageSource>>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
    root_page: u32,
    /// The entry most recently positioned on, `None` after a miss. Its key
    /// record (key columns then the trailing rowid) stays encoded: a
    /// column is decoded only when something reads it (#113, as
    /// db-core#949).
    current: Option<IndexRow>,
    /// The current entry's header entries (serial type, body offset per
    /// column), parsed when the cursor is positioned. Its allocation is
    /// reused for every entry, so positioning allocates nothing.
    entries: Vec<(u64, usize)>,
}

impl IndexCursorAdapter {
    fn new(
        source: Rc<dyn PageSource>,
        writer: Option<SharedPager>,
        header: DatabaseHeader,
        root_page: u32,
    ) -> Self {
        IndexCursorAdapter {
            cursor: IndexCursor::new(source, header.usable_page_size(), root_page),
            writer,
            header,
            root_page,
            current: None,
            entries: Vec::new(),
        }
    }

    /// Positions on `row`. The header is walked and the body length
    /// checked (a malformed record is an error here, as when the whole
    /// record was decoded), but no column body is decoded.
    fn set_current(
        &mut self,
        row: Result<Option<IndexRow>, btree::BtreeError>,
    ) -> CursorResult<bool> {
        self.current = None;
        let Some(row) = row.map_err(cursor_err)? else {
            return Ok(false);
        };
        parse_header_checked_into(&row.payload, &mut self.entries).map_err(cursor_err)?;
        self.current = Some(row);
        Ok(true)
    }

    /// Column `col` of the current entry, decoded now; a column past the
    /// record's end is NULL. `Ok(None)` when there is no current entry.
    fn decode_column(&self, col: usize) -> CursorResult<Option<Value>> {
        let Some(row) = self.current.as_ref() else {
            return Ok(None);
        };
        match self.entries.get(col) {
            Some(&(serial_type, offset)) => {
                let (value, _) = decode_serial_value(
                    serial_type,
                    &row.payload,
                    offset,
                    self.header.text_encoding,
                )
                .map_err(cursor_err)?;
                Ok(Some(value))
            }
            None => Ok(Some(Value::Null)),
        }
    }

    /// sqlite-rs `decode_leading_columns`: the entry's first `probe.len()`
    /// columns compared to `probe` under `collations`, or `None` when the
    /// entry has no trailing rowid column beyond them. Only those leading
    /// columns are decoded.
    fn compare_leading(&self, probe: &[Value], collations: &[Collation]) -> Option<Ordering> {
        self.current.as_ref()?;
        if self.entries.len() <= probe.len() {
            return None;
        }
        for (col, (p, &c)) in probe
            .iter()
            .zip(
                collations
                    .iter()
                    .chain(std::iter::repeat(&Collation::Binary)),
            )
            .enumerate()
        {
            // The header was checked when the cursor was positioned, so a
            // body that still fails to decode (invalid text) has no
            // ordering: report "no entry" rather than a wrong one.
            let key = self.decode_column(col).ok()??;
            let ordering = compare(&key, p, c);
            if !ordering.is_eq() {
                return Some(ordering);
            }
        }
        Some(Ordering::Equal)
    }
}

/// [`parse_header_into`], plus the length check `decode_record` makes: the
/// bodies the header declares end exactly at the end of `payload`. The
/// lazy index cursor calls this when it positions, so a truncated or
/// over-long record is still an error there without decoding any body.
/// db-core's own copy (db-core#949) is crate-private.
fn parse_header_checked_into(
    payload: &[u8],
    entries: &mut Vec<(u64, usize)>,
) -> Result<(), RecordError> {
    parse_header_into(payload, entries)?;
    let end = match entries.last() {
        Some(&(serial_type, offset)) => offset.saturating_add(serial_type_len(serial_type)),
        None => {
            let (header_len, _) = decode_varint(payload)?;
            usize::try_from(header_len).unwrap_or(usize::MAX)
        }
    };
    if end > payload.len() {
        return Err(RecordError::UnexpectedEof { offset: end });
    }
    if end < payload.len() {
        return Err(RecordError::TrailingData {
            trailing: payload.len().saturating_sub(end),
        });
    }
    Ok(())
}

/// The body length of a record column of `serial_type` (file format
/// 2.1); 10 and 11 are reserved and have no body.
fn serial_type_len(serial_type: u64) -> usize {
    let len = match serial_type {
        0 | 8..=11 => 0,
        1..=4 => serial_type,
        5 => 6,
        6 | 7 => 8,
        n => n.saturating_sub(12) / 2,
    };
    usize::try_from(len).unwrap_or(usize::MAX)
}

impl Cursor for IndexCursorAdapter {
    fn rewind(&mut self) -> CursorResult<bool> {
        let r = self.cursor.first();
        self.set_current(r)
    }

    fn next(&mut self) -> CursorResult<bool> {
        let r = self.cursor.next();
        self.set_current(r)
    }

    fn last(&mut self) -> CursorResult<bool> {
        let r = self.cursor.last();
        self.set_current(r)
    }

    fn prev(&mut self) -> CursorResult<bool> {
        let r = self.cursor.prev();
        self.set_current(r)
    }

    fn column(&self, col: usize) -> CursorResult<Option<Value>> {
        self.decode_column(col)
    }

    fn rowid(&self) -> Option<i64> {
        self.idx_rowid()
    }

    fn idx_rowid(&self) -> Option<i64> {
        // The trailing rowid is the last column; nothing else is decoded.
        match self.decode_column(self.entries.len().checked_sub(1)?) {
            Ok(Some(Value::Integer(rowid))) => Some(rowid),
            _ => None,
        }
    }

    fn seek_index_eq(&mut self, key: &[Value], collations: &[Collation]) -> CursorResult<bool> {
        let r = self.cursor.seek(key, self.header.text_encoding);
        if !self.set_current(r)? {
            return Ok(false);
        }
        let matched = self.compare_leading(key, collations) == Some(Ordering::Equal);
        if !matched {
            self.current = None;
        }
        Ok(matched)
    }

    fn seek_index_ge(&mut self, key: &[Value], _collations: &[Collation]) -> CursorResult<bool> {
        let r = self.cursor.seek(key, self.header.text_encoding);
        self.set_current(r)
    }

    fn idx_compare(&self, key: &[Value], collations: &[Collation]) -> Option<Ordering> {
        self.current.as_ref()?;
        // No trailing rowid beyond the probe: sqlite-rs treats it as
        // "not greater".
        Some(
            self.compare_leading(key, collations)
                .unwrap_or(Ordering::Equal),
        )
    }

    fn idx_insert(&mut self, key: Vec<Value>) -> CursorResult<bool> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(false);
        };
        btree::insert_entry(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            &key,
            self.header.text_encoding,
        )
        .map_err(cursor_err)?;
        Ok(true)
    }

    fn idx_delete(&mut self, key: &[Value]) -> CursorResult<bool> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(false);
        };
        btree::delete_entry(
            &mut writer.borrow_mut(),
            &self.header,
            self.root_page,
            key,
            self.header.text_encoding,
        )
        .map_err(cursor_err)?;
        Ok(true)
    }
}

// --------------------------------------------------------------- factory

/// Type-erases any shared page source into the `Rc<dyn PageSource>` this
/// module stores. Kept here — the one `dyn` storage boundary (ADR-0013,
/// `MVL_LIMIT_EXCLUDE`) — so `src/vdbe.rs` and every caller stay generic
/// and limit-clean. The extra `Rc` hop is one pointer chase per page
/// read; `P` may itself be unsized (an `Rc<dyn PageSource>` a caller
/// already holds).
struct ErasedSource<P: ?Sized>(Rc<P>);

impl<P: PageSource + ?Sized> PageSource for ErasedSource<P> {
    fn read_page(&self, page_num: u32) -> Result<Rc<[u8]>, PageError> {
        self.0.read_page(page_num)
    }
}

fn erase<P: PageSource + ?Sized + 'static>(source: Rc<P>) -> Rc<dyn PageSource> {
    Rc::new(ErasedSource(source))
}

/// Resolves `OpenRead`/`OpenWrite` root pages to the adapters above.
pub struct StorageFactory {
    source: Rc<dyn PageSource>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
}

impl StorageFactory {
    /// Cursors over `source` only; `OpenWrite` is refused.
    pub fn read_only<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        header: DatabaseHeader,
    ) -> Self {
        StorageFactory {
            source: erase(source),
            writer: None,
            header,
        }
    }

    /// Cursors that read through `source` and write through `pager` (the
    /// same `Rc<RefCell<Pager>>` unsized into `source`, ADR-0017).
    pub fn writable<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        pager: SharedPager,
        header: DatabaseHeader,
    ) -> Self {
        StorageFactory {
            source: erase(source),
            writer: Some(pager),
            header,
        }
    }
}

impl CursorFactory for StorageFactory {
    fn open_read(&mut self, root: u32) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        Ok(Box::new(TableCursorAdapter::new(
            Rc::clone(&self.source),
            self.writer.clone(),
            self.header,
            root,
        )))
    }

    fn open_write(&mut self, root: u32) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        if self.writer.is_none() {
            return Err(CursorFactoryError(
                "OpenWrite: this connection is read-only (no writable pager)".to_string(),
            ));
        }
        self.open_read(root)
    }

    fn open_index(
        &mut self,
        root: u32,
        _key: &[SortKeyColumn],
    ) -> Result<Box<dyn Cursor>, CursorFactoryError> {
        Ok(Box::new(IndexCursorAdapter::new(
            Rc::clone(&self.source),
            self.writer.clone(),
            self.header,
            root,
        )))
    }
}

// ----------------------------------------------------------- transaction

/// `Transaction`/`AutoCommit`/`SetJournalMode`/`Synchronous`/
/// `IntegrityCheck` against the pager (former `control.rs`/`pragma.rs`).
pub struct PagerTransaction {
    source: Rc<dyn PageSource>,
    writer: Option<SharedPager>,
    header: DatabaseHeader,
}

impl PagerTransaction {
    /// A read-only connection: BEGIN/COMMIT only toggle state; integrity
    /// check still reads `source`.
    pub fn read_only<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        header: DatabaseHeader,
    ) -> Self {
        PagerTransaction {
            source: erase(source),
            writer: None,
            header,
        }
    }

    /// A writable connection over `pager`.
    pub fn writable<P: PageSource + ?Sized + 'static>(
        source: Rc<P>,
        pager: SharedPager,
        header: DatabaseHeader,
    ) -> Self {
        PagerTransaction {
            source: erase(source),
            writer: Some(pager),
            header,
        }
    }
}

impl Transaction for PagerTransaction {
    fn begin(&mut self, mode: i32) -> Result<(), TransactionError> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(());
        };
        let mut pager = writer.borrow_mut();
        match mode {
            db_core::vm::row::TRANSACTION_MODE_IMMEDIATE => pager.begin_immediate(),
            db_core::vm::row::TRANSACTION_MODE_EXCLUSIVE => pager.begin_exclusive(),
            _ => Ok(()),
        }
        .map_err(|e| TransactionError(storage_err(e)))
    }

    fn commit(&mut self) -> Result<(), TransactionError> {
        match self.writer.as_ref() {
            Some(writer) => writer
                .borrow_mut()
                .flush()
                .map_err(|e| TransactionError(storage_err(e))),
            None => Ok(()),
        }
    }

    fn rollback(&mut self) -> Result<(), TransactionError> {
        match self.writer.as_ref() {
            Some(writer) => writer
                .borrow_mut()
                .rollback()
                .map_err(|e| TransactionError(storage_err(e))),
            None => Ok(()),
        }
    }

    fn set_journal_mode(&mut self, mode: i32) -> Result<(), TransactionError> {
        let Some(writer) = self.writer.as_ref() else {
            return Ok(());
        };
        let mode = if mode == JOURNAL_MODE_WAL {
            JournalMode::Wal
        } else {
            JournalMode::Legacy
        };
        writer
            .borrow_mut()
            .set_journal_mode(mode)
            .map_err(|e| TransactionError(storage_err(e)))
    }

    fn synchronous(&self) -> Option<i32> {
        let writer = self.writer.as_ref()?;
        Some(writer.borrow().synchronous() as i32)
    }

    fn set_synchronous(&mut self, level: i32) -> Result<(), TransactionError> {
        if let Some(writer) = self.writer.as_ref() {
            let mode = match level {
                SYNCHRONOUS_OFF => SynchronousMode::Off,
                SYNCHRONOUS_NORMAL => SynchronousMode::Normal,
                _ => SynchronousMode::Full,
            };
            writer.borrow_mut().set_synchronous(mode);
        }
        Ok(())
    }

    fn integrity_check(&mut self, quick: bool) -> Option<Result<Vec<String>, TransactionError>> {
        Some(Ok(crate::integrity::run_integrity_check(
            Rc::clone(&self.source),
            &self.header,
            quick,
        )))
    }
}

// --------------------------------------------------------- schema writes

/// DDL and `ANALYZE` against `sqlite_master`/`sqlite_stat1`/
/// `sqlite_sequence` (former `cursor.rs::{create_table, …, analyze}`).
pub struct BtreeSchemaStorage {
    pager: SharedPager,
    header: DatabaseHeader,
}

impl BtreeSchemaStorage {
    /// Schema writes through `pager`.
    pub fn new(pager: SharedPager, header: DatabaseHeader) -> Self {
        BtreeSchemaStorage { pager, header }
    }
}

fn schema_err(e: impl std::fmt::Display) -> SchemaStorageError {
    SchemaStorageError(e.to_string())
}

/// `ANALYZE`'s index statistic: `(entries, avg_eq)` where `avg_eq` is the
/// average number of entries sharing a leading-column value.
fn count_index_entries_and_avg_eq(
    pager: &crate::pager::Pager,
    header: &DatabaseHeader,
    root_page: u32,
) -> Result<(u64, u64), SchemaStorageError> {
    let mut cursor = IndexCursor::new(pager, header.usable_page_size(), root_page);
    let mut total = 0u64;
    let mut distinct_groups = 0u64;
    let mut prev_leading: Option<Value> = None;
    let mut row = cursor.first().map_err(schema_err)?;
    while let Some(r) = row {
        let values = decode_record(&r.payload, header.text_encoding).map_err(schema_err)?;
        let leading = values.first().cloned();
        if prev_leading.as_ref() != leading.as_ref() {
            distinct_groups = distinct_groups.saturating_add(1);
            prev_leading = leading;
        }
        total = total.saturating_add(1);
        row = cursor.next().map_err(schema_err)?;
    }
    Ok((total, total.checked_div(distinct_groups).unwrap_or(0)))
}

impl SchemaStorage for BtreeSchemaStorage {
    fn create_table_root(&mut self) -> Result<u32, SchemaStorageError> {
        btree::create_empty_table_root(&mut self.pager.borrow_mut()).map_err(schema_err)
    }

    fn create_index_root(&mut self) -> Result<u32, SchemaStorageError> {
        btree::create_empty_index_root(&mut self.pager.borrow_mut()).map_err(schema_err)
    }

    fn populate_index(
        &mut self,
        index_root: u32,
        table_root: u32,
        column_indices: &[usize],
    ) -> Result<(), SchemaStorageError> {
        btree::populate_index_from_table(
            &mut self.pager.borrow_mut(),
            &self.header,
            table_root,
            index_root,
            column_indices,
        )
        .map_err(schema_err)
    }

    fn free_root(&mut self, root: u32) -> Result<(), SchemaStorageError> {
        btree::free_btree_pages(&mut self.pager.borrow_mut(), &self.header, root)
            .map_err(schema_err)
    }

    fn insert_master_row(
        &mut self,
        kind: &str,
        name: &str,
        tbl_name: &str,
        root_page: u32,
        sql: &str,
    ) -> Result<(), SchemaStorageError> {
        btree::insert_master_row(
            &mut self.pager.borrow_mut(),
            &self.header,
            &btree::MasterEntry {
                kind: kind.to_string(),
                name: name.to_string(),
                tbl_name: tbl_name.to_string(),
                rootpage: root_page,
                sql: sql.to_string(),
            },
        )
        .map_err(schema_err)
    }

    /// The `sqlite_master` row of an implicit `PRIMARY KEY`/`UNIQUE` index,
    /// with a NULL `sql` as sqlite3 writes it (db-core#694).
    fn insert_autoindex_row(
        &mut self,
        name: &str,
        tbl_name: &str,
        root_page: u32,
    ) -> Result<(), SchemaStorageError> {
        btree::insert_autoindex_master_row(
            &mut self.pager.borrow_mut(),
            &self.header,
            &btree::MasterEntry {
                kind: "index".to_string(),
                name: name.to_string(),
                tbl_name: tbl_name.to_string(),
                rootpage: root_page,
                sql: String::new(),
            },
        )
        .map_err(schema_err)
    }

    fn delete_master_row(&mut self, kind: &str, name: &str) -> Result<(), SchemaStorageError> {
        btree::delete_master_row(&mut self.pager.borrow_mut(), &self.header, kind, name)
            .map_err(schema_err)
    }

    fn bump_schema_cookie(&mut self) -> Result<(), SchemaStorageError> {
        btree::bump_schema_cookie(&mut self.pager.borrow_mut())
            .map(|_| ())
            .map_err(schema_err)
    }

    fn write_stat1(&mut self, target: &AnalyzeTarget) -> Result<(), SchemaStorageError> {
        let header = self.header;
        let mut pager = self.pager.borrow_mut();
        let stat1_root =
            btree::ensure_sqlite_stat1_table(&mut pager, &header).map_err(schema_err)?;
        btree::delete_stat1_rows_for_table(&mut pager, &header, stat1_root, &target.table_name)
            .map_err(schema_err)?;
        let row_count =
            btree::count_table_rows(&*pager, target.table_root_page).map_err(schema_err)?;
        btree::insert_stat1_row(
            &mut pager,
            &header,
            stat1_root,
            &target.table_name,
            None,
            &row_count.to_string(),
        )
        .map_err(schema_err)?;
        for index in &target.indexes {
            let (idx_rows, avg_eq) =
                count_index_entries_and_avg_eq(&pager, &header, index.root_page)?;
            btree::insert_stat1_row(
                &mut pager,
                &header,
                stat1_root,
                &target.table_name,
                Some(&index.index_name),
                &format!("{idx_rows} {avg_eq}"),
            )
            .map_err(schema_err)?;
        }
        Ok(())
    }

    fn autoincrement_rowid(
        &mut self,
        table: &str,
        max_from_table: i64,
    ) -> Result<i64, SchemaStorageError> {
        let header = self.header;
        let mut pager = self.pager.borrow_mut();
        let seq_root =
            btree::ensure_sqlite_sequence_table(&mut pager, &header).map_err(schema_err)?;
        let mut tracked_seq = 0i64;
        {
            let mut seq_cursor = TableCursor::new(&*pager, &header, seq_root);
            let mut row = seq_cursor.first_row().map_err(schema_err)?;
            while let Some(r) = row {
                let values = decode_record(&r.payload, header.text_encoding).map_err(schema_err)?;
                if let (Some(Value::Text(n)), Some(Value::Integer(seq))) =
                    (values.first(), values.get(1))
                {
                    if &**n == table {
                        tracked_seq = *seq;
                        break;
                    }
                }
                row = seq_cursor.next_row().map_err(schema_err)?;
            }
        }
        let candidate = max_from_table.max(tracked_seq).saturating_add(1);
        btree::update_sequence(&mut pager, &header, table, candidate).map_err(schema_err)?;
        Ok(candidate)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// `OpenWrite` on a read-only connection (`execute_with_db`) is refused
    /// by the factory — the former `Vm::writer("OpenWrite")` check.
    #[test]
    fn open_write_on_a_read_only_connection_is_refused() {
        let (vfs, header) = crate::btree::test_minimal_db(512);
        let source: Rc<dyn PageSource> = Rc::new(
            crate::vfs::VfsPageSource::open(&vfs, std::path::Path::new("/test.db"), 512).unwrap(),
        );
        let mut factory = StorageFactory::read_only(source, header);
        assert!(factory.open_read(1).is_ok());
        assert!(factory.open_write(1).is_err());
    }

    /// An index cursor over `CREATE INDEX ix ON ixt(a, b)` (#113): rows
    /// `(1,'x')`, `(1,'y')`, `(2,NULL)`, whose entries are `(a, b, rowid)`.
    fn index_cursor_over_a_b() -> IndexCursorAdapter {
        let (vfs, header) = crate::btree::test_minimal_db(4096);
        let pager =
            crate::pager::Pager::open(&vfs, std::path::Path::new("/test.db"), 4096).unwrap();
        let pager = Rc::new(RefCell::new(pager));
        let mut autocommit = true;
        for sql in [
            "CREATE TABLE ixt(a, b, c)",
            "CREATE INDEX ix ON ixt(a, b)",
            "INSERT INTO ixt VALUES (1, 'x', 10), (1, 'y', 20), (2, NULL, 30)",
        ] {
            let (schemas, views) = schema_of(&pager, &header);
            let program = crate::codegen::compile_statement(sql, &schemas, &views).unwrap();
            autocommit = crate::vdbe::execute_transaction_step(
                &program,
                Rc::clone(&pager),
                header,
                autocommit,
            )
            .unwrap()
            .1;
        }
        let (schemas, _) = schema_of(&pager, &header);
        let root = schemas
            .iter()
            .find(|t| t.name == "ixt")
            .unwrap()
            .indexes
            .first()
            .unwrap()
            .root_page;
        IndexCursorAdapter::new(erase(pager), None, header, root)
    }

    fn schema_of(
        pager: &Rc<RefCell<crate::pager::Pager>>,
        header: &DatabaseHeader,
    ) -> (
        Vec<crate::schema::TableSchema>,
        Vec<crate::schema::ViewSchema>,
    ) {
        let borrowed = pager.borrow();
        let mut cursor = TableCursor::new(&*borrowed, header, 1);
        let schemas = crate::schema::read_schema(&mut cursor, header.text_encoding).unwrap();
        let mut cursor = TableCursor::new(&*borrowed, header, 1);
        let views = crate::schema::read_views(&mut cursor, header.text_encoding).unwrap();
        (schemas, views)
    }

    /// Positioning keeps the record encoded; every reader decodes only
    /// what it asks for, and agrees with the old whole-record decode.
    #[test]
    fn an_index_entry_is_decoded_column_by_column() {
        let mut c = index_cursor_over_a_b();
        assert!(c.column(0).unwrap().is_none(), "nothing positioned yet");
        assert_eq!(c.idx_rowid(), None);
        assert!(c.idx_compare(&[Value::Integer(1)], &[]).is_none());

        assert!(c.rewind().unwrap());
        assert_eq!(c.column(0).unwrap(), Some(Value::Integer(1)));
        assert_eq!(c.column(1).unwrap(), Some(Value::Text("x".into())));
        assert_eq!(c.column(2).unwrap(), Some(Value::Integer(1)), "the rowid");
        assert_eq!(c.column(3).unwrap(), Some(Value::Null), "past the end");
        assert_eq!(c.idx_rowid(), Some(1));
        assert_eq!(c.rowid(), Some(1));

        // Only the probe's leading columns are compared.
        let probe = |key: &[Value]| c.idx_compare(key, &[]);
        assert_eq!(probe(&[Value::Integer(1)]), Some(Ordering::Equal));
        assert_eq!(probe(&[Value::Integer(2)]), Some(Ordering::Less));
        assert_eq!(probe(&[Value::Integer(0)]), Some(Ordering::Greater));
        assert_eq!(
            probe(&[Value::Integer(1), Value::Text("y".into())]),
            Some(Ordering::Less)
        );
        // A probe as long as the entry leaves no trailing rowid: "not greater".
        let full = [
            Value::Integer(1),
            Value::Text("x".into()),
            Value::Integer(1),
        ];
        assert_eq!(probe(&full), Some(Ordering::Equal));
        assert_eq!(c.compare_leading(&full, &[]), None);

        assert!(c.next().unwrap() && c.next().unwrap());
        assert_eq!(c.column(1).unwrap(), Some(Value::Null));
        assert_eq!(c.idx_rowid(), Some(3));
        assert!(!c.next().unwrap());
        assert!(c.column(0).unwrap().is_none());
        assert_eq!(c.idx_rowid(), None);

        assert!(c.last().unwrap());
        assert_eq!(c.idx_rowid(), Some(3));
        assert!(c.prev().unwrap());
        assert_eq!(c.idx_rowid(), Some(2));
    }

    #[test]
    fn seeking_an_index_entry_compares_the_key_under_its_collations() {
        let mut c = index_cursor_over_a_b();
        let key = [Value::Integer(1), Value::Text("X".into())];
        assert!(!c.seek_index_eq(&key, &[]).unwrap(), "binary: 'X' != 'x'");
        assert!(c.column(0).unwrap().is_none(), "a miss leaves no entry");
        assert!(c
            .seek_index_eq(&key, &[Collation::Binary, Collation::NoCase])
            .unwrap());
        assert_eq!(c.idx_rowid(), Some(1));
        assert!(c.seek_index_eq(&[Value::Integer(2)], &[]).unwrap());
        assert_eq!(c.idx_rowid(), Some(3));
        assert!(!c.seek_index_eq(&[Value::Integer(5)], &[]).unwrap());
        assert!(c
            .seek_index_ge(&[Value::Integer(1), Value::Text("y".into())], &[])
            .unwrap());
        assert_eq!(c.idx_rowid(), Some(2));
    }

    /// `parse_header_checked_into` accepts what `decode_record` accepts
    /// and refuses a body of the wrong length, without decoding it, so a
    /// malformed index entry is still an error when the cursor positions.
    #[test]
    fn checked_header_parse_requires_the_body_to_fill_the_payload() {
        let mut entries = Vec::new();
        // One 1-byte integer (7), then one empty text column.
        parse_header_checked_into(&[3, 1, 13, 7], &mut entries).unwrap();
        assert_eq!(entries, [(1, 3), (13, 4)]);
        // The integer's byte is missing.
        assert!(matches!(
            parse_header_checked_into(&[2, 1], &mut entries),
            Err(RecordError::UnexpectedEof { .. })
        ));
        // One byte too many.
        assert!(matches!(
            parse_header_checked_into(&[2, 1, 7, 7], &mut entries),
            Err(RecordError::TrailingData { trailing: 1 })
        ));
        // A record with no columns is just its header byte.
        parse_header_checked_into(&[1], &mut entries).unwrap();
        assert!(entries.is_empty());
        assert!(matches!(
            parse_header_checked_into(&[1, 0], &mut entries),
            Err(RecordError::TrailingData { trailing: 1 })
        ));
        // A broken header is refused by the parse itself.
        assert!(parse_header_checked_into(&[], &mut entries).is_err());
    }

    /// `serial_type_len` agrees with how far `decode_serial_value` reads,
    /// for every fixed-size type and for blobs and text of a few lengths.
    #[test]
    fn serial_type_len_matches_the_decoder() {
        let body = [0u8; 64];
        for serial_type in (0..=9).chain(12..=40) {
            let (_, next) =
                decode_serial_value(serial_type, &body, 0, crate::record::TextEncoding::Utf8)
                    .unwrap();
            assert_eq!(
                serial_type_len(serial_type),
                next,
                "serial type {serial_type}"
            );
        }
    }
}
