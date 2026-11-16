use std::ops::ControlFlow;

use crate::{
    gc::arena::Visit,
    sym::Sym,
    value::{Input, Slot, Value, private},
    vm::{Alloc, AllocExt},
};
use dolang_bytecode::{Rest, Variadic};

pub(crate) enum UnpackKeyKind<'v, 'a> {
    Sym(Sym<'v, 'a>),
    Const(Value<'v>),
}

impl<'v, 'a> Input<'v> for &UnpackKeyKind<'v, 'a> {
    fn input_take<'b>(
        &'b mut self,
        alloc: &mut dyn Alloc<'v>,
        mut out: Slot<'v, '_>,
        _: private::Sealed,
    ) -> Option<Slot<'v, 'b>> {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        // SAFETY: stored into the scanned `out` at once
        alloc.store(&mut out, unsafe {
            match self {
                UnpackKeyKind::Sym(sym) => Value::from_object(vm.sym_obj(*sym)),
                UnpackKeyKind::Const(value) => value.dup(),
            }
        });
        None
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

    /// Visit the values the signature owns: its optional defaults, constant keys and key
    /// defaults.
    pub(crate) fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for value in &self.optional {
            value.accept(visit)?;
        }
        for key in &self.keys {
            if let UnpackKeyKind::Const(value) = &key.kind {
                value.accept(visit)?;
            }
            if let Some(value) = &key.default {
                value.accept(visit)?;
            }
        }
        ControlFlow::Continue(())
    }

    pub(crate) fn len(&self) -> usize {
        self.required + self.optional.len() + self.keys.len() + self.variadic.captures()
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
