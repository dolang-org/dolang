use dolang::runtime::{
    Sym, Type,
    vm::{Register, StateTag},
};

use crate::{
    time::{Calendar, Date, DateTime, Duration, Month, Weekday},
    zone::{TimeZone, Zoned},
};

pub(crate) struct Types<'v> {
    pub(crate) date_time: Type<'v, DateTime>,
    pub(crate) duration: Type<'v, Duration>,
    pub(crate) date: Type<'v, Date>,
    pub(crate) month: Type<'v, Month>,
    pub(crate) weekday: Type<'v, Weekday>,
    pub(crate) time_zone: Type<'v, TimeZone>,
    pub(crate) zoned: Type<'v, Zoned>,
}

/// The `disambiguate:` choices of `TimeZone.datetime`.
pub(crate) struct Syms<'v> {
    pub(crate) compatible: Sym<'v, 'v>,
    pub(crate) earlier: Sym<'v, 'v>,
    pub(crate) later: Sym<'v, 'v>,
    pub(crate) reject: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) calendar: Calendar<'v>,
    pub(crate) syms: Syms<'v>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        let calendar = Calendar::new(builder);
        Self {
            types: Types {
                date_time: builder.register_type(),
                duration: builder.register_type(),
                date: calendar.date,
                month: calendar.month,
                weekday: calendar.weekday,
                time_zone: builder.register_type(),
                zoned: builder.register_type(),
            },
            calendar,
            syms: Syms {
                compatible: builder.sym("COMPATIBLE"),
                earlier: builder.sym("EARLIER"),
                later: builder.sym("LATER"),
                reject: builder.sym("REJECT"),
            },
        }
    }
}
