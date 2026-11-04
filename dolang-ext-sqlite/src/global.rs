use dolang::runtime::{
    Type,
    value::TypeObject,
    vm::{Register, StateTag},
};

use crate::sqlite::{
    connection::{Connection, Transaction},
    error::{Base, Busy, Constraint, Corrupt, SqliteError},
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
    pub(crate) error: Type<'v, SqliteError<Base>>,
    pub(crate) busy: Type<'v, SqliteError<Busy>>,
    pub(crate) constraint: Type<'v, SqliteError<Constraint>>,
    pub(crate) corrupt: Type<'v, SqliteError<Corrupt>>,
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
        let error = builder
            .build_type::<SqliteError<Base>>((), ())
            .nominal_supertype(TypeObject::RuntimeError)
            .build();
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
                .build_type::<SqliteError<Busy>>((), ())
                .nominal_supertype(error)
                .build(),
            constraint: builder
                .build_type::<SqliteError<Constraint>>((), ())
                .nominal_supertype(error)
                .build(),
            corrupt: builder
                .build_type::<SqliteError<Corrupt>>((), ())
                .nominal_supertype(error)
                .nominal_supertype(TypeObject::DataError)
                .build(),
        };

        Self { types }
    }
}
