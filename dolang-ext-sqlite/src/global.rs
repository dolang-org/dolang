use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::sqlite::{
    SqliteBusy, SqliteError,
    connection::{Connection, Transaction},
    row::{Row, RowIter, Rows},
    statement::Statement,
};

pub(crate) struct Types<'v> {
    pub(crate) connection: Type<'v, Connection>,
    pub(crate) statement: Type<'v, Statement>,
    pub(crate) rows: Type<'v, Rows>,
    pub(crate) row: Type<'v, Row>,
    pub(crate) row_iter: Type<'v, RowIter>,
    pub(crate) transaction: Type<'v, Transaction>,
    pub(crate) error: Type<'v, SqliteError>,
    pub(crate) busy: Type<'v, SqliteBusy>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let error = builder.register_type::<SqliteError>();
        let types = Types {
            connection: builder.register_type(),
            statement: builder.register_type(),
            rows: builder.register_type(),
            row: builder.register_type(),
            row_iter: builder.register_type(),
            transaction: builder.register_type(),
            error,
            busy: builder
                .build_type::<SqliteBusy>((), ())
                .nominal_supertype(error)
                .build(),
        };

        Self { types }
    }
}
