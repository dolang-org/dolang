use dolang::runtime::{
    Type,
    vm::{Register, StateTag},
};

use crate::sqlite::{
    SqliteBusy, SqliteError,
    connection::{Connection, Transaction},
    row::{Row, RowRest, RowRestIter, Rows},
    statement::Statement,
};

pub(crate) struct Types<'v> {
    pub(crate) connection: Type<'v, Connection>,
    pub(crate) statement: Type<'v, Statement>,
    pub(crate) rows: Type<'v, Rows>,
    pub(crate) row: Type<'v, Row>,
    pub(crate) row_rest: Type<'v, RowRest>,
    pub(crate) row_rest_iter: Type<'v, RowRestIter>,
    pub(crate) transaction: Type<'v, Transaction>,
    pub(crate) error: Type<'v, SqliteError>,
    pub(crate) busy: Type<'v, SqliteBusy>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        let error = builder.register_type::<SqliteError>();
        let types = Types {
            connection: builder.register_type(),
            statement: builder.register_type(),
            rows: builder.register_type(),
            row: builder.register_type(),
            row_rest: builder.register_type(),
            row_rest_iter: builder.register_type(),
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
