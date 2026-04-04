use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    mem,
    num::NonZero,
    ops::Deref,
};

use crate::{
    gc::{BoxWeak, arena::Arena},
    object::{
        protocol::{GcObj, Header, TypeHandle},
        sym::SymObj,
    },
    value::{self, Input, InputBy, Value},
    vm::Vm,
};

pub(crate) type Tag = NonZero<u64>;

macro_rules! well_known_symbols {
    {
        $(
            ($variant:ident, $string:literal, $const:ident),
        )*
    } => {
        #[repr(u64)]
        #[derive(Clone, Copy)]
        pub(crate) enum WellKnown {
            _INVALID = 0,
            $(
                $variant,
            )*
            _COUNT,
        }

        $(
            pub(crate) const $const: Tag = NonZero::new(WellKnown::$variant as u64).unwrap();
        )*

        const WELL_KNOWN: [&str; WellKnown::_COUNT as usize] = [
            "<invalid>",
            $(
                $string,
            )*
        ];
    };
}

well_known_symbols! {
    (ArgMethod, "(arg)", ARG_METHOD),
    (All, "all", ALL),
    (Any, "any", ANY),
    (Add, "add", ADD),
    (Backtrace, "backtrace", BACKTRACE),
    (Cancel, "cancel", CANCEL),
    (CallMethod, "(call)", CALL_METHOD),
    (Chain, "chain", CHAIN),
    (Clear, "clear", CLEAR),
    (Close, "close", CLOSE),
    (Count, "count", COUNT),
    (Contains, "contains", CONTAINS),
    (DbgMethod, "(dbg)", DBG_METHOD),
    (Default, "default", DEFAULT),
    (Delete, "delete", DELETE),
    (Dict, "dict", DICT),
    (Diff, "diff", DIFF),
    (Done, "done", DONE),
    (Else, "else", ELSE),
    (End, "end", END),
    (EndsWith, "ends_with", ENDS_WITH),
    (Get, "get", GET),
    (Hex, "hex", HEX),
    (Intersect, "intersect", INTERSECT),
    (Iter, "iter", ITER),
    (IterMethod, "(iter)", ITER_METHOD),
    (Insert, "insert", INSERT),
    (InitMethod, "(init)", INIT_METHOD),
    (Int, "int", INT),
    (ItemError, "ItemError", ITEM_ERROR),
    (Join, "join", JOIN),
    (Key, "key", KEY),
    (Keys, "keys", KEYS),
    (Len, "len", LEN),
    (Limit, "limit", LIMIT),
    (Line, "line", LINE),
    (Map, "map", MAP),
    (Max, "max", MAX),
    (Min, "min", MIN),
    (Module, "module", MODULE),
    (Next, "next", NEXT),
    (NextMethod, "(next)", NEXT_METHOD),
    (Values, "values", VALUES),
    (SinkMethod, "(sink)", SINK_METHOD),
    (Sink, "sink", SINK),
    (Pack, "pack", PACK),
    (Pairs, "pairs", PAIRS),
    (Pos, "pos", POS),
    (PosKeys, "pos_keys", POS_KEYS),
    (Pop, "pop", POP),
    (Put, "put", PUT),
    (PutMethod, "(put)", PUT_METHOD),
    (Push, "push", PUSH),
    (Replace, "replace", REPLACE),
    (Record, "record", RECORD),
    (Repeat, "repeat", REPEAT),
    (Receiver, "receiver", RECEIVER),
    (Rsplit, "rsplit", RSPLIT),
    (Reverse, "reverse", REVERSE),
    (Set, "set", SET),
    (Split, "split", SPLIT),
    (SpreadMethod, "(spread)", SPREAD_METHOD),
    (Start, "start", START),
    (StartsWith, "starts_with", STARTS_WITH),
    (Step, "step", STEP),
    (StrMethod, "(str)", STR_METHOD),
    (Sub, "sub", SUB),
    (IsSubset, "is_subset", IS_SUBSET),
    (IsSuperset, "is_superset", IS_SUPERSET),
    (Source, "source", SOURCE),
    (Sort, "sort", SORT),
    (SymDiff, "sym_diff", SYM_DIFF),
    (TrimEnd, "trim_end", TRIM_END),
    (TrimStart, "trim_start", TRIM_START),
    (Trim, "trim", TRIM),
    (Filter, "filter", FILTER),
    (Fold, "fold", FOLD),
    (Method, "method", METHOD),
    (Union, "union", UNION),
    (Unpack, "unpack", UNPACK),
    (UnpackMethod, "(unpack)", UNPACK_METHOD),
    (Upper, "upper", UPPER),
    (Lower, "lower", LOWER),
    (Wait, "wait", WAIT),
    (WithoutPrefix, "without_prefix", WITHOUT_PREFIX),
    (WithoutSuffix, "without_suffix", WITHOUT_SUFFIX),
    (Zip, "zip", ZIP),
    (AddMethod, "(add)", ADD_METHOD),
    (SubMethod, "(sub)", SUB_METHOD),
    (RsubMethod, "(rsub)", RSUB_METHOD),
    (MulMethod, "(mul)", MUL_METHOD),
    (DivMethod, "(div)", DIV_METHOD),
    (RdivMethod, "(rdiv)", RDIV_METHOD),
    (EdivMethod, "(ediv)", EDIV_METHOD),
    (RedivMethod, "(rediv)", REDIV_METHOD),
    (ModMethod, "(mod)", MOD_METHOD),
    (RmodMethod, "(rmod)", RMOD_METHOD),
    (BandMethod, "(band)", BAND_METHOD),
    (BorMethod, "(bor)", BOR_METHOD),
    (BxorMethod, "(bxor)", BXOR_METHOD),
    (NegMethod, "(neg)", NEG_METHOD),
    (BnotMethod, "(bnot)", BNOT_METHOD),
    (EqMethod, "(eq)", EQ_METHOD),
    (LtMethod, "(lt)", LT_METHOD),
    (BoolMethod, "(bool)", BOOL_METHOD),
    (IndexMethod, "(index)", INDEX_METHOD),
    (AssignMethod, "(assign)", ASSIGN_METHOD),
    (GetMethod, "(get)", GET_METHOD),
    (SetMethod, "(set)", SET_METHOD),
    (HashMethod, "(hash)", HASH_METHOD),
}

/// A symbol
///
/// A per-VM globally unique identifier with an associated
/// name (a string).  Symbols are used to represent:
///
/// - Keys in key/value arguments
/// - Field names in struct-like objects, including exported items in modules
///
/// # Lifetimes
/// - `'v`: VM brand, binding this object to a particular VM
/// - `'a`: scope for which the symbol is valid, which may be shorter than the
///   lifetime of the VM (e.g. until the last code module referencing it is unloaded)
///
/// # Symbol objects
///
/// Allocated symbols are backed by an object which is sometimes exposed as a
/// [`Value`], such as when iterating the contents of a module, or instantiating
/// a symbol dynamically with the `sym` prelude function.  Such an object can
/// be downcast to this type with [`Value::as_sym`].  This type also implements
/// [`Input`], which implicitly converts it into the underlying object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sym<'v, 'a>(Tag, PhantomData<(&'v mut &'v (), &'a ())>);

impl<'v, 'a> Sym<'v, 'a> {
    pub(crate) unsafe fn into_static_scope_unchecked(self) -> Sym<'v, 'static> {
        Sym(self.0, PhantomData)
    }

    pub(crate) unsafe fn from_tag(tag: Tag) -> Self {
        Self(tag, PhantomData)
    }

    pub(crate) unsafe fn from_obj(obj: &GcObj<'v, SymObj>) -> Self {
        Self(obj.tag, PhantomData)
    }

    pub(crate) fn tag(self) -> Tag {
        self.0
    }

    pub(crate) const fn well_known(tag: Tag) -> Self {
        if tag.get() >= WellKnown::_COUNT as u64 {
            panic!("invalid well_known")
        }
        Self(tag, PhantomData)
    }

    /// Get name of symbol
    #[inline]
    pub fn as_str(self, vm: &Vm<'v>) -> &'a str {
        vm.name_for_sym(self)
    }
}

impl<'v, 'a> Input<'v> for Sym<'v, 'a> {
    #[expect(private_interfaces)]
    fn input_take<'b>(&'b mut self, vm: &'b Vm<'v>, _: value::private::Sealed) -> InputBy<'v, 'b> {
        InputBy::Value(Value::from_object(vm.sym_obj(*self)), None)
    }
}

pub(crate) struct Table<'v> {
    next: Cell<Tag>,
    by_name: RefCell<HashMap<String, BoxWeak<'v, Header, SymObj>>>,
    by_id: RefCell<HashMap<Tag, BoxWeak<'v, Header, SymObj>>>,
}

impl<'v> Table<'v> {
    pub(crate) fn new(arena: &'v Arena<'v>, vtbl: TypeHandle<'v, SymObj>) -> Self {
        let this = Self {
            next: Cell::new(const { NonZero::new(1).unwrap() }),
            by_name: Default::default(),
            by_id: Default::default(),
        };

        for name in WELL_KNOWN[1..].iter() {
            mem::forget(this.register(arena, vtbl, name));
        }

        this
    }

    pub(crate) fn clear(&mut self) {
        if !self.by_id.get_mut().is_empty() {
            for i in 1..WELL_KNOWN.len() {
                unsafe {
                    self.unregister(NonZero::new_unchecked(i as u64));
                }
            }
        }
        self.by_name.get_mut().clear();
        self.by_id.get_mut().clear();
    }

    pub(crate) fn register(
        &self,
        arena: &Arena<'v>,
        vtbl: TypeHandle<'v, SymObj>,
        name: &str,
    ) -> GcObj<'v, SymObj> {
        if let Some(wr) = self.by_name.borrow().get(name)
            && let Some(sr) = wr.upgrade()
        {
            return sr;
        }
        let next = self.next.get();
        self.next
            .set(next.checked_add(1).expect("are you kidding?"));
        let obj = GcObj::new(
            arena,
            vtbl,
            SymObj {
                tag: next,
                name: name.to_owned(),
            },
        );
        self.by_name
            .borrow_mut()
            .insert(name.to_owned(), GcObj::downgrade(&obj));
        self.by_id.borrow_mut().insert(next, GcObj::downgrade(&obj));
        obj
    }

    unsafe fn unregister(&self, tag: Tag) {
        let obj = unsafe { GcObj::from_weak(self.by_id.borrow().get(&tag).unwrap()) };
        mem::drop(obj)
    }

    pub(crate) fn name<'a>(&self, sym: Sym<'v, 'a>) -> &'a str {
        let borrow = self.by_id.borrow();
        let weak = borrow.get(&sym.0).unwrap();
        unsafe {
            let inner = weak.get_unchecked();
            mem::transmute(inner.name.deref())
        }
    }

    pub(crate) fn obj(&self, sym: Sym<'v, '_>) -> GcObj<'v, SymObj> {
        BoxWeak::upgrade(&self.by_id.borrow()[&sym.0]).unwrap()
    }

    /// Register a symbol without inserting into `by_name`, ensuring
    /// it remains globally unique even if another module uses the same name.
    /// Used for private-field symbols loaded from bytecode (`#`-prefixed names).
    pub(crate) fn register_unique(
        &self,
        arena: &Arena<'v>,
        vtbl: TypeHandle<'v, SymObj>,
        name: &str,
    ) -> GcObj<'v, SymObj> {
        let next = self.next.get();
        self.next
            .set(next.checked_add(1).expect("are you kidding?"));
        let obj = GcObj::new(
            arena,
            vtbl,
            SymObj {
                tag: next,
                name: name.to_owned(),
            },
        );
        // Insert into by_id only — NOT by_name — to preserve uniqueness across modules
        self.by_id.borrow_mut().insert(next, GcObj::downgrade(&obj));
        obj
    }

    pub(crate) fn gc(&self) {
        self.by_name
            .borrow_mut()
            .retain(|_, v| v.strong_count() != 0);
        self.by_id.borrow_mut().retain(|_, v| v.strong_count() != 0);
    }
}

impl<'v> Drop for Table<'v> {
    fn drop(&mut self) {
        self.clear()
    }
}
