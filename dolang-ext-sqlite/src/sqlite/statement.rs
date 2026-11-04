use std::{
    cell::Cell,
    ffi::{CStr, CString},
    os::raw::{c_char, c_int},
    ptr,
};

use bitvec::boxed::BitBox;
use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, State, Strand, Value,
    error::ResultExt,
    object::Mut,
    object::TypeBuilder,
    unpack,
    value::{View, fmt::Spec},
};
use libsqlite3_sys::{
    SQLITE_DONE, SQLITE_OK, SQLITE_ROW, SQLITE_TRANSIENT, sqlite3_bind_blob, sqlite3_bind_double,
    sqlite3_bind_int64, sqlite3_bind_null, sqlite3_bind_parameter_count,
    sqlite3_bind_parameter_index, sqlite3_bind_parameter_name, sqlite3_bind_text, sqlite3_changes,
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
    pub(super) bool_columns: BitBox,
    /// Values the template carried, with the bind index each resolved to.
    ///
    /// These are the statement's own: they are rebound from here on every call,
    /// so nothing of the template needs to be kept alive.
    prebound: Prebound,
    /// Total parameters SQLite found, which is what exhaustiveness is checked
    /// against.
    param_count: c_int,
}

impl<'v> StatementAnnex<'v> {
    pub(crate) fn new(
        global: State<'v, Global<'v>>,
        raw: *mut sqlite3_stmt,
        prebound: Prebound,
        param_count: c_int,
    ) -> Self {
        let bool_columns = unsafe { scan_bool_columns(raw) };
        Self {
            global,
            raw: Cell::new(raw),
            epoch: Cell::new(0),
            row_epoch: Cell::new(0),
            bool_columns,
            prebound,
            param_count,
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

    fn finalize<'a>(this: Instance<'v, 'a, Self>) {
        let annex = this.annex();
        let global = annex.global;
        let mut borrow = this.borrow_mut_unwrap();
        if borrow.is_query_active() {
            borrow.query = QueryState::None;
            // Release borrow on connection
            let _ = global
                .types
                .connection
                .cast(Mut::slot::<0>(&borrow))
                .unwrap();
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
                    unsafe { bind_params(strand, &annex, raw, args)? };

                    // Mark query as active
                    borrow.query = QueryState::Active { owned: false };
                    drop(borrow);

                    global.types.rows.create_with_annex(
                        strand,
                        Rows,
                        RowsAnnex { global, epoch },
                        &mut wrapper,
                    );

                    global
                        .types
                        .rows
                        .cast(&wrapper)
                        .unwrap()
                        .enter_sync(strand, |strand, inst| {
                            Output::set(
                                strand,
                                Mut::slot_mut::<0>(&mut inst.borrow_mut_unwrap()),
                                this,
                            );
                        });

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
                    let affected = annex
                        .global
                        .types
                        .connection
                        .cast(&conn)
                        .unwrap()
                        .enter(strand, async move |strand, conn| {
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
                            unsafe { bind_params(strand, &annex, raw, args)? };

                            // Execute to completion
                            conn.annex()
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
                                                Ok(sqlite3_changes(sqlite3_db_handle(raw)))
                                            } else {
                                                Err(rc)
                                            }
                                        })
                                        .await
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
                    .map_err(|err| super::join_error(strand, err))?;
                    annex.raw.set(ptr::null_mut());
                }
                Ok(())
            })
    }
}

/// Scans the declared types of all columns in a freshly prepared statement and
/// returns a bitmap indicating which columns have a BOOLEAN or BOOL declared
/// type.  Must be called before any `sqlite3_step`.
unsafe fn scan_bool_columns(raw: *mut sqlite3_stmt) -> BitBox {
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

/// The values a statement binds for itself, each with the index it resolved to.
pub(crate) type Prebound = Box<[(c_int, Scalar)]>;

/// A value converted to its SQLite representation.
///
/// The values a template carries are converted once, when the statement is
/// prepared, so a statement reused in a loop rebinds without going back to the
/// GC heap for them.
pub(crate) enum Scalar {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Box<[u8]>),
}

/// A parameter the template leaves for the caller to fill.
#[derive(PartialEq)]
enum Param {
    /// `${#0}` — filled by a positional argument.
    Pos(u32),
    /// `${#name}` — filled by a keyword argument.
    Key(String),
}

/// SQL compiled from a template, ready to prepare.
pub(super) struct Template {
    /// The text, with a named placeholder standing in for every segment that
    /// was not literal program text.
    pub(super) sql: String,
    /// Values the template interpolated, in placeholder order.
    values: Vec<Scalar>,
    /// How many distinct parameters the template declares. A hole used twice is
    /// one parameter, which is what a SQL template wants.
    params: usize,
}

/// The placeholder standing in for the interpolated value with ordinal `k`.
fn value_placeholder(k: usize) -> String {
    format!(":0v{k}")
}

/// The placeholder standing in for the positional parameter named `i`.
fn pos_placeholder(i: u32) -> String {
    format!(":0p{i}")
}

/// Appends a placeholder, spaced away from whatever literal text surrounds it.
///
/// A SQLite parameter name runs to the first character that cannot continue an
/// identifier, so `t"select ${#0}x"` would otherwise emit `:0p0x` and quietly
/// swallow the literal `x` into the parameter's name. The spaces keep the
/// promise the whole scheme rests on: literal text becomes SQL text, and a
/// placeholder is a token of its own.
fn push_placeholder(sql: &mut String, placeholder: &str) {
    sql.push(' ');
    sql.push_str(placeholder);
    sql.push(' ');
}

/// Compiles a template into SQL text plus the values it carried.
///
/// Every placeholder is emitted as a *named* SQLite parameter, including the
/// ones standing in for `${#0}` and for interpolated values. That is what makes
/// the scheme safe rather than merely convenient: SQLite gives a named wildcard
/// the next free index, while `?NNN` takes NNN literally, so the two forms alias
/// each other in text order — `select :a, ?1` is one parameter, not two. With
/// every placeholder named, SQLite assigns dense indices and does the
/// hole-to-index bookkeeping the extension would otherwise have to keep itself.
///
/// The mangled forms begin with a digit, which no `${#name}` can, so a template
/// cannot write a name that collides with one.
pub(super) fn compile_template<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sql: &Value<'v>,
) -> Result<'v, 's, Template> {
    let Some(seq) = sql.as_fmt(strand.vm()) else {
        return Err(Error::type_error(
            strand,
            "SQL must be a template (t\"...\"), so that interpolated values can \
             be bound rather than spliced into the statement",
        ));
    };
    let len = seq.len(strand)?;
    let mut out = Template {
        sql: String::new(),
        values: Vec::new(),
        params: 0,
    };
    let mut params: Vec<Param> = Vec::new();

    strand.with_slots_sync(|strand, [mut segment, mut inner]| {
        for index in 0..len {
            seq.get(strand, index, &mut segment)?;
            match segment.view(strand.vm()) {
                // Literal program text is the only thing that becomes SQL.
                View::Str(text) => strand.access(|access| out.sql.push_str(text.as_str(access))),
                View::FmtValue(value) => {
                    if value.spec(strand) != Spec::default() {
                        let source = value.source(strand)?;
                        return Err(spec_error(strand, source));
                    }
                    value.value(strand, &mut inner)?;
                    let scalar = to_scalar(strand, &inner)?;
                    push_placeholder(&mut out.sql, &value_placeholder(out.values.len()));
                    out.values.push(scalar);
                }
                View::FmtParam(param) => {
                    if param.spec(strand) != Spec::default() {
                        let source = param.source(strand)?;
                        return Err(spec_error(strand, source));
                    }
                    param.name(strand, &mut inner)?;
                    let param = param_name(strand, &inner)?;
                    match &param {
                        Param::Pos(i) => push_placeholder(&mut out.sql, &pos_placeholder(*i)),
                        Param::Key(name) => push_placeholder(&mut out.sql, &format!(":{name}")),
                    }
                    if !params.contains(&param) {
                        params.push(param);
                    }
                }
                _ => {
                    return Err(Error::type_error(
                        strand,
                        "template segment is neither literal text, a value, nor a parameter",
                    ));
                }
            }
        }
        Ok(())
    })?;

    out.params = params.len();
    Ok(out)
}

/// Rejects a specification on a segment bound to a SQL parameter.
///
/// A bound value goes through `sqlite3_bind_*` and is never rendered, so a
/// specification asks for something that cannot happen. Silently dropping it
/// would leave the program saying one thing and doing another.
fn spec_error<'v, 's>(strand: &mut Strand<'v, 's>, source: Option<String>) -> Error<'v, 's> {
    let what = source.unwrap_or_else(|| "parameter".into());
    Error::value(
        strand,
        format!("{what}: a SQL parameter is bound, not formatted, so it takes no specification"),
    )
}

/// Reads a parameter's name, which is an `Int` or a `Sym`.
fn param_name<'v, 's>(strand: &mut Strand<'v, 's>, name: &Value<'v>) -> Result<'v, 's, Param> {
    match name.view(strand.vm()) {
        View::Int(i) => match u32::try_from(i) {
            Ok(i) => Ok(Param::Pos(i)),
            // Only reachable from a runtime-built `FmtParam`: `${#-1}` does not
            // parse. A negative name has no placeholder spelling, since `-` ends
            // a SQLite parameter name rather than continuing it.
            Err(_) => Err(Error::value(
                strand,
                format!("{i}: a SQL parameter position must fit in 32 bits"),
            )),
        },
        View::Sym(sym) => {
            let name = sym.as_str(strand);
            if is_bare_name(name) {
                Ok(Param::Key(name.to_string()))
            } else {
                Err(Error::value(
                    strand,
                    format!("{name}: not spellable as a SQL parameter name"),
                ))
            }
        }
        _ => Err(Error::type_error(
            strand,
            "a SQL parameter must be named by an Int or a Sym",
        )),
    }
}

/// Is `name` an identifier, and so both a legal SQLite parameter name and
/// outside the digit-led namespace the mangled placeholders use?
fn is_bare_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Converts a value to the SQLite representation it binds as.
fn to_scalar<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, Scalar> {
    if value.is_nil() {
        Ok(Scalar::Null)
    } else if let Ok(i) = value.to_i64(strand) {
        Ok(Scalar::Int(i))
    } else if let Some(f) = value.as_f64(strand) {
        Ok(Scalar::Float(f))
    } else if let Some(s) = value.as_str(strand) {
        let text = strand.access(|access| s.as_str(access).to_string());
        check_text(strand, &text)?;
        Ok(Scalar::Text(text))
    } else if let Some(b) = value.as_bool(strand) {
        Ok(Scalar::Int(b as i64))
    } else if let Some(b) = value.as_bin(strand) {
        let bytes = strand.access(|access| b.as_slice(access).to_vec());
        if bytes.len() > i32::MAX as usize {
            return Err(Error::value(strand, "binary value too large to bind"));
        }
        Ok(Scalar::Blob(bytes.into_boxed_slice()))
    } else {
        Err(Error::type_error(
            strand,
            "expected nil, Bool, Int, Float, Str, or Bin for a SQL parameter",
        ))
    }
}

fn check_text<'v, 's>(strand: &mut Strand<'v, 's>, text: &str) -> Result<'v, 's, ()> {
    if text.contains('\0') {
        Err(Error::value(strand, "string contains a NUL"))
    } else if text.len() > i32::MAX as usize {
        Err(Error::value(strand, "string too large to bind"))
    } else {
        Ok(())
    }
}

/// Binds a converted value.
///
/// Text and blobs are handed over as `SQLITE_TRANSIENT` so SQLite takes its own
/// copy: the buffer belongs to the annex, which a dropped statement frees while
/// its deferred `sqlite3_finalize` is still queued.
unsafe fn bind_scalar(raw: *mut sqlite3_stmt, idx: c_int, scalar: &Scalar) -> c_int {
    unsafe {
        match scalar {
            Scalar::Null => sqlite3_bind_null(raw, idx),
            Scalar::Int(i) => sqlite3_bind_int64(raw, idx, *i),
            Scalar::Float(f) => sqlite3_bind_double(raw, idx, *f),
            Scalar::Text(s) => sqlite3_bind_text(
                raw,
                idx,
                s.as_ptr() as *const c_char,
                s.len() as c_int,
                SQLITE_TRANSIENT(),
            ),
            Scalar::Blob(b) => sqlite3_bind_blob(
                raw,
                idx,
                b.as_ptr() as *const _,
                b.len() as c_int,
                SQLITE_TRANSIENT(),
            ),
        }
    }
}

/// Resolves a prepared statement's placeholders and binds the template's own
/// values into it.
///
/// The parameter count is the check that nothing of SQLite's own placeholder
/// syntax survived in literal text. A `:name` or a `?` the template did not
/// write is a real parameter nobody will ever fill, which would silently step as
/// NULL — so it is an error here rather than a wrong answer later.
pub(super) unsafe fn bind_template<'v, 's>(
    strand: &mut Strand<'v, 's>,
    raw: *mut sqlite3_stmt,
    template: Template,
) -> Result<'v, 's, (Prebound, c_int)> {
    let count = unsafe { sqlite3_bind_parameter_count(raw) };
    let expected = template.values.len() + template.params;
    if count as usize > expected {
        // A `:name` or `?` the template did not write is a real parameter
        // nobody will ever fill, which would silently step as NULL.
        return Err(Error::value(
            strand,
            "SQL has a parameter the template does not account for; write a \
             hole as ${#name} rather than in SQLite's own placeholder syntax",
        ));
    }
    if (count as usize) < expected {
        // The usual cause is quoting an interpolation — `'$name'` puts the
        // placeholder inside a string literal, where it binds nothing.
        return Err(Error::value(
            strand,
            "SQL swallowed a parameter; an interpolation is bound as a value \
             and must not be quoted or spliced into a literal",
        ));
    }

    let mut prebound = Vec::with_capacity(template.values.len());
    for (k, scalar) in template.values.into_iter().enumerate() {
        let name = CString::new(value_placeholder(k)).expect("placeholders contain no NUL");
        let idx = unsafe { sqlite3_bind_parameter_index(raw, name.as_ptr()) };
        if idx == 0 {
            return Err(Error::runtime(strand, "interpolated value went missing"));
        }
        prebound.push((idx, scalar));
    }
    Ok((prebound.into_boxed_slice(), count))
}

/// Binds a call's arguments, and the statement's own values along with them.
///
/// Everything is cleared and rebound on each call, which is what makes the
/// exhaustiveness check fall out of a count: each argument names one parameter
/// and the prebound indices are disjoint from those, so a bind total short of
/// the parameter count means a hole nobody filled.
unsafe fn bind_params<'v, 's>(
    strand: &mut Strand<'v, 's>,
    annex: &StatementAnnex<'v>,
    raw: *mut sqlite3_stmt,
    args: Args<'v, '_>,
) -> Result<'v, 's, ()> {
    unsafe {
        sqlite3_reset(raw);
        sqlite3_clear_bindings(raw);
    };

    let mut filled = vec![false; annex.param_count as usize];
    for (idx, scalar) in &annex.prebound {
        let rc = unsafe { bind_scalar(raw, *idx, scalar) };
        if rc != SQLITE_OK {
            return Err(Error::runtime(strand, "failed to bind value"));
        }
        filled[*idx as usize - 1] = true;
    }

    let mut position = 0;
    for arg in args {
        let (placeholder, value, key) = match arg {
            Arg::Pos(value) => {
                let placeholder = pos_placeholder(position);
                position += 1;
                (placeholder, value, None)
            }
            Arg::Key(sym, value) => (format!(":{}", sym.as_str(strand)), value, Some(sym)),
        };
        let name = CString::new(placeholder).into_do_value(strand)?;
        let idx = unsafe { sqlite3_bind_parameter_index(raw, name.as_ptr()) };
        if idx == 0 {
            return Err(match key {
                None => Error::unexpected_positional(strand, position as usize - 1),
                Some(sym) => Error::unexpected_key(strand, sym),
            });
        }
        let scalar = to_scalar(strand, &value)?;
        let rc = unsafe { bind_scalar(raw, idx, &scalar) };
        if rc != SQLITE_OK {
            return Err(Error::runtime(strand, "failed to bind parameter"));
        }
        filled[idx as usize - 1] = true;
    }

    if let Some(gap) = filled.iter().position(|f| !f) {
        return Err(unsafe { unfilled_error(strand, raw, gap as c_int + 1) });
    }

    Ok(())
}

/// Names the parameter at `idx`, which nothing filled.
unsafe fn unfilled_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    raw: *mut sqlite3_stmt,
    idx: c_int,
) -> Error<'v, 's> {
    let name = unsafe { sqlite3_bind_parameter_name(raw, idx) };
    let name = if name.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .to_string(),
        )
    };
    match name.as_deref().and_then(|name| name.strip_prefix(':')) {
        Some(name) => match name.strip_prefix("0p").and_then(|i| i.parse::<u32>().ok()) {
            Some(position) => Error::missing_positional(strand, position as usize),
            None => Error::missing_key(strand, name),
        },
        None => Error::runtime(strand, "a SQL parameter went unbound"),
    }
}
