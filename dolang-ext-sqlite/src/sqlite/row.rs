use std::{ffi::CStr, os::raw::c_char};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::{Mut, Ref, Spread, SpreadContext, TypeBuilder, Unpack, UnpackItem},
    value::Nil,
    value::TypeObject,
};
use libsqlite3_sys::{
    SQLITE_BLOB, SQLITE_FLOAT, SQLITE_INTEGER, SQLITE_NULL, SQLITE_ROW, SQLITE_TEXT,
    sqlite3_column_blob, sqlite3_column_bytes, sqlite3_column_count, sqlite3_column_double,
    sqlite3_column_int64, sqlite3_column_name, sqlite3_column_text, sqlite3_column_type,
    sqlite3_step, sqlite3_stmt,
};

use crate::global::Global;

use super::{
    AssertSend, Epoch,
    statement::{QueryState, StatementAnnex},
};

pub(crate) struct Rows;

pub(crate) struct RowsAnnex<'v> {
    pub(super) global: State<'v, Global<'v>>,
    pub(super) epoch: Epoch,
}

impl<'v> Object<'v> for Rows {
    const NAME: &'v str = "Rows";
    const MODULE: &'v str = "sqlite";
    const SLOTS: usize = 1;
    type Annex = RowsAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if unpack.required_keys() != 0 || unpack.required() != 0 {
            return Err(Error::not_supported(strand));
        }
        // Turn on owned flag to prevent row invalidation
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let stmt = annex
            .global
            .types
            .statement
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();
        if let Ok(mut stmt_borrow) = stmt.borrow_mut(strand)
            && let QueryState::Active { ref mut owned, .. } = stmt_borrow.query
        {
            *owned = true;
        }
        drop(borrow);
        for item in unpack.iter() {
            match item {
                UnpackItem::Pos { mut slot, default } => {
                    if !Self::next(this, strand, Slot::reborrow(&mut slot)).await? {
                        Output::set(strand, slot, default.unwrap())
                    }
                }
                UnpackItem::SymKey { slot, default, .. }
                | UnpackItem::ConstKey { slot, default, .. } => {
                    Output::set(strand, slot, default.unwrap())
                }
                UnpackItem::Rest { slot } => Output::set(strand, slot, this),
            }
        }
        Ok(())
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        if context != SpreadContext::Sequence {
            return Err(Error::not_supported(strand));
        }

        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let stmt = annex
            .global
            .types
            .statement
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();
        if let Ok(mut stmt_borrow) = stmt.borrow_mut(strand)
            && let QueryState::Active { ref mut owned, .. } = stmt_borrow.query
        {
            *owned = true;
        }
        drop(borrow);

        strand
            .with_slots(async move |strand, [mut item]| {
                while Self::next(this, strand, Slot::reborrow(&mut item)).await? {
                    sink.positional(strand, Slot::reborrow(&mut item))?;
                }
                Ok(())
            })
            .await
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let stmt = annex
            .global
            .types
            .statement
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();

        // Validate epoch
        if stmt.annex().epoch.get() != annex.epoch {
            return Err(Error::concurrency_msg(
                strand,
                "iterator invalidated by statement reuse",
            ));
        }

        strand
            .with_slots(async move |strand, [mut conn]| {
                let stmt_borrow = stmt.borrow(strand)?;
                let owned = matches!(&stmt_borrow.query, QueryState::Active { owned: true, .. });

                Output::set(strand, &mut conn, Ref::slot::<0>(&stmt_borrow));
                drop(stmt_borrow);
                let conn = annex.global.types.connection.downcast(&conn).unwrap();

                let raw = stmt.annex().raw.get();
                if raw.is_null() {
                    return Err(Error::state_error(strand, "statement closed"));
                }

                let row_epoch = stmt.annex().bump_row_epoch();

                // Step to next row
                let raw = AssertSend(raw);
                let has_row = conn
                    .annex()
                    .busy_retry(strand, async move |strand| {
                        conn.annex()
                            .with_raw(strand, move |_| {
                                let rc = unsafe { sqlite3_step(raw.into_inner()) };
                                Ok(rc == SQLITE_ROW)
                            })
                            .await
                    })
                    .await?;

                if has_row {
                    let data = if owned {
                        // Copy row data
                        let bool_columns = &stmt.annex().bool_columns;
                        let values: Vec<_> = unsafe {
                            let count = sqlite3_column_count(raw.into_inner());
                            (0..count)
                                .map(|i| column_to_value(raw.into_inner(), i, bool_columns))
                                .collect()
                        };
                        RowData::Owned(values.into_boxed_slice())
                    } else {
                        RowData::Ref(row_epoch)
                    };

                    strand
                        .with_slots(async |strand, [mut wrapper]| {
                            annex.global.types.row.create_with_annex(
                                strand,
                                Row,
                                RowAnnex {
                                    global: annex.global,
                                    epoch: annex.epoch,
                                    data,
                                },
                                &mut wrapper,
                            );

                            let mut row = annex
                                .global
                                .types
                                .row
                                .downcast(&wrapper)
                                .unwrap()
                                .borrow_mut_unwrap();
                            Output::set(strand, Mut::slot_mut::<0>(&mut row), stmt);
                            drop(row);

                            Output::set(strand, out, wrapper);
                            Ok(true)
                        })
                        .await
                } else {
                    stmt.borrow_mut(strand)?.query = QueryState::None;
                    Ok(false)
                }
            })
            .await
    }
}

pub(crate) struct Row;

enum RowData {
    Ref(Epoch),
    Owned(Box<[SqliteValue]>),
}

pub(crate) struct RowAnnex<'v> {
    global: State<'v, Global<'v>>,
    epoch: Epoch,
    data: RowData,
}

/// Helper function to unpack row columns
///
/// # Safety
///
/// The raw pointer must be a valid sqlite3_stmt pointer.
unsafe fn unpack_row<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    annex: &RowAnnex<'v>,
    stmt_annex: &StatementAnnex<'v>,
    raw: *mut sqlite3_stmt,
    unpack: &mut Unpack<'v, 'a>,
    consumed: &mut [bool],
) -> Result<'v, 's, Option<Slot<'v, 'a>>> {
    unsafe {
        let count = sqlite3_column_count(raw) as usize;
        let mut rest_slot = None;
        let exhaustive = unpack.exhaustive();

        'top: for item in unpack.iter() {
            match item {
                UnpackItem::Pos { mut slot, default } => {
                    // Find next unconsumed column
                    for (i, con) in consumed.iter_mut().enumerate() {
                        if !*con {
                            *con = true;
                            let found = !get(
                                strand,
                                annex,
                                stmt_annex,
                                raw,
                                i as i32,
                                Slot::reborrow(&mut slot),
                            )?;
                            debug_assert!(found);
                            continue 'top;
                        }
                    }
                    // No more columns available
                    let input = default.ok_or_else(|| Error::missing_positional(strand, count))?;
                    Output::set(strand, slot, input);
                }
                UnpackItem::SymKey {
                    key,
                    mut slot,
                    default,
                } => {
                    let name = key.as_str(strand);
                    let idx = column_for_name(raw, name);
                    if idx < 0 || consumed[idx as usize] {
                        // Column not found or already consumed
                        let input = default.ok_or_else(|| Error::missing_key(strand, key))?;
                        Output::set(strand, slot, input);
                    } else {
                        consumed[idx as usize] = true;
                        let found = get(
                            strand,
                            annex,
                            stmt_annex,
                            raw,
                            idx,
                            Slot::reborrow(&mut slot),
                        )?;
                        debug_assert!(found);
                    }
                }
                UnpackItem::ConstKey {
                    key,
                    mut slot,
                    default,
                } => {
                    let idx = if let Some(i) = key.as_i64(strand) {
                        i.try_into().map_err(|_| Error::overflow(strand))?
                    } else if let Some(name) = key.as_str(strand) {
                        column_for_name(raw, name)
                    } else {
                        return Err(Error::type_error(
                            strand,
                            "expected int or str for column key",
                        ));
                    };

                    if idx < 0 || consumed[idx as usize] {
                        let input = default.ok_or_else(|| Error::missing_key(strand, key))?;
                        Output::set(strand, slot, input);
                    } else {
                        consumed[idx as usize] = true;
                        let found = !get(
                            strand,
                            annex,
                            stmt_annex,
                            raw,
                            idx,
                            Slot::reborrow(&mut slot),
                        )?;
                        debug_assert!(found);
                    }
                }
                UnpackItem::Rest { slot } => {
                    rest_slot = Some(slot);
                }
            }
        }

        // Check for exhaustive unpack
        if exhaustive {
            for (i, con) in consumed.iter().enumerate() {
                if !*con {
                    // Find column name for error
                    let col_name_ptr = sqlite3_column_name(raw, i as i32);
                    if !col_name_ptr.is_null() {
                        let col_name = CStr::from_ptr(col_name_ptr).to_string_lossy();
                        return Err(Error::unexpected_key(strand, col_name.as_ref()));
                    } else {
                        return Err(Error::unexpected_positional(strand, i));
                    }
                }
            }
        }
        Ok(rest_slot)
    }
}

impl<'v> Object<'v> for Row {
    const NAME: &'v str = "Row";
    const MODULE: &'v str = "sqlite";
    const SLOTS: usize = 1;
    type Annex = RowAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let stmt = annex
            .global
            .types
            .statement
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();

        let stmt_annex = stmt.annex();
        if stmt_annex.epoch.get() != annex.epoch {
            return Err(Error::concurrency_msg(
                strand,
                "iterator invalidated by statement reuse",
            ));
        }

        let raw = stmt_annex.raw.get();
        if raw.is_null() {
            return Err(Error::state_error(strand, "statement closed"));
        }

        let count = unsafe { sqlite3_column_count(raw) } as usize;
        let mut consumed = vec![false; count];

        unsafe {
            let rest_slot = unpack_row(strand, annex, stmt_annex, raw, &mut unpack, &mut consumed)?;

            if let Some(slot) = rest_slot {
                // Create RowIter with remaining columns
                strand
                    .with_slots(async |strand, [mut wrapper]| {
                        annex.global.types.row_iter.create_with_annex(
                            strand,
                            RowIter {
                                consumed,
                                current: 0,
                            },
                            RowIterAnnex {
                                global: annex.global,
                            },
                            &mut wrapper,
                        );

                        // Store reference to Row in slot 0
                        Output::set(
                            strand,
                            Mut::slot_mut::<0>(
                                &mut annex
                                    .global
                                    .types
                                    .row_iter
                                    .downcast(&wrapper)
                                    .unwrap()
                                    .borrow_mut_unwrap(),
                            ),
                            this,
                        );

                        Output::set(strand, slot, wrapper);
                        Ok(())
                    })
                    .await?;
            }
        }
        Ok(())
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let stmt = annex
            .global
            .types
            .statement
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();

        let stmt_annex = stmt.annex();
        if stmt_annex.epoch.get() != annex.epoch {
            return Err(Error::concurrency_msg(
                strand,
                "iterator invalidated by statement reuse",
            ));
        }

        let raw = stmt_annex.raw.get();
        if raw.is_null() {
            return Err(Error::state_error(strand, "statement closed"));
        }

        unsafe {
            let idx = if let Some(i) = index.as_i64(strand) {
                i as i32
            } else if let Some(name) = index.as_str(strand) {
                column_for_name(raw, name)
            } else {
                return Err(Error::type_error(
                    strand,
                    "expected int or str for column key",
                ));
            };

            if idx < 0 {
                return Err(Error::index(strand));
            }

            if !get(strand, annex, stmt_annex, raw, idx, out)? {
                return Err(Error::index(strand));
            }
        }
        Ok(())
    }
}

// RowIter implementation
pub(crate) struct RowIter {
    consumed: Vec<bool>,
    current: usize,
}

pub(crate) struct RowIterAnnex<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for RowIter {
    const NAME: &'v str = "RowIter";
    const MODULE: &'v str = "sqlite";
    const SLOTS: usize = 1;
    type Annex = RowIterAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand
            .with_slots(async move |strand, [mut row]| {
                let annex = this.annex();
                let mut borrow = this.borrow_mut(strand)?;
                Output::set(strand, &mut row, Mut::slot::<0>(&borrow));
                let row = annex.global.types.row.downcast(&row).unwrap();

                let row_annex = row.annex();
                let row_borrow = row.borrow(strand)?;
                let stmt = row_annex
                    .global
                    .types
                    .statement
                    .downcast(Ref::slot::<0>(&row_borrow))
                    .unwrap();

                let stmt_annex = stmt.annex();
                if stmt_annex.epoch.get() != row_annex.epoch {
                    return Err(Error::concurrency_msg(
                        strand,
                        "iterator invalidated by statement reuse",
                    ));
                }

                let raw = stmt_annex.raw.get();
                if raw.is_null() {
                    return Err(Error::state_error(strand, "statement closed"));
                }

                unsafe {
                    let rest_slot = unpack_row(
                        strand,
                        row_annex,
                        stmt_annex,
                        raw,
                        &mut unpack,
                        &mut borrow.consumed,
                    )?;

                    if let Some(slot) = rest_slot {
                        // For Rest in RowIter, just set self again
                        Output::set(strand, slot, this);
                    }
                }
                Ok(())
            })
            .await
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        strand
            .with_slots(async move |strand, [mut row]| {
                let annex = this.annex();
                let mut borrow = this.borrow_mut(strand)?;
                Output::set(strand, &mut row, Mut::slot::<0>(&borrow));
                let row = annex.global.types.row.downcast(&row).unwrap();

                let row_annex = row.annex();
                let row_borrow = row.borrow(strand)?;
                let stmt = row_annex
                    .global
                    .types
                    .statement
                    .downcast(Ref::slot::<0>(&row_borrow))
                    .unwrap();

                let stmt_annex = stmt.annex();
                if stmt_annex.epoch.get() != row_annex.epoch {
                    return Err(Error::concurrency_msg(
                        strand,
                        "iterator invalidated by statement reuse",
                    ));
                }

                let raw = stmt_annex.raw.get();
                if raw.is_null() {
                    return Err(Error::state_error(strand, "statement closed"));
                }

                // Find next unconsumed column
                let current = borrow.current;
                for (i, con) in borrow.consumed[current..].iter_mut().enumerate() {
                    if !*con {
                        unsafe {
                            // Get the column value
                            let found = get(
                                strand,
                                row_annex,
                                stmt_annex,
                                raw,
                                (current + i) as i32,
                                Slot::reborrow(&mut out),
                            )?;
                            debug_assert!(found);
                            *con = true;
                            borrow.current = current + i + 1;
                            return Ok(true);
                        }
                    }
                }
                borrow.current = borrow.consumed.len();

                Ok(false)
            })
            .await
    }
}

unsafe fn column_for_name(raw: *mut sqlite3_stmt, name: &str) -> i32 {
    unsafe {
        let count = sqlite3_column_count(raw);
        let mut found_idx = -1;
        for i in 0..count {
            let col_name_ptr = sqlite3_column_name(raw, i);
            if !col_name_ptr.is_null() {
                let col_name = CStr::from_ptr(col_name_ptr).to_string_lossy();
                if col_name == name {
                    found_idx = i;
                    break;
                }
            }
        }
        found_idx
    }
}

unsafe fn get<'v, 's>(
    strand: &mut Strand<'v, 's>,
    annex: &RowAnnex<'v>,
    stmt_annex: &StatementAnnex<'v>,
    raw: *mut sqlite3_stmt,
    idx: i32,
    out: Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    match &annex.data {
        RowData::Ref(row_epoch) => {
            if *row_epoch != stmt_annex.row_epoch.get() {
                return Err(Error::concurrency_msg(
                    strand,
                    "row data invalidated by iterator advancing",
                ));
            }
            unsafe {
                match sqlite3_column_type(raw, idx) {
                    SQLITE_NULL => Output::set(strand, out, Nil),
                    SQLITE_INTEGER => {
                        let val = sqlite3_column_int64(raw, idx);
                        if stmt_annex
                            .bool_columns
                            .get(idx as usize)
                            .copied()
                            .unwrap_or(false)
                        {
                            Output::set(strand, out, val != 0);
                        } else {
                            Output::set(strand, out, val);
                        }
                    }
                    SQLITE_FLOAT => Output::set(strand, out, sqlite3_column_double(raw, idx)),
                    SQLITE_TEXT => {
                        let ptr = sqlite3_column_text(raw, idx) as *const c_char;
                        let len = sqlite3_column_bytes(raw, idx);
                        let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize);
                        Output::set(strand, out, String::from_utf8_lossy(bytes).as_ref())
                    }
                    SQLITE_BLOB => {
                        let ptr = sqlite3_column_blob(raw, idx) as *const u8;
                        let len = sqlite3_column_bytes(raw, idx);
                        let bytes = std::slice::from_raw_parts(ptr, len as usize);
                        Output::set(strand, out, bytes);
                    }
                    _ => return Err(Error::runtime(strand, "unsupported sqlite type")),
                }
            };
            Ok(true)
        }
        RowData::Owned(values) => {
            if let Some(value) = values.get(idx as usize) {
                match value {
                    SqliteValue::Null => Output::set(strand, out, Nil),
                    SqliteValue::Bool(b) => Output::set(strand, out, *b),
                    SqliteValue::Integer(i) => Output::set(strand, out, *i),
                    SqliteValue::Real(f) => Output::set(strand, out, *f),
                    SqliteValue::Text(s) => Output::set(strand, out, s.as_str()),
                    SqliteValue::Blob(b) => Output::set(strand, out, b.as_slice()),
                };
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

#[derive(Clone)]
enum SqliteValue {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

unsafe fn column_to_value(raw: *mut sqlite3_stmt, idx: i32, bool_columns: &[bool]) -> SqliteValue {
    unsafe {
        match sqlite3_column_type(raw, idx) {
            SQLITE_NULL => SqliteValue::Null,
            SQLITE_INTEGER => {
                let val = sqlite3_column_int64(raw, idx);
                if bool_columns.get(idx as usize).copied().unwrap_or(false) {
                    SqliteValue::Bool(val != 0)
                } else {
                    SqliteValue::Integer(val)
                }
            }
            SQLITE_FLOAT => SqliteValue::Real(sqlite3_column_double(raw, idx)),
            SQLITE_TEXT => {
                let ptr = sqlite3_column_text(raw, idx) as *const c_char;
                let len = sqlite3_column_bytes(raw, idx);
                let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize);
                SqliteValue::Text(String::from_utf8_lossy(bytes).into_owned())
            }
            SQLITE_BLOB => {
                let ptr = sqlite3_column_blob(raw, idx) as *const u8;
                let len = sqlite3_column_bytes(raw, idx);
                let bytes = std::slice::from_raw_parts(ptr, len as usize);
                SqliteValue::Blob(bytes.to_vec())
            }
            _ => SqliteValue::Null,
        }
    }
}
