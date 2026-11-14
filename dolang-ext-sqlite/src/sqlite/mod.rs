use std::{cell::Cell, ffi::CString, path::Path, ptr};

use dolang::runtime::{
    Error, State, Strand, call, error::ResultExt, method, strand::InterruptMask, unpack,
    vm::Register, with_interrupt_mask, with_slots,
};
use libsqlite3_sys::{
    SQLITE_OK, SQLITE_OPEN_CREATE, SQLITE_OPEN_READWRITE, sqlite3, sqlite3_close, sqlite3_open_v2,
};

use crate::global::Global;

/// Maps a failed blocking task: a panic resumes on this thread, and a task
/// dropped by a shutting-down runtime raises `CanceledError`.
pub(crate) fn join_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    err: tokio::task::JoinError,
) -> Error<'v, 's> {
    if err.is_panic() {
        std::panic::resume_unwind(err.into_panic())
    }
    Error::canceled(strand)
}

pub(crate) mod connection;
pub(crate) mod error;
pub(crate) mod row;
pub(crate) mod statement;

use connection::{Connection, ConnectionAnnex};
use error::Failure;

#[derive(Copy, Clone)]
struct AssertSend<T>(T);

unsafe impl<T> Send for AssertSend<T> {}

impl<T> AssertSend<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

type Epoch = u64;

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
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
                .ok_or_else(|| Error::type_error(strand, "path: expected Str"))?;
            let path = if path == Path::new(":memory:") {
                path
            } else {
                dolang_ext_shell::cwd(strand).join(path)
            };
            let path = CString::new(path.to_string_lossy().as_ref()).into_do_value(strand)?;

            // Parse retry configuration with defaults
            let busy_retries = if let Some(v) = retries {
                v.to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "busy_retries: expected Int"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?
            } else {
                connection::DEFAULT_RETRIES
            };
            let busy_min_wait = if let Some(v) = min_wait {
                v.to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "busy_min_wait: expected Int"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?
            } else {
                connection::DEFAULT_MIN_WAIT
            };
            let busy_max_wait = if let Some(v) = max_wait {
                v.to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "busy_max_wait: expected Int"))?
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
                if !vfs.is_direct() {
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
                                let failure = unsafe { Failure::capture(raw, rc) };
                                if !raw.is_null() {
                                    unsafe { sqlite3_close(raw) };
                                }
                                Err(failure)
                            } else {
                                Ok(AssertSend(raw))
                            }
                        })
                    })
                    .await
                    .map_err(|err| join_error(strand, err))?
                    .map_err(|failure| failure.raise(strand))?
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
                        let failure = unsafe { Failure::capture(raw, rc) };
                        if !raw.is_null() {
                            unsafe { sqlite3_close(raw) };
                        }
                        Err(failure)
                    } else {
                        Ok(AssertSend(raw))
                    }
                })
                .await
                .map_err(|err| join_error(strand, err))?
                .map_err(|failure| failure.raise(strand))?
                .into_inner()
            }

            if let Some(block) = block {
                with_slots!(strand, |mut wrapper, mut tmp| {
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
                            agent: (!dolang_ext_shell::vfs(strand).is_direct())
                                .then(|| dolang_ext_shell::vfs(strand)),
                            busy_retries,
                            busy_min_wait,
                            busy_max_wait,
                        },
                        &mut wrapper,
                    );

                    let result = call!(strand, block, &wrapper, out);

                    with_interrupt_mask!(strand, InterruptMask::all(), {
                        let _ = method!(strand, &wrapper, close, &mut tmp);
                    });

                    result
                })
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
        .value("ConstraintError", global.types.constraint)
        .value("CorruptError", global.types.corrupt)
        .value("Connection", global.types.connection)
        .value("Statement", global.types.statement)
        .value("Transaction", global.types.transaction)
        .value("Row", global.types.row)
        .commit();
}
