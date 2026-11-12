//! The checker's types as a typelib writes them, for what it records that its
//! surface can't: what a public declaration's decorators settled to.
//!
//! A type is written node by node, as the database holds it, rather than as a type
//! expression elaboration would read again. Nodes are in one table, each distinct
//! node once, after its children. A binder reference keeps its depth and slot. A
//! declaration is named by its module and the name the module exports it by, never
//! by an ID, so that it's found again in a check of other units; one that doesn't
//! resolve, or resolves to a declaration of another kind, reads as unknown, as a
//! missing import's interface does.
//!
//! A table holds its own strings, by its own IDs. Writing it into a typelib, and
//! reading it from one, renumbers them to and from the typelib's string table.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::typeck::r#type::{
    Argument, Binder, Binding, BoundRef, Database, DeclId, DeclKind, Element, FieldAccess,
    Function, Kind, Literal, Multiplicity, Rest, Restriction, SchemaItem, SettledMember, Type,
    TypeId, UnionMember, Variance,
};

/// What public decorated declarations settled to: each declaration's form and the
/// node of its type in `table`
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct Settled {
    pub(crate) table: Table,
    pub(crate) entries: Vec<(DeclId, Form, u32)>,
}

/// A form of [`SettledMember`], without its type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Form {
    Method,
    Value,
    Declared,
}

impl Form {
    pub(crate) fn of(member: SettledMember) -> (Self, TypeId) {
        match member {
            SettledMember::Method(ty) => (Self::Method, ty),
            SettledMember::Value(ty) => (Self::Value, ty),
            SettledMember::Declared(ty) => (Self::Declared, ty),
        }
    }

    pub(crate) fn member(self, ty: TypeId) -> SettledMember {
        match self {
            Self::Method => SettledMember::Method(ty),
            Self::Value => SettledMember::Value(ty),
            Self::Declared => SettledMember::Declared(ty),
        }
    }
}

/// Type nodes, each after its children, and the strings they name
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct Table {
    pub(crate) types: Vec<Ty>,
    pub(crate) strings: Vec<Box<str>>,
}

/// A type node, whose children are nodes before it, and whose strings are its
/// table's
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Ty {
    Top,
    Unknown(WireKind),
    Literal(Lit),
    Fresh(Lit),
    Decl(Named),
    Meta(Named),
    Bound {
        depth: u16,
        slot: u16,
        kind: WireKind,
    },
    Apply {
        base: u32,
        args: Vec<Arg>,
        kind: WireKind,
    },
    Union(Vec<Member>),
    Function {
        params: u32,
        result: u32,
        input: Option<u32>,
        output: Option<u32>,
    },
    Schema(Vec<Item>),
    Quantified {
        binders: Vec<WireBinder>,
        body: u32,
    },
    Map {
        packs: Vec<u32>,
        pattern: u32,
    },
    Restricted {
        schema: u32,
        /// The key of a `Deletable` restriction, else `Clearable`
        deletable: Option<u32>,
    },
    Fields {
        target: u32,
        write: bool,
    },
    Overloaded {
        overloads: Vec<u32>,
        implementation: Option<u32>,
        function: Option<Named>,
    },
}

/// A declaration, by its module and the name the module exports it by, with what
/// it was, which what it resolves to must still be
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Named {
    module: u32,
    item: u32,
    kind: WireDeclKind,
    result: WireKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum WireKind {
    Type,
    Schema,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum WireDeclKind {
    Class,
    Protocol,
    OpaqueAlias,
    Alias,
    Function,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Lit {
    Nil,
    Bool(bool),
    Int(i128),
    Str(u32),
    Sym(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Arg {
    Pos(u32),
    Key(u32, u32),
    Expand(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Member {
    Type(u32),
    Expand(u32),
    Keys(u32),
    Values(u32),
    Entries(u32),
    IndexItem(u32, u32),
    AssignItem(u32, u32),
    InsertItem(u32, u32),
    PushItem(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Item {
    multiplicity: WireMultiplicity,
    element: Elem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum WireMultiplicity {
    Required,
    Optional,
    Repeated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Elem {
    Pos(u32),
    Keyed { key: u32, value: u32 },
    Include(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct WireBinder {
    kind: WireKind,
    binding: WireBinding,
    bound: Option<u32>,
    default: Option<u32>,
    variance: WireVariance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum WireBinding {
    Pos,
    Key(u32),
    Rest(WireRest),
    Implicit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum WireRest {
    All,
    Pos,
    Key,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum WireVariance {
    Invariant,
    Covariant,
    Contravariant,
}

impl Ty {
    /// The same node, each string mapped by `f`
    pub(crate) fn map_strings<E>(
        &self,
        mut f: impl FnMut(u32) -> Result<u32, E>,
    ) -> Result<Self, E> {
        let mut node = self.clone();
        let named = |named: &mut Named, f: &mut dyn FnMut(u32) -> Result<u32, E>| {
            named.module = f(named.module)?;
            named.item = f(named.item)?;
            Ok(())
        };
        match &mut node {
            Ty::Literal(Lit::Str(text) | Lit::Sym(text))
            | Ty::Fresh(Lit::Str(text) | Lit::Sym(text)) => *text = f(*text)?,
            Ty::Decl(decl) | Ty::Meta(decl) => named(decl, &mut f)?,
            Ty::Overloaded {
                function: Some(function),
                ..
            } => named(function, &mut f)?,
            Ty::Apply { args, .. } => {
                for arg in args {
                    if let Arg::Key(name, _) = arg {
                        *name = f(*name)?;
                    }
                }
            }
            Ty::Quantified { binders, .. } => {
                for binder in binders {
                    if let WireBinding::Key(name) = &mut binder.binding {
                        *name = f(*name)?;
                    }
                }
            }
            _ => {}
        }
        Ok(node)
    }

    /// Visit each child node
    fn children(&self, mut visit: impl FnMut(u32)) {
        match self {
            Ty::Top
            | Ty::Unknown(_)
            | Ty::Literal(_)
            | Ty::Fresh(_)
            | Ty::Decl(_)
            | Ty::Meta(_)
            | Ty::Bound { .. } => {}
            Ty::Apply { base, args, .. } => {
                visit(*base);
                for arg in args {
                    let (Arg::Pos(ty) | Arg::Key(_, ty) | Arg::Expand(ty)) = *arg;
                    visit(ty);
                }
            }
            Ty::Union(members) => {
                for member in members {
                    match *member {
                        Member::Type(ty)
                        | Member::Expand(ty)
                        | Member::Keys(ty)
                        | Member::Values(ty)
                        | Member::Entries(ty)
                        | Member::PushItem(ty) => visit(ty),
                        Member::IndexItem(ty, key)
                        | Member::AssignItem(ty, key)
                        | Member::InsertItem(ty, key) => {
                            visit(ty);
                            visit(key);
                        }
                    }
                }
            }
            Ty::Function {
                params,
                result,
                input,
                output,
            } => {
                visit(*params);
                visit(*result);
                input.iter().chain(output).copied().for_each(visit);
            }
            Ty::Schema(items) => {
                for item in items {
                    match item.element {
                        Elem::Pos(ty) | Elem::Include(ty) => visit(ty),
                        Elem::Keyed { key, value } => {
                            visit(key);
                            visit(value);
                        }
                    }
                }
            }
            Ty::Quantified { binders, body } => {
                for binder in binders {
                    binder
                        .bound
                        .iter()
                        .chain(&binder.default)
                        .copied()
                        .for_each(&mut visit);
                }
                visit(*body);
            }
            Ty::Map { packs, pattern } => {
                packs.iter().copied().for_each(&mut visit);
                visit(*pattern);
            }
            Ty::Restricted { schema, deletable } => {
                visit(*schema);
                deletable.iter().copied().for_each(visit);
            }
            Ty::Fields { target, .. } => visit(*target),
            Ty::Overloaded {
                overloads,
                implementation,
                ..
            } => {
                overloads
                    .iter()
                    .chain(implementation)
                    .copied()
                    .for_each(visit);
            }
        }
    }
}

impl Table {
    /// A table of `types` read from a typelib, whose strings are `strings`, checked
    /// to be one: each child before its parent, and each string in range. It keeps
    /// only the strings it names, numbered as it first names them.
    pub(crate) fn read<S: AsRef<str>>(types: Vec<Ty>, strings: &[S]) -> Result<Self, &'static str> {
        let mut table = Table::default();
        let mut ids: FxHashMap<u32, u32> = FxHashMap::default();
        for (index, ty) in types.iter().enumerate() {
            let mut ordered = true;
            ty.children(|child| ordered &= (child as usize) < index);
            if !ordered {
                return Err("a type node is before its children");
            }
            let node = ty.map_strings(|id| {
                let text = strings
                    .get(id as usize)
                    .ok_or("a string ID is out of range")?;
                let next = table.strings.len() as u32;
                let local = *ids.entry(id).or_insert(next);
                if local == next {
                    table.strings.push(text.as_ref().into());
                }
                Ok(local)
            })?;
            table.types.push(node);
        }
        Ok(table)
    }

    /// Whether `node` is a node of the table
    pub(crate) fn has(&self, node: u32) -> bool {
        (node as usize) < self.types.len()
    }

    /// Intern every node in `db`, giving each one's type by its index. A name is
    /// resolved by `resolve`, from a module's name and an item's.
    pub(crate) fn intern(
        &self,
        db: &Database,
        resolve: impl Fn(&str, &str) -> Option<DeclId>,
    ) -> Vec<TypeId> {
        let mut ids: Vec<TypeId> = Vec::with_capacity(self.types.len());
        let text = |id: u32| &*self.strings[id as usize];
        for node in &self.types {
            let at = |id: u32| ids[id as usize];
            let found = |named: &Named| {
                let decl = resolve(text(named.module), text(named.item))?;
                let source = &db.declaration(decl).source;
                (decl_kind(source.kind) == Some(named.kind)
                    && kind(source.result_kind) == named.result)
                    .then_some(decl)
            };
            let ty = match node {
                Ty::Top => Type::Top,
                Ty::Unknown(of) => Type::Unknown(unkind(*of)),
                Ty::Literal(lit) => Type::Literal(unliteral(db, lit, text)),
                Ty::Fresh(lit) => Type::Fresh(unliteral(db, lit, text)),
                Ty::Decl(named) => match found(named) {
                    Some(decl) => Type::Decl(decl),
                    None => Type::Unknown(unkind(named.result)),
                },
                Ty::Meta(named) => match found(named) {
                    Some(decl) => Type::Meta(decl),
                    None => Type::Unknown(Kind::Type),
                },
                &Ty::Bound { depth, slot, kind } => Type::Bound {
                    reference: BoundRef { depth, slot },
                    kind: unkind(kind),
                },
                Ty::Apply { base, args, kind } => Type::Apply {
                    base: at(*base),
                    args: (args.iter())
                        .map(|arg| match *arg {
                            Arg::Pos(ty) => Argument::Positional(at(ty)),
                            Arg::Key(name, ty) => {
                                Argument::Keyword(db.intern_symbol(text(name)), at(ty))
                            }
                            Arg::Expand(ty) => Argument::Expand(at(ty)),
                        })
                        .collect(),
                    kind: unkind(*kind),
                },
                Ty::Union(members) => Type::Union(
                    (members.iter())
                        .map(|member| match *member {
                            Member::Type(ty) => UnionMember::Type(at(ty)),
                            Member::Expand(ty) => UnionMember::Expand(at(ty)),
                            Member::Keys(ty) => UnionMember::Keys(at(ty)),
                            Member::Values(ty) => UnionMember::Values(at(ty)),
                            Member::Entries(ty) => UnionMember::Entries(at(ty)),
                            Member::PushItem(ty) => UnionMember::PushItem(at(ty)),
                            Member::IndexItem(ty, key) => UnionMember::IndexItem(at(ty), at(key)),
                            Member::AssignItem(ty, key) => UnionMember::AssignItem(at(ty), at(key)),
                            Member::InsertItem(ty, key) => UnionMember::InsertItem(at(ty), at(key)),
                        })
                        .collect(),
                ),
                Ty::Function {
                    params,
                    result,
                    input,
                    output,
                } => Type::Function(Function {
                    params: at(*params),
                    result: at(*result),
                    input: input.map(at),
                    output: output.map(at),
                }),
                Ty::Schema(items) => Type::Schema(
                    (items.iter())
                        .map(|item| SchemaItem {
                            multiplicity: unmultiplicity(item.multiplicity),
                            element: match item.element {
                                Elem::Pos(ty) => Element::Positional(at(ty)),
                                Elem::Keyed { key, value } => Element::Keyed {
                                    key: at(key),
                                    value: at(value),
                                },
                                Elem::Include(ty) => Element::Include(at(ty)),
                            },
                        })
                        .collect(),
                ),
                Ty::Quantified { binders, body } => Type::Quantified {
                    binders: (binders.iter())
                        .map(|binder| Binder {
                            kind: unkind(binder.kind),
                            binding: match binder.binding {
                                WireBinding::Pos => Binding::Positional,
                                WireBinding::Key(name) => {
                                    Binding::Keyword(db.intern_symbol(text(name)))
                                }
                                WireBinding::Rest(rest) => Binding::Rest(match rest {
                                    WireRest::All => Rest::All,
                                    WireRest::Pos => Rest::Positional,
                                    WireRest::Key => Rest::Keyed,
                                }),
                                WireBinding::Implicit => Binding::Implicit,
                            },
                            bound: binder.bound.map(at),
                            default: binder.default.map(at),
                            variance: match binder.variance {
                                WireVariance::Invariant => Variance::Invariant,
                                WireVariance::Covariant => Variance::Covariant,
                                WireVariance::Contravariant => Variance::Contravariant,
                            },
                        })
                        .collect(),
                    body: at(*body),
                },
                Ty::Map { packs, pattern } => Type::Map {
                    packs: packs.iter().copied().map(at).collect(),
                    pattern: at(*pattern),
                },
                Ty::Restricted { schema, deletable } => Type::Restricted {
                    schema: at(*schema),
                    restriction: match deletable {
                        Some(key) => Restriction::Deletable(at(*key)),
                        None => Restriction::Clearable,
                    },
                },
                Ty::Fields { target, write } => Type::Fields {
                    target: at(*target),
                    access: match write {
                        true => FieldAccess::Write,
                        false => FieldAccess::Read,
                    },
                },
                Ty::Overloaded {
                    overloads,
                    implementation,
                    function,
                } => Type::Overloaded {
                    overloads: overloads.iter().copied().map(at).collect(),
                    implementation: implementation.map(at),
                    function: function.as_ref().and_then(found),
                },
            };
            ids.push(db.intern(ty));
        }
        ids
    }
}

/// Writes types of a database into a table. A declaration is named as `names`
/// gives it, by its module and the name the module exports it by.
pub(crate) struct Encode<'a> {
    db: &'a Database,
    names: &'a FxHashMap<DeclId, (&'a str, &'a str)>,
    table: Table,
    memo: FxHashMap<TypeId, u32>,
    nodes: FxHashMap<Ty, u32>,
    texts: FxHashMap<Box<str>, u32>,
}

impl<'a> Encode<'a> {
    pub(crate) fn new(db: &'a Database, names: &'a FxHashMap<DeclId, (&'a str, &'a str)>) -> Self {
        Self {
            db,
            names,
            table: Table::default(),
            memo: FxHashMap::default(),
            nodes: FxHashMap::default(),
            texts: FxHashMap::default(),
        }
    }

    pub(crate) fn finish(self) -> Table {
        self.table
    }

    /// The node of a type, which must be one a typelib can write: it has no rigid
    /// or stand-in, and it names only declarations `names` names, but for an
    /// overloaded function's, which is written as its signatures instead
    pub(crate) fn ty(&mut self, ty: TypeId) -> u32 {
        if let Some(&node) = self.memo.get(&ty) {
            return node;
        }
        let db = self.db;
        let node = match db.ty(ty) {
            Type::Top => Ty::Top,
            Type::Unknown(of) => Ty::Unknown(kind(*of)),
            Type::Unsupported { .. } | Type::Rigid { .. } => {
                unreachable!("a type a typelib can write has no rigid or stand-in")
            }
            Type::Literal(lit) => Ty::Literal(self.literal(lit)),
            Type::Fresh(lit) => Ty::Fresh(self.literal(lit)),
            &Type::Decl(decl) => match self.named(decl) {
                Some(named) => Ty::Decl(named),
                None if db.declaration(decl).source.kind == DeclKind::Function => {
                    let overloads = (db.overloads(decl).iter())
                        .map(|&signature| self.ty(db.declaration(signature).ty))
                        .collect();
                    let implementation = (db.implementation(decl))
                        .map(|signature| self.ty(db.declaration(signature).ty));
                    Ty::Overloaded {
                        overloads,
                        implementation,
                        function: None,
                    }
                }
                None => unreachable!("a type a typelib can write names what's exported"),
            },
            &Type::Meta(decl) => Ty::Meta(
                self.named(decl)
                    .expect("a class object written is exported"),
            ),
            &Type::Bound {
                reference,
                kind: of,
            } => Ty::Bound {
                depth: reference.depth,
                slot: reference.slot,
                kind: kind(of),
            },
            Type::Apply {
                base,
                args,
                kind: of,
            } => Ty::Apply {
                base: self.ty(*base),
                args: (args.iter())
                    .map(|arg| match *arg {
                        Argument::Positional(ty) => Arg::Pos(self.ty(ty)),
                        Argument::Keyword(name, ty) => {
                            Arg::Key(self.text(db.symbol(name)), self.ty(ty))
                        }
                        Argument::Expand(ty) => Arg::Expand(self.ty(ty)),
                    })
                    .collect(),
                kind: kind(*of),
            },
            Type::Union(members) => Ty::Union(
                (members.iter())
                    .map(|member| match *member {
                        UnionMember::Type(ty) => Member::Type(self.ty(ty)),
                        UnionMember::Expand(ty) => Member::Expand(self.ty(ty)),
                        UnionMember::Keys(ty) => Member::Keys(self.ty(ty)),
                        UnionMember::Values(ty) => Member::Values(self.ty(ty)),
                        UnionMember::Entries(ty) => Member::Entries(self.ty(ty)),
                        UnionMember::PushItem(ty) => Member::PushItem(self.ty(ty)),
                        UnionMember::IndexItem(ty, key) => {
                            Member::IndexItem(self.ty(ty), self.ty(key))
                        }
                        UnionMember::AssignItem(ty, key) => {
                            Member::AssignItem(self.ty(ty), self.ty(key))
                        }
                        UnionMember::InsertItem(ty, key) => {
                            Member::InsertItem(self.ty(ty), self.ty(key))
                        }
                    })
                    .collect(),
            ),
            Type::Function(func) => Ty::Function {
                params: self.ty(func.params),
                result: self.ty(func.result),
                input: func.input.map(|ty| self.ty(ty)),
                output: func.output.map(|ty| self.ty(ty)),
            },
            Type::Schema(items) => Ty::Schema(
                (items.iter())
                    .map(|item| Item {
                        multiplicity: multiplicity(item.multiplicity),
                        element: match item.element {
                            Element::Positional(ty) => Elem::Pos(self.ty(ty)),
                            Element::Keyed { key, value } => Elem::Keyed {
                                key: self.ty(key),
                                value: self.ty(value),
                            },
                            Element::Include(ty) => Elem::Include(self.ty(ty)),
                        },
                    })
                    .collect(),
            ),
            Type::Quantified { binders, body } => Ty::Quantified {
                binders: (binders.iter())
                    .map(|binder| WireBinder {
                        kind: kind(binder.kind),
                        binding: match binder.binding {
                            Binding::Positional => WireBinding::Pos,
                            Binding::Keyword(name) => WireBinding::Key(self.text(db.symbol(name))),
                            Binding::Rest(rest) => WireBinding::Rest(match rest {
                                Rest::All => WireRest::All,
                                Rest::Positional => WireRest::Pos,
                                Rest::Keyed => WireRest::Key,
                            }),
                            Binding::Implicit => WireBinding::Implicit,
                        },
                        bound: binder.bound.map(|ty| self.ty(ty)),
                        default: binder.default.map(|ty| self.ty(ty)),
                        variance: match binder.variance {
                            Variance::Invariant => WireVariance::Invariant,
                            Variance::Covariant => WireVariance::Covariant,
                            Variance::Contravariant => WireVariance::Contravariant,
                        },
                    })
                    .collect(),
                body: self.ty(*body),
            },
            Type::Map { packs, pattern } => Ty::Map {
                packs: packs.iter().map(|&ty| self.ty(ty)).collect(),
                pattern: self.ty(*pattern),
            },
            Type::Restricted {
                schema,
                restriction,
            } => Ty::Restricted {
                schema: self.ty(*schema),
                deletable: restriction.key().map(|key| self.ty(key)),
            },
            Type::Fields { target, access } => Ty::Fields {
                target: self.ty(*target),
                write: *access == FieldAccess::Write,
            },
            Type::Overloaded {
                overloads,
                implementation,
                function,
            } => Ty::Overloaded {
                overloads: overloads.iter().map(|&ty| self.ty(ty)).collect(),
                implementation: implementation.map(|ty| self.ty(ty)),
                function: function.and_then(|function| self.named(function)),
            },
        };
        let next = self.table.types.len() as u32;
        let id = *self.nodes.entry(node.clone()).or_insert(next);
        if id == next {
            self.table.types.push(node);
        }
        self.memo.insert(ty, id);
        id
    }

    fn named(&mut self, decl: DeclId) -> Option<Named> {
        let &(module, item) = self.names.get(&decl)?;
        let source = &self.db.declaration(decl).source;
        Some(Named {
            module: self.text(module),
            item: self.text(item),
            kind: decl_kind(source.kind)?,
            result: kind(source.result_kind),
        })
    }

    fn literal(&mut self, lit: &Literal) -> Lit {
        match lit {
            Literal::Nil => Lit::Nil,
            Literal::Bool(value) => Lit::Bool(*value),
            Literal::Int(value) => Lit::Int(*value),
            Literal::Str(text) => Lit::Str(self.text(text)),
            Literal::Sym(name) => Lit::Sym(self.text(self.db.symbol(*name))),
        }
    }

    fn text(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.texts.get(text) {
            return id;
        }
        let id = self.table.strings.len() as u32;
        self.table.strings.push(text.into());
        self.texts.insert(text.into(), id);
        id
    }
}

fn kind(kind: Kind) -> WireKind {
    match kind {
        Kind::Type => WireKind::Type,
        Kind::Schema => WireKind::Schema,
    }
}

fn unkind(kind: WireKind) -> Kind {
    match kind {
        WireKind::Type => Kind::Type,
        WireKind::Schema => Kind::Schema,
    }
}

/// A declaration's kind, if it's one a type can name from another unit
fn decl_kind(kind: DeclKind) -> Option<WireDeclKind> {
    Some(match kind {
        DeclKind::Class => WireDeclKind::Class,
        DeclKind::Protocol => WireDeclKind::Protocol,
        DeclKind::OpaqueAlias => WireDeclKind::OpaqueAlias,
        DeclKind::Alias => WireDeclKind::Alias,
        DeclKind::Function => WireDeclKind::Function,
        DeclKind::Closure | DeclKind::Quantifier | DeclKind::Annotation => return None,
    })
}

fn multiplicity(multiplicity: Multiplicity) -> WireMultiplicity {
    match multiplicity {
        Multiplicity::Required => WireMultiplicity::Required,
        Multiplicity::Optional => WireMultiplicity::Optional,
        Multiplicity::Repeated => WireMultiplicity::Repeated,
    }
}

fn unmultiplicity(multiplicity: WireMultiplicity) -> Multiplicity {
    match multiplicity {
        WireMultiplicity::Required => Multiplicity::Required,
        WireMultiplicity::Optional => Multiplicity::Optional,
        WireMultiplicity::Repeated => Multiplicity::Repeated,
    }
}

fn unliteral<'t>(db: &Database, lit: &Lit, text: impl Fn(u32) -> &'t str) -> Literal {
    match *lit {
        Lit::Nil => Literal::Nil,
        Lit::Bool(value) => Literal::Bool(value),
        Lit::Int(value) => Literal::Int(value),
        Lit::Str(id) => Literal::Str(text(id).into()),
        Lit::Sym(id) => Literal::Sym(db.intern_symbol(text(id))),
    }
}

/// What every name in a set of units' exports names, by declaration: its unit's
/// module and the name, for what each module exports of its own
pub(crate) fn exported<'t>(
    tables: &'t crate::typeck::elab::Tables<'_>,
) -> FxHashMap<DeclId, (&'t str, &'t str)> {
    use crate::typeck::elab::{Referent, Target};
    let mut names = FxHashMap::default();
    for shard in &tables.shards {
        let Some(module) = shard.info.module else {
            continue;
        };
        for export in &shard.exports {
            if let Target::Local(Referent::Decl(decl)) = export.target {
                names
                    .entry(decl)
                    .or_insert((module, &*shard.strings[export.name.index()]));
            }
        }
    }
    names
}
