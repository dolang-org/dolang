use crate::{
    sym::Sym,
    value::{Input, InputBy, Value, private},
    vm::Vm,
};
use dolang_bytecode::Variadic;

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
        self.required
            + self.optional.len()
            + self.keys.len()
            + match self.variadic {
                Variadic::None | Variadic::Discard => 0,
                Variadic::Capture => 1,
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
