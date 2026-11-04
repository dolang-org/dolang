//! `sqlite.Error` and its subclasses, and the capture of a failed call.

use std::{ffi::CStr, io, marker::PhantomData};

use dolang::runtime::object::fmt;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Strand, object::TypeBuilder, value::fmt::Format,
};
use dolang_ext_shell::ErrorExt as _;
use libsqlite3_sys::{
    SQLITE_BUSY, SQLITE_CANTOPEN, SQLITE_CONSTRAINT, SQLITE_CONSTRAINT_CHECK,
    SQLITE_CONSTRAINT_FOREIGNKEY, SQLITE_CONSTRAINT_NOTNULL, SQLITE_CONSTRAINT_PRIMARYKEY,
    SQLITE_CONSTRAINT_UNIQUE, SQLITE_CORRUPT, SQLITE_ERROR, SQLITE_FULL, SQLITE_IOERR,
    SQLITE_LOCKED, SQLITE_NOTADB, SQLITE_PERM, SQLITE_READONLY, sqlite3, sqlite3_errmsg,
    sqlite3_errstr, sqlite3_extended_errcode, sqlite3_system_errno,
};

use crate::global::Tag;

/// A failed SQLite call, captured on the thread that made it, while the
/// connection's error state still describes it.
pub(crate) struct Failure {
    /// The extended result code.
    code: i32,
    message: String,
    /// The OS error behind an I/O failure, when one can be recovered.
    os: Option<io::Error>,
}

impl Failure {
    /// Captures the failure `rc` that a call on `raw` returned. `raw` may be
    /// null, as after an `sqlite3_open_v2` that could not allocate.
    ///
    /// # Safety
    ///
    /// `raw` is null or a live connection that nothing else is using.
    pub(crate) unsafe fn capture(raw: *mut sqlite3, rc: i32) -> Self {
        let primary = rc & 0xff;
        let (code, message) = if raw.is_null() {
            (rc, unsafe { c_message(sqlite3_errstr(rc)) })
        } else {
            let code = unsafe { sqlite3_extended_errcode(raw) };
            // Not every call records its failure on the connection.
            let code = if code & 0xff == primary { code } else { rc };
            (code, unsafe { c_message(sqlite3_errmsg(raw)) })
        };
        let os = if matches!(
            primary,
            SQLITE_IOERR | SQLITE_CANTOPEN | SQLITE_FULL | SQLITE_READONLY | SQLITE_PERM
        ) {
            #[cfg(unix)]
            let os = crate::vfs::take_last_error(primary);
            #[cfg(not(unix))]
            let os = None;
            os.or_else(|| {
                // SQLite's own VFSes record the OS error for `IOERR` and
                // `CANTOPEN`; the shell VFS does not, and reports zero.
                let errno = if raw.is_null() {
                    0
                } else {
                    unsafe { sqlite3_system_errno(raw) }
                };
                (errno != 0).then(|| io::Error::from_raw_os_error(errno))
            })
        } else {
            None
        };
        Self { code, message, os }
    }

    /// Raises the failure: the OS error behind it as its `sys` class, or else
    /// the `sqlite.Error` subclass its code belongs to.
    pub(crate) fn raise<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        if let Some(os) = self.os {
            return os.into_sys(strand);
        }
        let types = &strand.state::<Tag>().types;
        let annex = ErrorAnnex {
            code: self.code,
            message: self.message,
        };
        match self.code & 0xff {
            SQLITE_BUSY | SQLITE_LOCKED => {
                Error::object_with_annex(strand, types.busy, SqliteError::default(), annex)
            }
            SQLITE_CONSTRAINT => {
                Error::object_with_annex(strand, types.constraint, SqliteError::default(), annex)
            }
            SQLITE_CORRUPT | SQLITE_NOTADB => {
                Error::object_with_annex(strand, types.corrupt, SqliteError::default(), annex)
            }
            _ => Error::object_with_annex(strand, types.error, SqliteError::default(), annex),
        }
    }
}

unsafe fn c_message(msg: *const std::ffi::c_char) -> String {
    if msg.is_null() {
        "<unknown>".into()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The `kind` symbols, indexed by [`kind_index`].
const KINDS: [&str; 17] = [
    "UNIQUE",
    "PRIMARY_KEY",
    "FOREIGN_KEY",
    "NOT_NULL",
    "CHECK",
    "BUSY",
    "LOCKED",
    "CONSTRAINT",
    "CORRUPT",
    "NOTADB",
    "SQL",
    "FULL",
    "READONLY",
    "PERM",
    "CANTOPEN",
    "IO",
    "OTHER",
];

/// The index in [`KINDS`] of the `kind` for an extended result code.
fn kind_index(code: i32) -> usize {
    match code {
        SQLITE_CONSTRAINT_UNIQUE => 0,
        SQLITE_CONSTRAINT_PRIMARYKEY => 1,
        SQLITE_CONSTRAINT_FOREIGNKEY => 2,
        SQLITE_CONSTRAINT_NOTNULL => 3,
        SQLITE_CONSTRAINT_CHECK => 4,
        _ => match code & 0xff {
            SQLITE_BUSY => 5,
            SQLITE_LOCKED => 6,
            SQLITE_CONSTRAINT => 7,
            SQLITE_CORRUPT => 8,
            SQLITE_NOTADB => 9,
            SQLITE_ERROR => 10,
            SQLITE_FULL => 11,
            SQLITE_READONLY => 12,
            SQLITE_PERM => 13,
            SQLITE_CANTOPEN => 14,
            SQLITE_IOERR => 15,
            _ => 16,
        },
    }
}

/// Names one of the `sqlite` error classes.
pub(crate) trait ErrorClass<'v>: 'v {
    const NAME: &'v str;
}

pub(crate) struct Base;
pub(crate) struct Busy;
pub(crate) struct Constraint;
pub(crate) struct Corrupt;

impl<'v> ErrorClass<'v> for Base {
    const NAME: &'v str = "Error";
}

impl<'v> ErrorClass<'v> for Busy {
    const NAME: &'v str = "Busy";
}

impl<'v> ErrorClass<'v> for Constraint {
    const NAME: &'v str = "ConstraintError";
}

impl<'v> ErrorClass<'v> for Corrupt {
    const NAME: &'v str = "CorruptError";
}

/// One of the `sqlite` error classes; they differ only in name and place in
/// the hierarchy.
pub(crate) struct SqliteError<C>(PhantomData<C>);

impl<C> Default for SqliteError<C> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct ErrorAnnex {
    code: i32,
    message: String,
}

impl<'v, C: ErrorClass<'v>> Object<'v> for SqliteError<C> {
    const NAME: &'v str = C::NAME;
    const MODULE: &'v str = "sqlite";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let kinds = KINDS.map(|name| builder.sym(name));
        builder
            .get("kind", move |this, strand, out| {
                Output::set(strand, out, kinds[kind_index(this.annex().code)]);
                Ok(())
            })
            .get("code", |this, strand, out| {
                Output::set(strand, out, this.annex().code);
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<sqlite.{} ", C::NAME)?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}
