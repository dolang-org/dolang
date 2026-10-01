use std::ffi::CStr;

use bitvec::slice::BitSlice;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::{Mut, Ref, Rest, Spread, SpreadContext, TypeBuilder, Unpack, UnpackItem},
    value::Nil,
    value::TypeObject,
    value::{AsSym, AsTuple},
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

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let stmt = annex
            .global
            .types
            .statement
            .cast(Ref::slot::<0>(&borrow))
            .unwrap();
        stmt.enter_sync(strand, |strand, stmt| {
            if let Ok(mut stmt_borrow) = stmt.borrow_mut(strand)
                && let QueryState::Active { ref mut owned, .. } = stmt_borrow.query
            {
                *owned = true;
            }
        });
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
            .cast(Ref::slot::<0>(&borrow))
            .unwrap();

        // Validate epoch
        if stmt.enter_sync(strand, |_strand, stmt| {
            stmt.annex().epoch.get() != annex.epoch
        }) {
            return Err(Error::concurrency_msg(
                strand,
                "iterator invalidated by statement reuse",
            ));
        }

        stmt.enter(strand, async move |strand, stmt| {
            strand
                .with_slots(async move |strand, [mut conn]| {
                    let stmt_borrow = stmt.borrow(strand)?;
                    let owned =
                        matches!(&stmt_borrow.query, QueryState::Active { owned: true, .. });

                    Output::set(strand, &mut conn, Ref::slot::<0>(&stmt_borrow));
                    drop(stmt_borrow);
                    let conn = annex.global.types.connection.cast(&conn).unwrap();

                    let raw = stmt.annex().raw.get();
                    if raw.is_null() {
                        return Err(Error::state_error(strand, "statement closed"));
                    }

                    let row_epoch = stmt.annex().bump_row_epoch();

                    // Step to next row
                    let raw = AssertSend(raw);
                    let has_row = conn
                        .enter(strand, async move |strand, conn| {
                            conn.annex()
                                .busy_retry(strand, async move |strand| {
                                    conn.annex()
                                        .with_raw(strand, move |_| {
                                            let rc = unsafe { sqlite3_step(raw.into_inner()) };
                                            Ok(rc == SQLITE_ROW)
                                        })
                                        .await
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

                                annex.global.types.row.cast(&wrapper).unwrap().enter_sync(
                                    strand,
                                    |strand, row| {
                                        let mut row = row.borrow_mut_unwrap();
                                        Output::set(strand, Mut::slot_mut::<0>(&mut row), stmt);
                                    },
                                );

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

/// The columns a rest captured by [`unpack_row`] holds.
enum RestColumns {
    /// The leftover columns.
    Leftover { keyed: bool },
    /// No columns, keyed.
    Empty,
}

/// Helper function to unpack row columns
///
/// Columns are positional items unless `keyed`,
/// when they are keyed items named by their columns. A `**` rest alone takes
/// leftover positional columns as keyed ones.
///
/// Returns the rests to create over the leftover columns.
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
    keyed: bool,
) -> Result<'v, 's, Vec<(Slot<'v, 'a>, RestColumns)>> {
    unsafe {
        let count = sqlite3_column_count(raw) as usize;
        let mut rests = Vec::new();
        let pos_rest = unpack.pos_rest();
        let exhaustive = unpack.key_rest() == Rest::None && (keyed || pos_rest == Rest::None);

        'top: for item in unpack.iter() {
            match item {
                UnpackItem::Pos { slot, default } if keyed => {
                    let input = default.ok_or_else(|| Error::missing_positional(strand, 0))?;
                    Output::set(strand, slot, input);
                }
                UnpackItem::Pos { mut slot, default } => {
                    // Find next unconsumed column
                    for (i, con) in consumed.iter_mut().enumerate() {
                        if !*con {
                            *con = true;
                            let found = get(
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
                    if idx < 0 || idx as usize >= consumed.len() || consumed[idx as usize] {
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
                    let idx = if let Ok(i) = key.to_i64(strand) {
                        i.try_into().map_err(|_| Error::overflow(strand))?
                    } else if let Some(name) = key.as_str(strand) {
                        strand.access(|x| column_for_name(raw, name.as_str(x)))
                    } else {
                        return Err(Error::type_error(
                            strand,
                            "expected Int or Str for column key",
                        ));
                    };

                    if idx < 0 || idx as usize >= consumed.len() || consumed[idx as usize] {
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
                UnpackItem::Rest { slot } => rests.push((slot, RestColumns::Leftover { keyed })),
                UnpackItem::PosRest { slot } if keyed => Unpack::empty_pos_rest(strand, slot),
                UnpackItem::PosRest { slot } => {
                    rests.push((slot, RestColumns::Leftover { keyed: false }))
                }
                UnpackItem::KeyRest { slot } if keyed || pos_rest == Rest::None => {
                    rests.push((slot, RestColumns::Leftover { keyed: true }))
                }
                UnpackItem::KeyRest { slot } => rests.push((slot, RestColumns::Empty)),
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
        Ok(rests)
    }
}

/// Runs `f` against the statement behind `row`, once the row is known to be
/// current.
fn with_stmt<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    row: Instance<'v, '_, Row>,
    f: impl FnOnce(
        &mut Strand<'v, 's>,
        &RowAnnex<'v>,
        &StatementAnnex<'v>,
        *mut sqlite3_stmt,
    ) -> Result<'v, 's, R>,
) -> Result<'v, 's, R> {
    let annex = row.annex();
    let borrow = row.borrow(strand)?;
    let stmt = annex
        .global
        .types
        .statement
        .cast(Ref::slot::<0>(&borrow))
        .unwrap();
    stmt.enter_sync(strand, move |strand, stmt| {
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
        f(strand, &annex, &stmt_annex, raw)
    })
}

/// Unpacks the columns of `row` not in `consumed` (all of them if `None`),
/// creating a [`RowRest`] for each rest captured.
fn unpack_columns<'v, 's>(
    strand: &mut Strand<'v, 's>,
    row: Instance<'v, '_, Row>,
    consumed: Option<&[bool]>,
    keyed: bool,
    mut unpack: Unpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let (consumed, rests) = with_stmt(strand, row, |strand, annex, stmt_annex, raw| {
        let mut consumed = match consumed {
            Some(consumed) => consumed.to_vec(),
            None => vec![false; unsafe { sqlite3_column_count(raw) } as usize],
        };
        let rests = unsafe {
            unpack_row(
                strand,
                annex,
                stmt_annex,
                raw,
                &mut unpack,
                &mut consumed,
                keyed,
            )?
        };
        Ok((consumed, rests))
    })?;
    let global = row.annex().global;
    for (mut slot, rest) in rests {
        let (consumed, keyed) = match rest {
            RestColumns::Leftover { keyed } => (consumed.clone(), keyed),
            RestColumns::Empty => (vec![true; consumed.len()], true),
        };
        global.types.row_rest.create_with_annex(
            strand,
            RowRest,
            RowRestAnnex {
                global,
                consumed: consumed.into(),
                keyed,
            },
            Slot::reborrow(&mut slot),
        );
        let rest = global.types.row_rest.cast(&slot).unwrap();
        rest.enter_sync(strand, |strand, rest| {
            let mut rest = rest.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut rest), row);
        });
    }
    Ok(())
}

/// Spreads the columns of `row` not in `consumed` (all of them if `None`):
/// values positionally, or if `keyed`, name/value pairs in sequences and
/// keyed items elsewhere.
fn spread_columns<'v, 's>(
    strand: &mut Strand<'v, 's>,
    row: Instance<'v, '_, Row>,
    consumed: Option<&[bool]>,
    keyed: bool,
    context: SpreadContext,
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    with_stmt(strand, row, |strand, annex, stmt_annex, raw| {
        strand.with_slots_sync(|strand, [mut key, mut value, mut pair]| {
            let count = unsafe { sqlite3_column_count(raw) } as usize;
            for index in 0..count {
                if consumed.is_some_and(|consumed| consumed[index]) {
                    continue;
                }
                let found = unsafe {
                    get(
                        strand,
                        annex,
                        stmt_annex,
                        raw,
                        index as i32,
                        Slot::reborrow(&mut value),
                    )?
                };
                debug_assert!(found);
                if !keyed {
                    sink.positional(strand, Slot::reborrow(&mut value))?;
                    continue;
                }
                let name = unsafe { column_name(raw, index) };
                Output::set(strand, Slot::reborrow(&mut key), AsSym::new(&name));
                if context == SpreadContext::Sequence {
                    Output::set(
                        strand,
                        Slot::reborrow(&mut pair),
                        AsTuple::new([&key, &value]),
                    );
                    sink.positional(strand, Slot::reborrow(&mut pair))?;
                } else {
                    sink.keyed(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut value))?;
                }
            }
            Ok(())
        })
    })
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
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack_columns(strand, this, None, false, unpack)
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let keyed = context != SpreadContext::Sequence;
        spread_columns(strand, this, None, keyed, context, sink)
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_stmt(strand, this, |strand, annex, stmt_annex, raw| unsafe {
            let idx = if let Ok(i) = index.to_i64(strand) {
                i as i32
            } else if let Some(name) = index.as_str(strand) {
                strand.access(|x| column_for_name(raw, name.as_str(x)))
            } else {
                return Err(Error::type_error(
                    strand,
                    "expected Int or Str for column key",
                ));
            };

            if idx < 0 {
                return Err(Error::index(strand));
            }

            if !get(strand, annex, stmt_annex, raw, idx, out)? {
                return Err(Error::index(strand));
            }
            Ok(())
        })
    }
}

/// Returns the name of column `index`.
///
/// # Safety
///
/// The raw pointer must be a valid sqlite3_stmt pointer.
unsafe fn column_name(raw: *mut sqlite3_stmt, index: usize) -> String {
    unsafe {
        let ptr = sqlite3_column_name(raw, index as i32);
        if ptr.is_null() {
            index.to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
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
                            .is_some_and(|flag| *flag)
                        {
                            Output::set(strand, out, val != 0);
                        } else {
                            Output::set(strand, out, val);
                        }
                    }
                    SQLITE_FLOAT => Output::set(strand, out, sqlite3_column_double(raw, idx)),
                    SQLITE_TEXT => {
                        let ptr = sqlite3_column_text(raw, idx);
                        let len = sqlite3_column_bytes(raw, idx);
                        let bytes = std::slice::from_raw_parts(ptr, len as usize);
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

unsafe fn column_to_value(
    raw: *mut sqlite3_stmt,
    idx: i32,
    bool_columns: &BitSlice,
) -> SqliteValue {
    unsafe {
        match sqlite3_column_type(raw, idx) {
            SQLITE_NULL => SqliteValue::Null,
            SQLITE_INTEGER => {
                let val = sqlite3_column_int64(raw, idx);
                if bool_columns.get(idx as usize).is_some_and(|flag| *flag) {
                    SqliteValue::Bool(val != 0)
                } else {
                    SqliteValue::Integer(val)
                }
            }
            SQLITE_FLOAT => SqliteValue::Real(sqlite3_column_double(raw, idx)),
            SQLITE_TEXT => {
                let ptr = sqlite3_column_text(raw, idx);
                let len = sqlite3_column_bytes(raw, idx);
                let bytes = std::slice::from_raw_parts(ptr, len as usize);
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

/// The leftover columns of a row, captured by a rest.
pub(crate) struct RowRest;

pub(crate) struct RowRestAnnex<'v> {
    global: State<'v, Global<'v>>,
    consumed: Box<[bool]>,
    keyed: bool,
}

impl<'v> Object<'v> for RowRest {
    const NAME: &'v str = "RowRest";
    const MODULE: &'v str = "sqlite";
    // Slot 0: the row
    const SLOTS: usize = 1;
    type Annex = RowRestAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let row = annex
            .global
            .types
            .row
            .cast(Ref::slot::<0>(&borrow))
            .unwrap();
        row.enter_sync(strand, |strand, row| {
            unpack_columns(strand, row, Some(&annex.consumed), annex.keyed, unpack)
        })
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let borrow = this.borrow(strand)?;
        let row = annex
            .global
            .types
            .row
            .cast(Ref::slot::<0>(&borrow))
            .unwrap();
        row.enter_sync(strand, |strand, row| {
            spread_columns(
                strand,
                row,
                Some(&annex.consumed),
                annex.keyed,
                context,
                sink,
            )
        })
    }
}
