use crate::global::Tag;

use std::{
    cell::Cell,
    ffi::{CString, c_int},
    ptr, result,
    time::Duration,
};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
    error::ResultExt,
    method,
    object::{Mut, Ref, TypeBuilder},
    strand::InterruptMask,
    unpack, with_instance, with_interrupt_mask, with_slots,
};

#[cfg(unix)]
use dolang_vfs::Vfs;
use libsqlite3_sys::{
    SQLITE_OK, sqlite3, sqlite3_exec, sqlite3_finalize, sqlite3_interrupt, sqlite3_prepare_v2,
    sqlite3_randomness, sqlite3_stmt,
};

unsafe extern "C" {
    fn sqlite3_close_v2(conn: *mut sqlite3) -> c_int;
}

use crate::global::Global;

use super::{
    AssertSend, Epoch,
    error::Failure,
    statement::{Statement, StatementAnnex, Template, bind_template, compile_template},
};

/// Returns a random `u32` drawn from SQLite's internal PRNG.
/// `sqlite3_randomness` is thread-safe and already seeded once any SQLite
/// connection has been opened, so no extra initialisation is needed.
fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    unsafe { sqlite3_randomness(4, buf.as_mut_ptr().cast()) };
    u32::from_le_bytes(buf)
}

pub(super) const DEFAULT_RETRIES: u32 = 10;
pub(super) const DEFAULT_MIN_WAIT: u32 = 1;
pub(super) const DEFAULT_MAX_WAIT: u32 = 1000;

/// Connection object wrapping a SQLite connection.
pub(crate) struct Connection;

pub(crate) struct ConnectionAnnex<'v> {
    pub(super) global: State<'v, Global<'v>>,
    /// Raw SQLite connection pointer (nullable for close)
    pub(super) raw: Cell<*mut sqlite3>,
    /// Strand-level lock - prevents concurrent operations on same connection
    pub(super) in_use: Cell<bool>,
    /// Transaction state
    pub(super) in_transaction: Cell<bool>,
    /// Close state
    pub(super) pending_close: Cell<bool>,
    /// Epoch counter bumped on transaction begin/end/close
    pub(super) epoch: Cell<Epoch>,
    /// Optional shell VFS for container-aware operations.
    #[cfg(unix)]
    pub(super) agent: Option<Vfs>,
    /// Maximum number of retries on SQLITE_BUSY
    pub(super) busy_retries: u32,
    /// Initial wait time in milliseconds for retry backoff
    pub(super) busy_min_wait: u32,
    /// Maximum wait time in milliseconds for retry backoff
    pub(super) busy_max_wait: u32,
}

impl ConnectionAnnex<'_> {
    /// Execute a closure with the raw connection pointer in a blocking task
    /// Wraps in `with_shell` if a shell VFS client is available.
    pub(super) async fn with_raw<'v, 's, F, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        f: F,
    ) -> Result<'v, 's, R>
    where
        F: FnOnce(*mut sqlite3) -> result::Result<R, i32> + Send + 'static,
        R: Send + 'static,
    {
        // Check strand-level lock
        if self.in_use.get() {
            return Err(Error::concurrency_msg(strand, "connection already in use"));
        }

        let raw = self.raw.get();
        if raw.is_null() {
            return Err(Error::state_error(strand, "connection closed"));
        }

        self.in_use.set(true);
        let pending_close = self.pending_close.get();

        let raw = AssertSend(raw);
        // The failure is captured before the handle can be closed, and inside
        // `with_shell`, where the shell VFS keeps the OS error behind it.
        let run = move || {
            let raw = raw.into_inner();
            let res = f(raw).map_err(|rc| unsafe { Failure::capture(raw, rc) });
            if pending_close {
                unsafe { sqlite3_close_v2(raw) };
            }
            res
        };
        #[cfg(unix)]
        let agent = self.agent.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            if let Some(agent) = agent {
                return crate::vfs::with_shell(agent, run);
            }
            run()
        });
        // An interrupt of the strand interrupts the running statement, and the
        // strand's own interrupt error replaces the `SQLITE_INTERRUPT` that
        // follows. A closing task frees the handle itself, so it is left to
        // finish.
        let result = match strand
            .interrupt_guard(async |_| Ok((&mut task).await))
            .await
        {
            Ok(joined) => joined
                .map_err(|err| super::join_error(strand, err))
                .and_then(|res| res.map_err(|failure| failure.raise(strand))),
            Err(interrupt) => {
                if !pending_close {
                    unsafe { sqlite3_interrupt(raw.into_inner()) };
                }
                let _ = with_interrupt_mask!(strand, InterruptMask::all(), task.await);
                Err(interrupt)
            }
        };

        if pending_close {
            self.raw.set(ptr::null_mut())
        }

        self.in_use.set(false);

        result
    }

    /// Execute SQL that doesn't return rows
    pub(super) async fn exec<'v, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        sql: &str,
    ) -> Result<'v, 's, ()> {
        let sql = CString::new(sql).into_do_value(strand)?;
        self.with_raw(strand, move |raw| {
            let rc =
                unsafe { sqlite3_exec(raw, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) };
            if rc != SQLITE_OK { Err(rc) } else { Ok(()) }
        })
        .await
    }

    pub(super) fn bump_epoch(&self) -> Epoch {
        let new = self.epoch.get() + 1;
        self.epoch.set(new);
        new
    }

    pub(super) async fn busy_retry<'v, 's, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        mut f: impl AsyncFnMut(&mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let global = strand.state::<Tag>();
        if self.in_transaction.get() {
            return f(strand).await;
        }
        let mut wait = self.busy_min_wait;
        let mut retries = self.busy_retries;
        with_slots!(strand, |mut tmp| {
            loop {
                let mut res = f(strand).await;
                if retries != 0
                    && let Err(e) = &mut res
                {
                    e.get_value(strand, &mut tmp);
                    if global.types.busy.cast(&tmp).is_some() {
                        let jitter = random_u32() % (wait / 2 + 1);
                        tokio::time::sleep(Duration::from_millis((wait + jitter).into())).await;
                        wait = self.busy_max_wait.min(wait * 2);
                        retries -= 1;
                        continue;
                    }
                }
                break res;
            }
        })
    }
}

impl Drop for ConnectionAnnex<'_> {
    fn drop(&mut self) {
        let raw = self.raw.get();
        if !raw.is_null() {
            // Spawn a background task to close the connection
            // This may involve I/O operations, so we shouldn't block drop
            let raw = AssertSend(raw);
            #[cfg(unix)]
            let agent = self.agent.clone();
            tokio::spawn(async move {
                tokio::task::spawn_blocking(move || {
                    #[cfg(unix)]
                    {
                        if let Some(agent) = agent {
                            crate::vfs::with_shell(agent, || {
                                unsafe { sqlite3_close_v2(raw.into_inner()) };
                            });
                        } else {
                            unsafe { sqlite3_close_v2(raw.into_inner()) };
                        }
                    }
                    #[cfg(not(unix))]
                    unsafe {
                        sqlite3_close_v2(raw.into_inner())
                    };
                })
                .await
                .ok();
            });
        }
    }
}

impl<'v> Object<'v> for Connection {
    const NAME: &'v str = "Connection";
    const MODULE: &'v str = "sqlite";
    type Annex = ConnectionAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let close = builder.sym("close");
        let execute = builder.sym("execute");
        builder
            .method_with_slots(
                "prepare",
                async move |this, strand, args, out, [mut wrapper, mut tmp]| {
                    let ([sql], [block]) = unpack!(strand, args, 1, 1)?;
                    let template = compile_template(strand, &sql)?;

                    let stmt = create_statement(strand, &this.annex(), &template.sql).await?;

                    wrap_statement(this, strand, stmt, template, Slot::reborrow(&mut wrapper))?;

                    if let Some(block) = block {
                        // Call the block with the statement handle
                        let result = call!(strand, block, &wrapper, out);

                        // Always close the statement, even on error
                        with_interrupt_mask!(strand, InterruptMask::all(), {
                            let _ = method!(strand, &wrapper, close, &mut tmp);
                        });

                        result
                    } else {
                        Output::set(strand, out, wrapper);
                        Ok(())
                    }
                },
            )
            .method_with_slots(
                "execute",
                async move |this, strand, args, out, [mut wrapper, tmp]| {
                    let ([sql], [], args) = unpack!(strand, args, 1, 0, ...)?;
                    let template = compile_template(strand, &sql)?;

                    let stmt = create_statement(strand, &this.annex(), &template.sql).await?;

                    wrap_statement(this, strand, stmt, template, Slot::reborrow(&mut wrapper))?;
                    let res = wrapper.method(strand, execute, args, out).await;
                    let _ = with_interrupt_mask!(strand, InterruptMask::all(), {
                        method!(strand, wrapper, close, tmp)
                    });
                    res
                },
            )
            .method_with_slots(
                "transaction",
                async move |this, strand, args, mut out, [mut tmp]| {
                    let annex = this.annex();
                    let global = annex.global;
                    let ([block], []) = unpack!(strand, args, 1, 0)?;

                    let mut retries = annex.busy_retries;
                    let mut wait = annex.busy_min_wait;

                    loop {
                        // Check if transaction already active
                        if annex.in_transaction.get() {
                            return Err(Error::concurrency_msg(
                                strand,
                                "transaction already active on this connection",
                            ));
                        }

                        let mut res =
                            transact(this, strand, &block, Slot::reborrow(&mut out)).await;
                        // Rollback if transaction not closed
                        if annex.in_transaction.get() {
                            let _ = with_interrupt_mask!(strand, InterruptMask::all(), {
                                annex
                                    .exec(strand, if res.is_err() { "ROLLBACK" } else { "COMMIT" })
                                    .await
                            });
                            annex.bump_epoch();
                            annex.in_transaction.set(false);

                            if retries != 0
                                && let Err(e) = &mut res
                            {
                                e.get_value(strand, &mut tmp);
                                if global.types.busy.cast(&tmp).is_some() {
                                    let jitter = random_u32() % (wait / 2 + 1);
                                    tokio::time::sleep(Duration::from_millis(
                                        (wait + jitter).into(),
                                    ))
                                    .await;
                                    wait = annex.busy_max_wait.min(wait * 2);
                                    retries -= 1;
                                    continue;
                                }
                            }
                        }
                        break res;
                    }
                },
            )
            .method("close", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;
                annex.pending_close.set(true);
                annex.with_raw(strand, move |_| Ok(())).await
            })
    }
}

/// Binds the template's own values into the fresh statement and wraps it.
///
/// Binding happens here rather than at each call because these values belong to
/// the statement, not to the caller: they came from the template's text.
fn wrap_statement<'v, 's>(
    this: Instance<'v, '_, Connection>,
    strand: &mut Strand<'v, 's>,
    stmt: *mut sqlite3_stmt,
    template: Template,
    mut wrapper: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let global = this.annex().global;
    // Nothing owns the statement yet, so a rejected template has to release it.
    let (prebound, param_count) = match unsafe { bind_template(strand, stmt, template) } {
        Ok(bound) => bound,
        Err(e) => {
            unsafe { sqlite3_finalize(stmt) };
            return Err(e);
        }
    };
    global.types.statement.create_with_annex(
        strand,
        Statement::new(),
        StatementAnnex::new(global, stmt, prebound, param_count),
        Slot::reborrow(&mut wrapper),
    );

    // Store the connection object in slot 0 of the statement
    with_instance!(
        strand,
        global.types.statement.cast(&wrapper).unwrap(),
        |inst| {
            Output::set(
                strand,
                Mut::slot_mut::<0>(&mut inst.borrow_mut_unwrap()),
                this,
            );
        }
    );

    Ok(())
}

async fn create_statement<'v, 's>(
    strand: &mut Strand<'v, 's>,
    annex: &ConnectionAnnex<'v>,
    sql: &str,
) -> Result<'v, 's, *mut sqlite3_stmt> {
    let stmt = annex
        .busy_retry(strand, async |strand| {
            let sql = CString::new(sql).into_do_value(strand)?;
            annex
                .with_raw(strand, move |raw| {
                    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
                    let rc = unsafe {
                        sqlite3_prepare_v2(
                            raw,
                            sql.as_ptr(),
                            -1, // Read until null terminator
                            &mut stmt,
                            ptr::null_mut(),
                        )
                    };
                    if rc != SQLITE_OK {
                        return Err(rc);
                    }
                    Ok(AssertSend(stmt))
                })
                .await
        })
        .await?
        .into_inner();
    // SQLite reports success and hands back nothing at all for SQL that is
    // empty or only a comment. Saying so here beats the "statement closed" the
    // null pointer would otherwise provoke on first use.
    if stmt.is_null() {
        return Err(Error::value(strand, "SQL contains no statement"));
    }
    Ok(stmt)
}

async fn transact<'v, 's, 'a>(
    this: Instance<'v, 'a, Connection>,
    strand: &'a mut Strand<'v, 's>,
    block: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let annex = this.annex();
    // Begin transaction
    annex.exec(strand, "BEGIN").await?;
    annex.in_transaction.set(true);

    let epoch = annex.bump_epoch();

    // Create Transaction handle and call block
    with_slots!(strand, |mut wrapper| {
        annex.global.types.transaction.create_with_annex(
            strand,
            Transaction,
            TransactionAnnex {
                global: annex.global,
                epoch,
            },
            &mut wrapper,
        );

        // Store the connection object in slot 0
        with_instance!(
            strand,
            annex.global.types.transaction.cast(&wrapper).unwrap(),
            |inst| {
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut inst.borrow_mut_unwrap()),
                    this,
                );
            }
        );

        // Call the block with the transaction handle
        call!(strand, &block, &wrapper, out)
    })
}

pub(crate) struct Transaction;

pub(crate) struct TransactionAnnex<'v> {
    global: State<'v, Global<'v>>,
    epoch: Epoch,
}

impl<'v> Object<'v> for Transaction {
    const NAME: &'v str = "Transaction";
    const MODULE: &'v str = "sqlite";
    const SLOTS: usize = 1;
    type Annex = TransactionAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("commit", async move |this, strand, args, _out| {
                let annex = this.annex();
                let borrow = this.borrow(strand)?;
                with_instance!(
                    strand,
                    annex
                        .global
                        .types
                        .connection
                        .cast(Ref::slot::<0>(&borrow))
                        .unwrap(),
                    |conn| {
                        if conn.annex().epoch.get() != annex.epoch {
                            return Err(Error::concurrency_msg(strand, "stale transaction"));
                        }

                        let ([], []) = unpack!(strand, args, 0, 0)?;

                        conn.annex().in_transaction.set(false);
                        conn.annex().bump_epoch();
                        conn.annex().exec(strand, "COMMIT").await?;
                        Ok(())
                    }
                )
            })
            .method("rollback", async move |this, strand, args, _out| {
                let annex = this.annex();
                let borrow = this.borrow(strand)?;
                with_instance!(
                    strand,
                    annex
                        .global
                        .types
                        .connection
                        .cast(Ref::slot::<0>(&borrow))
                        .unwrap(),
                    |conn| {
                        if conn.annex().epoch.get() != annex.epoch {
                            return Err(Error::concurrency_msg(strand, "stale transaction"));
                        }

                        let ([], []) = unpack!(strand, args, 0, 0)?;

                        conn.annex().in_transaction.set(false);
                        conn.annex().bump_epoch();
                        conn.annex().exec(strand, "ROLLBACK").await?;
                        Ok(())
                    }
                )
            })
    }
}
