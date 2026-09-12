use dolang::runtime::{
    Type,
    vm::{Builder, Stateful},
};

use crate::time::{Calendar, Date, DateTime, Duration, Month, Weekday};

pub(crate) struct Types<'v> {
    pub(crate) date_time: Type<'v, DateTime>,
    pub(crate) duration: Type<'v, Duration>,
    pub(crate) date: Type<'v, Date>,
    pub(crate) month: Type<'v, Month>,
    pub(crate) weekday: Type<'v, Weekday>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) calendar: Calendar<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let calendar = Calendar::new(builder);
        Self {
            types: Types {
                date_time: builder.register_type(),
                duration: builder.register_type(),
                date: calendar.date,
                month: calendar.month,
                weekday: calendar.weekday,
            },
            calendar,
        }
    }
}
