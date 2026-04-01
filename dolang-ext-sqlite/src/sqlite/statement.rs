use std::{
    alloc::{Layout, handle_alloc_error},
    cell::Cell,
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
    ptr,
};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, State, Strand, error::ResultExt,
    object::Mut, object::TypeBuilder, unpack,
};
use libsqlite3_sys::{
    SQLITE_DONE, SQLITE_OK, SQLITE_ROW, sqlite3_bind_blob, sqlite3_bind_double, sqlite3_bind_int64,
    sqlite3_bind_null, sqlite3_bind_parameter_index, sqlite3_bind_text, sqlite3_changes,
    sqlite3_clear_bindings, sqlite3_column_count, sqlite3_column_decltype, sqlite3_db_handle,
    sqlite3_finalize, sqlite3_reset, sqlite3_step, sqlite3_stmt,
};

use crate::global::Global;

use super::{
    AssertSend, Epoch,
    row::{Rows, RowsAnnex},
};

/// Query state tracking for statements
pub(super) enum QueryState {
    None,
    Active { owned: bool },
}

pub(crate) struct Statement {
    pub(super) query: QueryState,
}

pub(crate) struct StatementAnnex<'v> {
    global: State<'v, Global<'v>>,
    /// Raw SQLite statement pointer (nullable)
    pub(super) raw: Cell<*mut sqlite3_stmt>,
    /// Epoch counter bumped on every query/execute
    pub(super) epoch: Cell<Epoch>,
    /// Row epoch bumped on each next()
    pub(super) row_epoch: Cell<Epoch>,
    /// Per-column flag: true if the column was declared BOOLEAN or BOOL
    pub(super) bool_columns: Box<[bool]>,
}

impl<'v> StatementAnnex<'v> {
    pub(crate) fn new(global: State<'v, Global<'v>>, raw: *mut sqlite3_stmt) -> Self {
        let bool_columns = unsafe { scan_bool_columns(raw) };
        Self {
            global,
            raw: Cell::new(raw),
            epoch: Cell::new(0),
            row_epoch: Cell::new(0),
            bool_columns,
        }
    }

    fn bump_epoch(&self) -> Epoch {
        let new = self.epoch.get() + 1;
        self.epoch.set(new);
        new
    }

    pub(super) fn bump_row_epoch(&self) -> Epoch {
        let new = self.row_epoch.get() + 1;
        self.row_epoch.set(new);
        new
    }
}

impl Drop for StatementAnnex<'_> {
    fn drop(&mut self) {
        let raw = self.raw.get();
        if !raw.is_null() {
            let raw = AssertSend(raw);
            tokio::spawn(async move {
                tokio::task::spawn_blocking(move || {
                    unsafe { sqlite3_finalize(raw.into_inner()) };
                })
                .await
                .ok();
            });
        }
    }
}

impl Statement {
    pub(super) fn new() -> Self {
        Self {
            query: QueryState::None,
        }
    }

    fn is_query_active(&self) -> bool {
        matches!(self.query, QueryState::Active { .. })
    }
}

impl<'v> Object<'v> for Statement {
    const NAME: &'v str = "Statement";
    const MODULE: &'v str = "sqlite";
    const SLOTS: usize = 1;
    type Annex = StatementAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn clear<'a>(this: Instance<'v, 'a, Self>) {
        let annex = this.annex();
        let global = annex.global;
        let mut borrow = this.borrow_mut_unwrap();
        if borrow.is_query_active() {
            borrow.query = QueryState::None;
            // Release borrow on connection
            global
                .types
                .connection
                .downcast(Mut::slot::<0>(&borrow))
                .unwrap()
                .annex();
            // Finalize statement
            let raw = annex.raw.get();
            if !raw.is_null() {
                unsafe { sqlite3_finalize(raw) };
                annex.raw.set(ptr::null_mut());
            }
        }
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method_with_slots(
                "query",
                async move |this, strand, args, out, [mut wrapper]| {
                    let annex = this.annex();
                    let global = annex.global;
                    let mut borrow = this.borrow_mut(strand)?;

                    let raw = annex.raw.get();
                    if raw.is_null() {
                        return Err(Error::state_error(strand, "statement closed"));
                    }

                    let epoch = annex.bump_epoch();
                    unsafe { bind_keyword_params(strand, raw, args)? };

                    // Mark query as active
                    borrow.query = QueryState::Active { owned: false };
                    drop(borrow);

                    global.types.rows.create_with_annex(
                        strand,
                        Rows,
                        RowsAnnex { global, epoch },
                        &mut wrapper,
                    );

                    Output::set(
                        strand,
                        Mut::slot_mut::<0>(
                            &mut global
                                .types
                                .rows
                                .downcast(&wrapper)
                                .unwrap()
                                .borrow_mut_unwrap(),
                        ),
                        this,
                    );

                    Output::set(strand, out, wrapper);
                    Ok(())
                },
            )
            .method_with_slots(
                "execute",
                async move |this, strand, args, out, [mut conn]| {
                    let annex = this.annex();
                    let mut borrow = this.borrow_mut(strand)?;

                    Output::set(strand, &mut conn, Mut::slot::<0>(&borrow));
                    let conn = annex.global.types.connection.downcast(&conn).unwrap();
                    if conn.annex().raw.get().is_null() {
                        return Err(Error::state_error(strand, "connection closed"));
                    }

                    let raw = annex.raw.get();
                    if raw.is_null() {
                        return Err(Error::state_error(strand, "statement closed"));
                    }

                    borrow.query = QueryState::None;
                    drop(borrow);
                    annex.bump_epoch();
                    unsafe { bind_keyword_params(strand, raw, args)? };

                    // Execute to completion
                    let affected = conn
                        .annex()
                        .busy_retry(strand, async move |strand| {
                            let raw = AssertSend(raw);
                            conn.annex()
                                .with_raw(strand, move |_| unsafe {
                                    let raw = raw.into_inner();
                                    let mut rc = sqlite3_step(raw);
                                    while rc == SQLITE_ROW {
                                        rc = sqlite3_step(raw);
                                    }
                                    if rc == SQLITE_DONE {
                                        Ok(sqlite3_changes(sqlite3_db_handle(raw)) as i64)
                                    } else {
                                        Err(rc)
                                    }
                                })
                                .await
                        })
                        .await?;

                    if affected < 0 {
                        return Err(Error::runtime(strand, "execution failed"));
                    }

                    Output::set(strand, out, affected);
                    Ok(())
                },
            )
            .method("close", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;

                let mut borrow = this.borrow_mut(strand)?;
                if borrow.is_query_active() {
                    borrow.query = QueryState::None;
                }

                let raw = annex.raw.get();
                if !raw.is_null() {
                    let raw = AssertSend(raw);
                    tokio::task::spawn_blocking(move || unsafe {
                        sqlite3_finalize(raw.into_inner());
                    })
                    .await
                    .into_do(strand)?;
                    annex.raw.set(ptr::null_mut());
                }
                Ok(())
            })
    }
}

/// Scans the declared types of all columns in a freshly prepared statement and
/// returns a boxed slice indicating which columns have a BOOLEAN or BOOL declared
/// type.  Must be called before any `sqlite3_step`.
unsafe fn scan_bool_columns(raw: *mut sqlite3_stmt) -> Box<[bool]> {
    unsafe {
        let count = sqlite3_column_count(raw);
        (0..count)
            .map(|i| {
                let decltype = sqlite3_column_decltype(raw, i);
                if decltype.is_null() {
                    return false;
                }
                let s = CStr::from_ptr(decltype as *const c_char).to_bytes();
                s.eq_ignore_ascii_case(b"boolean") || s.eq_ignore_ascii_case(b"bool")
            })
            .collect()
    }
}

// Destructor callback for blob data
unsafe extern "C" fn free_data(ptr: *mut c_void) {
    if !ptr.is_null() {
        unsafe { libc::free(ptr) };
    }
}

unsafe fn bind_keyword_params<'v, 's>(
    strand: &mut Strand<'v, 's>,
    raw: *mut sqlite3_stmt,
    args: Args<'v, '_>,
) -> Result<'v, 's, ()> {
    unsafe {
        sqlite3_reset(raw);
        sqlite3_clear_bindings(raw);
    };

    for arg in args {
        match arg {
            Arg::Pos(_) => return Err(Error::unexpected_positional(strand, 0)),
            Arg::Key(sym, value) => {
                // sqlite3_bind_parameter_index expects the name WITH the prefix (e.g., ":name")
                let name = CString::new(format!(":{}", sym.as_str(strand))).into_do(strand)?;
                let rc = unsafe {
                    let idx = sqlite3_bind_parameter_index(raw, name.as_ptr());
                    if value.is_nil() {
                        sqlite3_bind_null(raw, idx)
                    } else if let Some(i) = value.as_i64(strand) {
                        sqlite3_bind_int64(raw, idx, i)
                    } else if let Some(f) = value.as_f64(strand) {
                        sqlite3_bind_double(raw, idx, f)
                    } else if let Some(s) = value.as_str(strand) {
                        if s.len() > i32::MAX as usize || s.contains('\0') {
                            return Err(Error::runtime(strand, "invalid string"));
                        }
                        let ptr = libc::malloc(s.len() + 1) as *mut u8;
                        if ptr.is_null() {
                            handle_alloc_error(Layout::from_size_align(s.len() + 1, 1).unwrap());
                        }
                        std::ptr::copy_nonoverlapping(s.as_ptr(), ptr, s.len());
                        *ptr.add(s.len()) = b'\0';
                        let rc = sqlite3_bind_text(raw, idx, ptr as *const _, -1, Some(free_data));
                        if rc != SQLITE_OK {
                            free_data(ptr as *mut _)
                        }
                        rc
                    } else if let Some(b) = value.as_bool(strand) {
                        sqlite3_bind_int64(raw, idx, b as i64)
                    } else if let Some(b) = value.as_bin(strand) {
                        let ptr = libc::malloc(b.len()) as *mut u8;
                        if ptr.is_null() {
                            handle_alloc_error(Layout::from_size_align(b.len(), 1).unwrap());
                        }
                        std::ptr::copy_nonoverlapping(b.as_ptr(), ptr, b.len());
                        let rc = sqlite3_bind_blob(
                            raw,
                            idx,
                            ptr as *const _,
                            b.len() as i32,
                            Some(free_data),
                        );
                        if rc != SQLITE_OK {
                            free_data(ptr as *mut _)
                        }
                        rc
                    } else {
                        return Err(Error::type_error(
                            strand,
                            format!(
                                "unsupported type for SQL parameter: {}",
                                sym.as_str(strand.vm())
                            ),
                        ));
                    }
                };

                if rc != SQLITE_OK {
                    return Err(Error::runtime(strand, "failed to bind parameter"));
                }
            }
        }
    }

    Ok(())
}
