use std::{
    cell::Cell,
    ffi::{CStr, CString},
    fmt, ptr,
};

use dolang::runtime::{
    Error, Instance, Object, Result, State, Strand, call, error::ResultExt, method, unpack,
    vm::Builder,
};
use libsqlite3_sys::{
    SQLITE_BUSY, SQLITE_LOCKED, SQLITE_OK, SQLITE_OPEN_CREATE, SQLITE_OPEN_READWRITE, sqlite3,
    sqlite3_close, sqlite3_errmsg, sqlite3_errstr, sqlite3_open_v2,
};

use crate::global::Global;

pub(crate) mod connection;
pub(crate) mod row;
pub(crate) mod statement;

use connection::{Connection, ConnectionAnnex};

#[derive(Copy, Clone)]
struct AssertSend<T>(T);

unsafe impl<T> Send for AssertSend<T> {}

impl<T> AssertSend<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

type Epoch = u64;

unsafe fn sqlite_error_message(raw: *mut sqlite3) -> String {
    unsafe {
        let msg = sqlite3_errmsg(raw);
        if msg.is_null() {
            "<unknown>".into()
        } else {
            CStr::from_ptr(msg).to_string_lossy().to_string()
        }
    }
}

unsafe fn map_sqlite_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    rc: i32,
    raw: *mut sqlite3,
) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    let msg = unsafe { sqlite_error_message(raw) };
    if matches!(rc, SQLITE_BUSY | SQLITE_LOCKED) {
        Error::object_with_annex(strand, global.types.busy, SqliteBusy, ErrorAnnex(rc, msg))
    } else {
        Error::object_with_annex(strand, global.types.error, SqliteError, ErrorAnnex(rc, msg))
    }
}

unsafe fn sqlite_error_string(rc: i32) -> String {
    unsafe {
        let msg = sqlite3_errstr(rc);
        if msg.is_null() {
            "<unknown>".into()
        } else {
            CStr::from_ptr(msg).to_string_lossy().to_string()
        }
    }
}

fn map_sqlite_errcode<'v, 's>(strand: &mut Strand<'v, 's>, rc: i32) -> Error<'v, 's> {
    let global = strand.state::<Global<'v>>();
    let msg = unsafe { sqlite_error_string(rc) };
    if matches!(rc, SQLITE_BUSY | SQLITE_LOCKED) {
        Error::object_with_annex(strand, global.types.busy, SqliteBusy, ErrorAnnex(rc, msg))
    } else {
        Error::object_with_annex(strand, global.types.error, SqliteError, ErrorAnnex(rc, msg))
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    // Register the shell VFS for container-aware file operations.
    #[cfg(unix)]
    crate::vfs::register_vfs().expect("Failed to register shell VFS");

    // Get symbols for keyword arguments
    let retries = builder.sym("retries");
    let min_wait = builder.sym("min_wait");
    let max_wait = builder.sym("max_wait");
    let close = builder.sym("close");

    // Register the sqlite.open function
    builder
        .module("sqlite")
        .function("open", async move |strand, args, out| {
            let ([path], [block, retries, min_wait, max_wait]) = unpack!(
                strand,
                args,
                1,
                1,
                retries = None,
                min_wait = None,
                max_wait = None
            )?;
            let path = dolang_ext_shell::as_path(strand, &path)
                .ok_or_else(|| Error::type_error(strand, "path: expected str"))?;
            let path = CString::new(path.as_ref().to_string_lossy().as_ref()).into_do(strand)?;

            // Parse retry configuration with defaults
            let busy_retries = if let Some(v) = retries {
                v.to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "busy_retries: expected int"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?
            } else {
                connection::DEFAULT_RETRIES
            };
            let busy_min_wait = if let Some(v) = min_wait {
                v.to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "busy_min_wait: expected int"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?
            } else {
                connection::DEFAULT_MIN_WAIT
            };
            let busy_max_wait = if let Some(v) = max_wait {
                v.to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "busy_max_wait: expected int"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?
            } else {
                connection::DEFAULT_MAX_WAIT
            };

            let mut raw: *mut sqlite3 = ptr::null_mut();

            #[cfg(unix)]
            {
                // Check if we're in a container context
                let vfs = dolang_ext_shell::vfs(strand);
                let path = path.clone();

                // Open the connection with appropriate VFS
                if let Some(vfs) = vfs {
                    raw = tokio::task::spawn_blocking(move || {
                        crate::vfs::with_shell(vfs, || {
                            let mut raw: *mut sqlite3 = ptr::null_mut();
                            let rc = unsafe {
                                sqlite3_open_v2(
                                    path.as_ptr(),
                                    &mut raw,
                                    SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
                                    c"dolang-shell".as_ptr() as *const _,
                                )
                            };
                            if rc != SQLITE_OK {
                                if !raw.is_null() {
                                    unsafe { sqlite3_close(raw) };
                                }
                                Err(rc)
                            } else {
                                Ok(AssertSend(raw))
                            }
                        })
                    })
                    .await
                    .into_do(strand)?
                    .map_err(|rc| map_sqlite_errcode(strand, rc))?
                    .into_inner()
                }
            };

            if raw.is_null() {
                raw = tokio::task::spawn_blocking(move || {
                    let mut raw: *mut sqlite3 = ptr::null_mut();
                    let rc = unsafe {
                        sqlite3_open_v2(
                            path.as_ptr(),
                            &mut raw,
                            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
                            ptr::null(),
                        )
                    };
                    if rc != SQLITE_OK {
                        if !raw.is_null() {
                            unsafe { sqlite3_close(raw) };
                        }
                        Err(rc)
                    } else {
                        Ok(AssertSend(raw))
                    }
                })
                .await
                .into_do(strand)?
                .map_err(|rc| map_sqlite_errcode(strand, rc))?
                .into_inner()
            }

            if let Some(block) = block {
                strand
                    .with_slots(async |strand, [mut wrapper, mut tmp]| {
                        global.types.connection.create_with_annex(
                            strand,
                            Connection,
                            ConnectionAnnex {
                                global,
                                raw: Cell::new(raw),
                                in_use: Cell::new(false),
                                in_transaction: Cell::new(false),
                                pending_close: Cell::new(false),
                                epoch: Cell::new(0),
                                #[cfg(unix)]
                                agent: dolang_ext_shell::vfs(strand),
                                busy_retries,
                                busy_min_wait,
                                busy_max_wait,
                            },
                            &mut wrapper,
                        );

                        let result = call!(strand, block, out, &wrapper).await;

                        strand
                            .with_interrupt_mask(true, async move |strand| {
                                let _ = method!(strand, &wrapper, close, &mut tmp).await;
                            })
                            .await;

                        result
                    })
                    .await
            } else {
                global.types.connection.create_with_annex(
                    strand,
                    Connection,
                    ConnectionAnnex {
                        global,
                        raw: Cell::new(raw),
                        in_use: Cell::new(false),
                        in_transaction: Cell::new(false),
                        pending_close: Cell::new(false),
                        epoch: Cell::new(0),
                        #[cfg(unix)]
                        agent: None,
                        busy_retries,
                        busy_min_wait,
                        busy_max_wait,
                    },
                    out,
                );
                Ok(())
            }
        })
        .value("Error", global.types.error)
        .value("Busy", global.types.busy)
        .commit();
}

pub(crate) struct SqliteError;
pub(crate) struct SqliteBusy;
pub(crate) struct ErrorAnnex(#[expect(dead_code)] i32, String);

impl<'v> Object<'v> for SqliteError {
    const NAME: &'v str = "Error";
    const MODULE: &'v str = "sqlite";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().1).into_do(strand)
    }
}

impl<'v> Object<'v> for SqliteBusy {
    const NAME: &'v str = "Busy";
    const MODULE: &'v str = "sqlite";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().1).into_do(strand)
    }
}
