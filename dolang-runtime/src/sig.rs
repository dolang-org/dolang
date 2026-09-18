use crate::{
    error::{Error, Result},
    object::record,
    strand::Strand,
    sym::Sym,
    value::{Input, InputBy, Slots, Value, private},
    vm::Vm,
};
use dolang_bytecode::{Rest, Variadic};

pub(crate) enum UnpackKeyKind<'v, 'a> {
    Sym(Sym<'v, 'a>),
    Const(Value<'v>),
}

impl<'v, 'a> Input<'v> for &UnpackKeyKind<'v, 'a> {
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: private::Sealed) -> InputBy<'v, 'b> {
        match self {
            UnpackKeyKind::Sym(sym) => InputBy::Value(Value::from_object(vm.sym_obj(*sym)), None),
            UnpackKeyKind::Const(value) => InputBy::Borrow(value),
        }
    }
}

pub(crate) struct UnpackKey<'v, 'a> {
    pub(crate) kind: UnpackKeyKind<'v, 'a>,
    pub(crate) default: Option<Value<'v>>,
}

pub(crate) struct Unpack<'v, 'a> {
    pub(crate) required: usize,
    pub(crate) optional: Vec<Value<'v>>,
    pub(crate) keys: Vec<UnpackKey<'v, 'a>>,
    pub(crate) sym_index: Vec<(Sym<'v, 'a>, usize)>,
    pub(crate) variadic: Variadic,
}

impl<'v, 'a> Unpack<'v, 'a> {
    pub(crate) fn new(
        required: usize,
        optional: Vec<Value<'v>>,
        keys: Vec<UnpackKey<'v, 'a>>,
        variadic: Variadic,
    ) -> Self {
        let mut sym_index = keys
            .iter()
            .enumerate()
            .filter_map(|(i, k)| {
                if let UnpackKeyKind::Sym(s) = &k.kind {
                    Some((*s, i))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        sym_index.sort_by_key(|(s, _)| *s);
        Self {
            required,
            optional,
            keys,
            sym_index,
            variadic,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.required + self.optional.len() + self.keys.len() + self.variadic.captures()
    }

    /// Returns how leftover positional items are handled.
    pub(crate) fn pos_rest(&self) -> Rest {
        self.variadic.positional()
    }

    fn rest_base(&self) -> usize {
        self.required + self.optional.len() + self.keys.len()
    }

    /// Returns the slot of a rest that captures leftover positional items: `...name` or
    /// `*name`.
    pub(crate) fn pos_rest_slot(&self) -> Option<usize> {
        match self.variadic {
            Variadic::Capture | Variadic::Split(Rest::Capture, _) => Some(self.rest_base()),
            _ => None,
        }
    }

    /// Returns the slot of a `**name` rest.
    pub(crate) fn key_rest_slot(&self) -> Option<usize> {
        match self.variadic {
            Variadic::Split(pos, Rest::Capture) => {
                Some(self.rest_base() + usize::from(pos == Rest::Capture))
            }
            _ => None,
        }
    }

    // TODO(#703): remove once every keyed source handles `*` and `**` rests
    pub(crate) fn reject_split<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        if matches!(self.variadic, Variadic::Split(pos, key) if pos != Rest::None || key != Rest::None)
        {
            return Err(Error::type_error(
                strand,
                "`*` and `**` rests are not yet supported in destructuring",
            ));
        }
        Ok(())
    }

    /// Stores an empty record in a `**name` rest, for sources without keyed items.
    pub(crate) fn fill_empty_key_rest(&self, strand: &mut Strand<'v, '_>, out: &mut Slots<'v, '_>) {
        if let Some(slot) = self.key_rest_slot() {
            out.at(slot)
                .store(Value::from_object(record::empty(strand)));
        }
    }

    pub(crate) fn sym_offset(&self, sym: Sym<'v, '_>) -> Option<usize> {
        self.sym_index
            .binary_search_by_key(&sym, |(s, _)| *s)
            .ok()
            .map(
            |i| unsafe { self.sym_index.get_unchecked(i) }.1 + self.required + self.optional.len(),
        )
    }
}

pub(crate) enum Arg<'v, 'a> {
    Pos,
    Key(Sym<'v, 'a>),
    Expand,
}

pub(crate) enum Pack<'v, 'a> {
    Fixed(Vec<Option<Sym<'v, 'a>>>),
    Var(Vec<Arg<'v, 'a>>),
}

impl<'v, 'a> Pack<'v, 'a> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Pack::Fixed(syms) => syms.len(),
            Pack::Var(args) => args.len(),
        }
    }
}
