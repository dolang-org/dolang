use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, hash_map::Entry},
    hash::DefaultHasher,
    marker::PhantomData,
    ops::ControlFlow,
    ptr,
};

use crate::value::fmt::{Format, Spec};

use dolang_util::alias;

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, ErrorKind, Result},
    gc::{self, Annex, Collect, arena::Visit, with_pin},
    object::{
        BoundMethod,
        interface::Request,
        native,
        protocol::{
            Delegated, Dispatch, Inspect, Member, MemberKind, Recv, Spread, SpreadContext,
            instance_mcall_fallback, is_special_mcall,
        },
        sym::SymObj,
    },
    redirect,
    strand::{Strand, with_scan, with_slots_dynamic},
    sym::{self, Sym},
    unpack,
    value::{Input, Nil, Output, Singleton, Slot, TypeObject, Value, ValueCell},
    vm::{Alloc, AllocExt, Builder, StateTag, Vm},
    with_instance, with_slots, with_str,
};

use super::protocol::{GcObj, Protocol, Unpack};

/// Run a getter or setter with no input and its output discarded: it runs
/// wherever its field is accessed, so it can't use the accessing code's channels
macro_rules! isolated {
    ($strand:ident, $body:expr) => {
        redirect!(
            $strand,
            input: Singleton::Null,
            output: Singleton::Null,
            $body
        )
    };
}

/// Whether `value` is a function, as a member's or thunk's value must be. A type
/// object is a `Func`, since it's called to construct, but not a function.
fn is_function<'v>(strand: &mut Strand<'v, '_>, value: &Value<'v>) -> bool {
    value.is_instance_of(strand, &strand.singletons().func)
        && !value.is_instance_of(strand, &strand.singletons().type_obj)
}

/// Unwrap a `class`/`static` decorator marker, replacing `value` with the member
/// it wraps and reporting which namespace that member belongs to.
fn unwrap_member_scope<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &mut Slot<'v, '_>,
) -> Result<'v, 's, MemberScope> {
    let scopes = strand.vm().state::<crate::object::class::MemberScopeTag>();
    let scope = with_slots!(strand, |mut inner| {
        let scope = if let Some(cast) = scopes.class.cast(value) {
            with_instance!(strand, cast, |inst| {
                let marker = inst.borrow(strand)?;
                strand.store(&mut inner, native::Ref::slot::<0>(&marker));
                Ok(())
            })?;
            MemberScope::Class
        } else if let Some(cast) = scopes.statik.cast(value) {
            with_instance!(strand, cast, |inst| {
                let marker = inst.borrow(strand)?;
                strand.store(&mut inner, native::Ref::slot::<0>(&marker));
                Ok(())
            })?;
            MemberScope::Static
        } else {
            return Ok(MemberScope::Instance);
        };
        strand.store(Slot::reborrow(value), inner);
        scope
    });

    if scopes.class.cast(value).is_some() || scopes.statik.cast(value).is_some() {
        return Err(Error::type_error(
            strand,
            "class_create: `class` and `static` cannot be combined",
        ));
    }
    Ok(scope)
}

/// Reject special methods that are not meaningful in the class-level namespace.
///
/// Only `(call)` is supported so far: it is what lets a class expose a factory
/// that a subclass does not inherit. Every other special method would need its
/// own hook in the corresponding `ClassObject::op_*`.
fn reject_disallowed_type_member<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sym: Sym<'v, 'static>,
) -> Result<'v, 's, ()> {
    if sym == Sym::well_known(sym::CALL_METHOD) {
        return Ok(());
    }
    if sym == Sym::well_known(sym::INIT_METHOD) {
        return Err(Error::type_error(
            strand,
            "class_create: `(init)` is never a class or static member",
        ));
    }
    let name = sym.as_str(strand);
    if name.starts_with('(') {
        return Err(Error::type_error(
            strand,
            format!("class_create: `{name}` cannot be a class or static member"),
        ));
    }
    Ok(())
}

/// Insert a member into the class-level namespace, merging a getter with its
/// matching setter and rejecting any other collision.
///
/// A `class` and a `static` member of the same name collide here, which is the
/// intended hard error: both would occupy the same type-object namespace.
fn insert_type_member<'v, 's>(
    strand: &mut Strand<'v, 's>,
    map: &RefCell<HashMap<Sym<'v, 'static>, ClassTypeEntry<'v>>>,
    sym: Sym<'v, 'static>,
    mut entry: ClassTypeEntry<'v>,
) -> Result<'v, 's, ()> {
    let inserted = match map.borrow_mut().entry(sym) {
        Entry::Vacant(vacant) => {
            vacant.insert(entry);
            true
        }
        Entry::Occupied(mut occupied) => match (occupied.get_mut(), &mut entry) {
            (
                ClassTypeEntry::Property {
                    property: existing,
                    inherited,
                },
                ClassTypeEntry::Property {
                    property: incoming,
                    inherited: incoming_inherited,
                },
            ) if *inherited == *incoming_inherited => existing.merge(incoming),
            _ => false,
        },
    };
    if inserted {
        Ok(())
    } else {
        Err(duplicate_member(strand, sym))
    }
}

/// Insert a member into the instance namespace, merging a getter with its matching
/// setter. Returns `false` on any other collision.
fn insert_entry<'v>(
    map: &RefCell<HashMap<Sym<'v, 'static>, ClassEntry<'v>>>,
    sym: Sym<'v, 'static>,
    mut entry: ClassEntry<'v>,
) -> bool {
    match map.borrow_mut().entry(sym) {
        Entry::Vacant(vacant) => {
            vacant.insert(entry);
            true
        }
        Entry::Occupied(mut occupied) => match (occupied.get_mut(), &mut entry) {
            (ClassEntry::Property(existing), ClassEntry::Property(incoming)) => {
                existing.merge(incoming)
            }
            _ => false,
        },
    }
}

fn duplicate_member<'v, 's>(strand: &mut Strand<'v, 's>, sym: Sym<'v, '_>) -> Error<'v, 's> {
    Error::runtime(
        strand,
        format!(
            "class_create: duplicate class member `{}`",
            sym.as_str(strand)
        ),
    )
}

/// The tables [`create`] gathers before it allocates the class, which an ad-hoc frame
/// roots meanwhile.
///
/// Each table has its own cell, so one can be read across code that allocates while
/// another changes. None is borrowed mutably across an allocation.
#[derive(Default)]
struct ClassTables<'v> {
    supers: RefCell<Vec<Value<'v>>>,
    symbols: RefCell<Vec<GcObj<'v, SymObj>>>,
    local_entries: RefCell<HashMap<Sym<'v, 'static>, ClassEntry<'v>>>,
    local_type_entries: RefCell<HashMap<Sym<'v, 'static>, ClassTypeEntry<'v>>>,
    field_defaults: RefCell<Vec<FieldDefault<'v>>>,
    type_field_defaults: RefCell<Vec<FieldDefault<'v>>>,
    native_supers: RefCell<Vec<Value<'v>>>,
    abstract_supers: RefCell<Vec<Value<'v>>>,
    entries: RefCell<HashMap<Sym<'v, 'static>, ClassEntry<'v>>>,
    type_entries: RefCell<HashMap<Sym<'v, 'static>, ClassTypeEntry<'v>>>,
    type_fields: RefCell<Vec<Value<'v>>>,
}

impl<'v> ClassTables<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for values in [
            &self.supers,
            &self.native_supers,
            &self.abstract_supers,
            &self.type_fields,
        ] {
            for value in values.borrow().iter() {
                value.accept(visit)?;
            }
        }
        for sym in self.symbols.borrow().iter() {
            sym.accept(visit)?;
        }
        for entries in [&self.local_entries, &self.entries] {
            for entry in entries.borrow().values() {
                entry.accept(visit)?;
            }
        }
        for entries in [&self.local_type_entries, &self.type_entries] {
            for entry in entries.borrow().values() {
                entry.accept(visit)?;
            }
        }
        for defaults in [&self.field_defaults, &self.type_field_defaults] {
            for default in defaults.borrow().iter() {
                default.accept(visit)?;
            }
        }
        ControlFlow::Continue(())
    }
}

#[inline(never)]
pub(crate) async fn create<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let Some(Arg::Pos(name)) = args.next() else {
        return Err(Error::missing_positional(strand, 0));
    };
    let Some(Arg::Pos(module_name)) = args.next() else {
        return Err(Error::missing_positional(strand, 1));
    };

    // Extract class name
    let name = name
        .as_str(strand)
        .ok_or_else(|| Error::type_error(strand, "class_create: expected string name"))?;
    let name: alias::Box<str> = with_str!(strand, name, |name| name.into());
    let module_name = module_name
        .as_str(strand)
        .ok_or_else(|| Error::type_error(strand, "class_create: expected string module name"))?;
    let module_name = if module_name.is_empty() {
        None
    } else {
        Some(with_str!(strand, module_name, |module_name| {
            alias::Box::<str>::from(module_name)
        }))
    };

    let tables = ClassTables::default();
    let visit = |visitor: &mut dyn Visit| tables.accept(visitor);
    // SAFETY: `visit` visits every table, and no table is borrowed mutably across an
    // allocation
    unsafe {
        with_scan!(
            strand,
            visit,
            build(strand, &tables, args, name, module_name, out).await
        )
    }
}

/// The body of [`create`], gathering into `tables`.
async fn build<'v, 's>(
    strand: &mut Strand<'v, 's>,
    tables: &ClassTables<'v>,
    mut args: Args<'v, '_>,
    name: alias::Box<str>,
    module_name: Option<alias::Box<str>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    while let Some(arg) = args.next() {
        let (key, mut slot) = match arg {
            Arg::Pos(_slot) => {
                return Err(Error::type_error(
                    strand,
                    "class_create: unexpected positional argument",
                ));
            }
            Arg::Key(key, slot) => (key, slot),
        };

        match key.tag() {
            sym::SUPER => {
                if !slot.is_instance_of(strand, &strand.singletons().type_obj) {
                    return Err(Error::type_error(
                        strand,
                        "class_create: superclass must be a type object",
                    ));
                }
                // SAFETY: the value moves straight into the scanned builder tables
                tables
                    .supers
                    .borrow_mut()
                    .push(unsafe { slot.take(strand) });
            }
            sym::FIELD
            | sym::FIELD_THUNK
            | sym::CLASS_FIELD
            | sym::CLASS_FIELD_THUNK
            | sym::STATIC_FIELD => {
                let scope = match key.tag() {
                    sym::FIELD | sym::FIELD_THUNK => MemberScope::Instance,
                    sym::CLASS_FIELD | sym::CLASS_FIELD_THUNK => MemberScope::Class,
                    _ => MemberScope::Static,
                };
                let sym = unsafe {
                    slot.as_sym(strand)
                        .ok_or_else(|| {
                            Error::type_error(strand, "class_create: field name must be a symbol")
                        })?
                        .into_static_scope_unchecked()
                };
                // SAFETY: the symbol moves straight into the scanned tables
                let sym_obj = unsafe { strand.sym_obj(sym) };
                tables.symbols.borrow_mut().push(sym_obj);
                let Some(Arg::Pos(mut default)) = args.next() else {
                    return Err(Error::type_error(
                        strand,
                        "class_create: field entry must include a default value",
                    ));
                };
                let is_thunk = matches!(key.tag(), sym::FIELD_THUNK | sym::CLASS_FIELD_THUNK);
                if is_thunk && !is_function(strand, &default) {
                    return Err(Error::type_error(
                        strand,
                        "class_create: field thunk must be a function",
                    ));
                }
                let defaults = match scope {
                    MemberScope::Instance => &tables.field_defaults,
                    _ => &tables.type_field_defaults,
                };
                let index = {
                    let mut defaults = defaults.borrow_mut();
                    // SAFETY: the value moves straight into the scanned builder tables
                    let default = unsafe { default.take(strand) };
                    defaults.push(if is_thunk {
                        FieldDefault::Thunk(default)
                    } else {
                        FieldDefault::Value(default)
                    });
                    defaults.len() - 1
                };

                if scope != MemberScope::Instance {
                    insert_type_member(
                        strand,
                        &tables.local_type_entries,
                        sym,
                        ClassTypeEntry::Field {
                            slot: index,
                            inherited: scope == MemberScope::Class,
                        },
                    )?;
                } else if !insert_entry(&tables.local_entries, sym, ClassEntry::Field(index)) {
                    return Err(duplicate_member(strand, sym));
                }
            }
            sym::METHOD => {
                let sym = unsafe {
                    slot.as_sym(strand)
                        .ok_or_else(|| {
                            Error::type_error(strand, "class_create: method name must be a symbol")
                        })?
                        .into_static_scope_unchecked()
                };
                // SAFETY: the symbol moves straight into the scanned tables
                let sym_obj = unsafe { strand.sym_obj(sym) };
                tables.symbols.borrow_mut().push(sym_obj);
                let Some(Arg::Pos(mut value)) = args.next() else {
                    return Err(Error::type_error(
                        strand,
                        "class_create: method entry must include a value",
                    ));
                };

                // `#[class]` / `#[static]` wrap the member; unwrap before deciding
                // what kind of member it is, so the scope composes with `#[getter]`.
                let scope = unwrap_member_scope(strand, &mut value)?;
                // An accessor may wrap any callable, such as an object with `(call)`
                let accessor = unwrap_accessor(strand, &mut value);
                if accessor.is_none() && !is_function(strand, &value) {
                    return Err(Error::type_error(
                        strand,
                        "class_create: method value must be a function",
                    ));
                }
                let property = |value| match accessor {
                    Some(AccessorKind::Getter) => Some(Property {
                        getter: Some(value),
                        setter: None,
                    }),
                    Some(AccessorKind::Setter) => Some(Property {
                        getter: None,
                        setter: Some(value),
                    }),
                    None => None,
                };
                if scope != MemberScope::Instance {
                    reject_disallowed_type_member(strand, sym)?;
                }
                // SAFETY: the value moves into an entry, which is inserted into the
                // scanned builder tables at once
                let value = unsafe { value.take(strand) };
                if scope != MemberScope::Instance {
                    let inherited = scope == MemberScope::Class;
                    let entry = match accessor {
                        Some(_) => ClassTypeEntry::Property {
                            property: property(value).unwrap(),
                            inherited,
                        },
                        None => ClassTypeEntry::Method { value, inherited },
                    };
                    insert_type_member(strand, &tables.local_type_entries, sym, entry)?;
                    continue;
                }

                let entry = match accessor {
                    Some(_) => ClassEntry::Property(property(value).unwrap()),
                    None => ClassEntry::Method(value),
                };
                if !insert_entry(&tables.local_entries, sym, entry) {
                    return Err(duplicate_member(strand, sym));
                }
            }
            _ => return Err(Error::unexpected_key(strand, key)),
        }
    }

    // A final type has no subclasses, which the type checker relies on. Supers are
    // marked inherited before any field default runs, so a default can't make one
    // final while this class is still being created; a creation that fails later
    // leaves the mark, which only forbids `#[final]`.
    for sup in tables.supers.borrow().iter() {
        let is_final = match sup.downcast_ref(strand.builtin_types().class_object) {
            Some(cls) => cls.annex().is_final.get(),
            None => sup
                .op_inspect(strand)
                .is_some_and(|inspect| inspect.is_final),
        };
        if is_final {
            let name = sup.to_debug(strand)?;
            return Err(Error::type_error(
                strand,
                format!("cannot inherit from final type {name}"),
            ));
        }
    }
    for sup in tables.supers.borrow().iter() {
        if let Some(cls) = sup.downcast_ref(strand.builtin_types().class_object) {
            cls.annex().inherited.set(true);
        }
    }

    // Build native_supers and entries in a single left-to-right MRO pass.
    // native_supers: non-abstract native type objects; index == ClassInstance native slot.
    // abstract_supers: abstract native type objects, direct and inherited; entries store
    // the type object directly.
    // entries: built left-to-right with first-insertion-wins (MRO order).

    // Every class ends its MRO with `Value`, which supplies the members every
    // value has, unless it names `Value` itself.
    // SAFETY (every `dup` in this pass): the tables are rooted by the scan above, so
    // each copy moves straight into a scanned table
    let value = TypeObject::Value.value(strand.vm());
    {
        let supers = tables.supers.borrow();
        let value = (!supers.iter().any(|sup| sup.repr_eq(value))).then_some(value);

        for sup in supers.iter().chain(value) {
            if let Some(cls) = sup.downcast_ref(strand.builtin_types().class_object) {
                let cls = cls.annex();
                let mut native_supers = tables.native_supers.borrow_mut();
                let mut abstract_supers = tables.abstract_supers.borrow_mut();
                let mut entries = tables.entries.borrow_mut();
                let mut type_entries = tables.type_entries.borrow_mut();
                let mut field_defaults = tables.field_defaults.borrow_mut();
                let mut type_field_defaults = tables.type_field_defaults.borrow_mut();
                // Inherit parent's native supers (dedup by repr_eq).
                for type_obj in cls.native_supers.iter() {
                    if native_supers.iter().any(|s| s.repr_eq(type_obj)) {
                        continue;
                    }
                    native_supers.push(unsafe { type_obj.dup() });
                }
                for type_obj in cls.abstract_supers.iter() {
                    if abstract_supers.iter().any(|s| s.repr_eq(type_obj)) {
                        continue;
                    }
                    abstract_supers.push(unsafe { type_obj.dup() });
                }
                // Merge parent's entries (left-wins). Abstract entries copy the value directly.
                for (sym, entry) in cls.entries.iter() {
                    if entries.contains_key(sym) {
                        continue; // left wins
                    }
                    let new_entry = match entry {
                        ClassEntry::Field(old_slot) => {
                            let new_slot = field_defaults.len();
                            field_defaults.push(unsafe { cls.field_defaults[*old_slot].dup() });
                            ClassEntry::Field(new_slot)
                        }
                        ClassEntry::Method(v) => ClassEntry::Method(unsafe { v.dup() }),
                        ClassEntry::Property(property) => ClassEntry::Property(Property {
                            getter: property.getter.as_ref().map(|v| unsafe { v.dup() }),
                            setter: property.setter.as_ref().map(|v| unsafe { v.dup() }),
                        }),
                        ClassEntry::Delegate(parent_slot, kind) => {
                            // Remap via parent's native_supers → our native_supers.
                            let type_obj = &cls.native_supers[*parent_slot];
                            let our_slot = native_supers
                                .iter()
                                .position(|s| s.repr_eq(type_obj))
                                .expect("bug: parent Delegate slot not found in our native_supers");
                            ClassEntry::Delegate(our_slot, *kind)
                        }
                        ClassEntry::Abstract(type_obj, kind) => {
                            ClassEntry::Abstract(unsafe { type_obj.dup() }, *kind)
                        }
                    };
                    entries.insert(*sym, new_entry);
                }
                // Type members are inherited independently of the instance namespace.
                // `static` members are skipped: they belong to the declaring class only.
                for (sym, entry) in cls.type_entries.iter() {
                    if !entry.inherited() || type_entries.contains_key(sym) {
                        continue; // left wins
                    }
                    let new_entry = match entry {
                        // Each class owns its class-field storage, so a subclass gets a
                        // fresh slot seeded from the declared default rather than sharing
                        // the parent's cell.
                        ClassTypeEntry::Field { slot: old_slot, .. } => {
                            let slot = type_field_defaults.len();
                            type_field_defaults
                                .push(unsafe { cls.type_field_defaults[*old_slot].dup() });
                            ClassTypeEntry::Field {
                                slot,
                                inherited: true,
                            }
                        }
                        other => unsafe { other.dup() },
                    };
                    type_entries.insert(*sym, new_entry);
                }
            } else {
                let inspect = sup.op_inspect(strand).ok_or_else(|| {
                    Error::type_error(strand, "inheritance not supported by superclass")
                })?;
                // SAFETY: native member symbols remain rooted by their registered native
                // type vtables
                let member_sym =
                    |member: &Member<'v, '_>| unsafe { member.sym.into_static_scope_unchecked() };
                {
                    let mut type_entries = tables.type_entries.borrow_mut();
                    for member in inspect.type_members {
                        type_entries.entry(member_sym(member)).or_insert_with(|| {
                            ClassTypeEntry::Delegate(unsafe { sup.dup() }, member.kind)
                        });
                    }
                }

                // Direct native super. Skip if already seen (inherited via a ClassObject parent).
                if tables.native_supers.borrow().iter().any(|s| s.repr_eq(sup))
                    || tables
                        .abstract_supers
                        .borrow()
                        .iter()
                        .any(|s| s.repr_eq(sup))
                {
                    continue;
                }
                let mut entries = tables.entries.borrow_mut();
                if inspect.is_abstract {
                    // Abstract super: store the type-object directly in each entry.
                    tables
                        .abstract_supers
                        .borrow_mut()
                        .push(unsafe { sup.dup() });
                    for member in inspect.members {
                        entries.entry(member_sym(member)).or_insert_with(|| {
                            ClassEntry::Abstract(unsafe { sup.dup() }, member.kind)
                        });
                    }
                } else {
                    // Concrete native super: members dispatched via instance native slot.
                    let mut native_supers = tables.native_supers.borrow_mut();
                    let our_slot = native_supers.len();
                    native_supers.push(unsafe { sup.dup() });
                    for member in inspect.members {
                        entries
                            .entry(member_sym(member))
                            .or_insert(ClassEntry::Delegate(our_slot, member.kind));
                    }
                }
            }
        }
    }

    // Type-object members inherit the protocol methods inspected from Type.
    // Instance defaults come from the inspected runtime supertypes above.
    let metatype = &strand.singletons().type_obj;
    let inspect = metatype
        .op_inspect(strand)
        .expect("Type supports inspection");
    {
        let mut type_entries = tables.type_entries.borrow_mut();
        for member in inspect.members {
            // SAFETY: as for the native super members above
            let sym = unsafe { member.sym.into_static_scope_unchecked() };
            type_entries.insert(
                sym,
                ClassTypeEntry::Delegate(unsafe { metatype.dup() }, member.kind),
            );
        }
    }

    // Apply this class's own entries, overriding
    let local_entries = tables.local_entries.take();
    tables.entries.borrow_mut().extend(local_entries);
    let local_type_entries = tables.local_type_entries.take();
    tables.type_entries.borrow_mut().extend(local_type_entries);

    // Evaluate class-level field defaults. Thunks run once per class, which is
    // what gives each class in a hierarchy its own storage.
    let count = tables.type_field_defaults.borrow().len();
    for index in 0..count {
        with_slots!(strand, |mut thunk, mut value| {
            match &tables.type_field_defaults.borrow()[index] {
                FieldDefault::Value(default) => strand.store(&mut value, default),
                FieldDefault::Thunk(default) => strand.store(&mut thunk, default),
            }
            if !thunk.is_nil() {
                call!(strand, &thunk, &mut value)?;
            }
            // SAFETY: the value moves straight into the scanned builder tables
            tables
                .type_fields
                .borrow_mut()
                .push(unsafe { value.take(strand) });
        });
    }

    let mut members = tables
        .entries
        .borrow()
        .iter()
        .filter(|(sym, _)| !strand.sym_private(**sym))
        .map(|(sym, entry)| {
            Member::new(
                *sym,
                match entry {
                    ClassEntry::Field(_) => MemberKind::Property,
                    ClassEntry::Method(_) => MemberKind::Method,
                    ClassEntry::Property(property) => property.kind(),
                    ClassEntry::Delegate(_, kind) | ClassEntry::Abstract(_, kind) => *kind,
                },
            )
        })
        .collect::<Vec<_>>();
    members.sort_by_key(|member| member.sym);
    let mut type_members = tables
        .type_entries
        .borrow()
        .iter()
        .filter(|(sym, _)| !strand.sym_private(**sym))
        .map(|(sym, entry)| {
            Member::new(
                *sym,
                match entry {
                    ClassTypeEntry::Field { .. } => MemberKind::Property,
                    ClassTypeEntry::Method { .. } => MemberKind::Method,
                    ClassTypeEntry::Property { property, .. } => property.kind(),
                    ClassTypeEntry::Delegate(_, kind) => *kind,
                },
            )
        })
        .collect::<Vec<_>>();
    type_members.sort_by_key(|member| member.sym);

    // The tables move out only once the class is allocated
    let embryo = strand.vm().builtin_types().class_object.embryo(strand);
    let mut entries: Vec<_> = tables.entries.take().into_iter().collect();
    entries.sort_by_key(|(s, _)| *s);
    let mut type_entries: Vec<_> = tables.type_entries.take().into_iter().collect();
    type_entries.sort_by_key(|(s, _)| *s);
    let class_annex = ClassObjectAnnex {
        members: members.into(),
        type_members: type_members.into(),
        name,
        module_name,
        symbols: tables.symbols.take().into(),
        entries: entries.into(),
        type_entries: type_entries.into(),
        supers: tables.supers.take().into(),
        field_defaults: tables.field_defaults.take().into(),
        type_field_defaults: tables.type_field_defaults.take().into(),
        native_supers: tables.native_supers.take().into(),
        abstract_supers: tables.abstract_supers.take().into(),
        is_final: Cell::new(false),
        inherited: Cell::new(false),
    };
    embryo.finish_with_annex(
        strand,
        ClassObject {
            type_fields: tables.type_fields.take().into(),
        },
        alias::Box::new(class_annex),
        out,
    );

    Ok(())
}

/// Which half of a property an [`Accessor`] supplies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessorKind {
    Getter,
    Setter,
}

impl AccessorKind {
    fn name(self) -> &'static str {
        match self {
            Self::Getter => "getter",
            Self::Setter => "setter",
        }
    }
}

/// Marker made by the prelude `getter` / `setter` types from a function.
///
/// [`create`] stores the function it holds as one half of a property and drops
/// the marker, so a property's accessors are called directly.
pub(crate) struct Accessor<'v> {
    kind: AccessorKind,
    /// The function it wraps.
    func: Value<'v>,
}

unsafe impl<'v> gc::ImmState for Accessor<'v> {
    const CYCLIC: bool = true;
}

impl<'v> Annex for Accessor<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.func.accept(visit)
    }
}

pub(crate) type AccessorObj<'v> = gc::Imm<Accessor<'v>>;

impl<'v> Protocol<'v> for AccessorObj<'v> {
    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        let singletons = strand.singletons();
        let ty = match this.get().kind {
            AccessorKind::Getter => &singletons.getter,
            AccessorKind::Setter => &singletons.setter,
        };
        strand.store(out, ty)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<{}>", this.get().kind.name())
    }
}

/// The `getter` and `setter` types, which make an [`Accessor`] when called.
pub(crate) struct AccessorType(pub(crate) AccessorKind);

unsafe impl gc::ImmState for AccessorType {
    const CYCLIC: bool = false;
}

impl gc::Annex for AccessorType {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

pub(crate) type AccessorTypeObj = gc::Imm<AccessorType>;

impl<'v> Protocol<'v> for AccessorTypeObj {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.{}>", this.get().0.name())
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let kind = this.get().0;
        unpack!(strand, args => mut func);
        let embryo = strand.builtin_types().accessor.embryo(strand);
        // SAFETY: the accessor is allocated; the function moves from its rooted slot
        // straight into it
        let accessor = Accessor {
            kind,
            func: unsafe { func.take(strand) },
        };
        embryo.finish_imm(strand, accessor, out);
        Ok(())
    }
}

/// If `value` is an [`Accessor`], replace it with the function it wraps and
/// return which half of a property it supplies.
fn unwrap_accessor<'v>(
    strand: &mut Strand<'v, '_>,
    value: &mut Slot<'v, '_>,
) -> Option<AccessorKind> {
    with_slots!(strand, |mut func| {
        let accessor = value.downcast_ref(strand.builtin_types().accessor)?;
        let kind = with_pin!(strand, accessor, |accessor| {
            strand.store(&mut func, &accessor.get().func);
            accessor.get().kind
        });
        strand.store(Slot::reborrow(value), func);
        Some(kind)
    })
}

/// Marker wrapper produced by the prelude `class` decorator.
///
/// The decorated member is held in slot 0. [`create`] unwraps it and records the
/// member in the class-level namespace rather than the instance namespace.
pub(crate) struct ClassMarker;

/// Marker wrapper produced by the prelude `static` decorator.
///
/// Like [`ClassMarker`], except the member is not inherited by subclasses.
pub(crate) struct StaticMarker;

macro_rules! member_marker {
    ($ty:ident, $name:literal) => {
        impl<'v> native::Object<'v> for $ty {
            const MODULE: &'v str = "std";
            const NAME: &'v str = $name;
            const SLOTS: usize = 1;

            type Annex = ();
            type Type = ();
            type TypeAnnex = ();

            async fn new<'a, 's>(
                this: native::Type<'v, Self>,
                strand: &'a mut Strand<'v, 's>,
                args: Args<'v, 'a>,
                mut out: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                unpack!(strand, args => member);
                this.create(strand, $ty, &mut out);
                with_instance!(strand, this.cast(&out).unwrap(), |inst| {
                    let mut borrow = inst.borrow_mut_unwrap();
                    strand.store(native::Mut::slot_mut::<0>(&mut borrow), member);
                });
                Ok(())
            }
        }
    };
}

member_marker!(ClassMarker, "class");
member_marker!(StaticMarker, "static");

pub(crate) struct MemberScopeTag;

/// Type handles for the member-scope decorators, kept as VM state so [`create`]
/// can recognize the wrappers they produce.
pub(crate) struct MemberScopeTypes<'v> {
    pub(crate) class: native::Type<'v, ClassMarker>,
    pub(crate) statik: native::Type<'v, StaticMarker>,
}

impl StateTag for MemberScopeTag {
    type State<'v> = MemberScopeTypes<'v>;
}

/// Forbid inheriting from a class, as `#[final]` does
pub(crate) fn make_final<'v, 's>(
    strand: &mut Strand<'v, 's>,
    class: &Value<'v>,
) -> Result<'v, 's, ()> {
    let Some(class) = class.downcast_ref(strand.builtin_types().class_object) else {
        return Err(Error::type_error(strand, "final: expected a class"));
    };
    let annex = class.annex();
    if annex.inherited.get() {
        return Err(Error::state_error(
            strand,
            "final: a class already inherits from this one",
        ));
    }
    annex.is_final.set(true);
    Ok(())
}

/// Let a class be combined with unrelated classes, as `#[mixable]` does. The
/// type checker relies on the mark; the runtime doesn't enforce it yet.
pub(crate) fn make_mixable<'v, 's>(
    strand: &mut Strand<'v, 's>,
    class: &Value<'v>,
) -> Result<'v, 's, ()> {
    let Some(class) = class.downcast_ref(strand.builtin_types().class_object) else {
        return Err(Error::type_error(strand, "mixable: expected a class"));
    };
    if class.annex().inherited.get() {
        return Err(Error::state_error(
            strand,
            "mixable: a class already inherits from this one",
        ));
    }
    Ok(())
}

pub(crate) fn register_member_scopes<'v>(builder: &mut Builder<'v>) -> MemberScopeTypes<'v> {
    MemberScopeTypes {
        class: builder.register_type(),
        statik: builder.register_type(),
    }
}

/// Which namespace a declared member belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberScope {
    /// The instance namespace.
    Instance,
    /// The type-object namespace, inherited by subclasses.
    Class,
    /// The type-object namespace, not inherited.
    Static,
}

pub(crate) struct Property<'v> {
    pub(crate) getter: Option<Value<'v>>,
    pub(crate) setter: Option<Value<'v>>,
}

impl<'v> Property<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for accessor in [&self.getter, &self.setter].into_iter().flatten() {
            accessor.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    /// Fill in whichever accessor `incoming` has and this lacks, taking it from
    /// `incoming`. Returns `false` if there is none.
    fn merge(&mut self, incoming: &mut Self) -> bool {
        if self.getter.is_none() && incoming.getter.is_some() {
            self.getter = incoming.getter.take();
            true
        } else if self.setter.is_none() && incoming.setter.is_some() {
            self.setter = incoming.setter.take();
            true
        } else {
            false
        }
    }

    fn kind(&self) -> MemberKind {
        match (self.getter.is_some(), self.setter.is_some()) {
            (true, true) => MemberKind::Property,
            (true, false) => MemberKind::Getter,
            (false, true) => MemberKind::Setter,
            (false, false) => unreachable!("property must have an accessor"),
        }
    }
}

pub(crate) enum FieldDefault<'v> {
    Value(Value<'v>),
    Thunk(Value<'v>),
}

impl<'v> FieldDefault<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        match self {
            Self::Value(v) | Self::Thunk(v) => v.accept(visit),
        }
    }

    /// # Safety
    /// As for [`Value::dup`]: the copy is loose.
    unsafe fn dup(&self) -> Self {
        // SAFETY: the caller's contract
        unsafe {
            match self {
                Self::Value(v) => Self::Value(v.dup()),
                Self::Thunk(v) => Self::Thunk(v.dup()),
            }
        }
    }
}

impl<'v> ClassEntry<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        match self {
            Self::Method(v) | Self::Abstract(v, _) => v.accept(visit),
            Self::Property(property) => property.accept(visit),
            Self::Field(_) | Self::Delegate(..) => ControlFlow::Continue(()),
        }
    }
}

/// A single entry in a class's unified symbol table.
pub(crate) enum ClassEntry<'v> {
    /// Index into `field_defaults` / instance `fields`.
    Field(usize),
    /// A Do function value (method).
    Method(Value<'v>),
    /// A getter/setter pair of functions, called with the instance.
    Property(Property<'v>),
    /// Native delegation: index into instance `natives`.
    Delegate(usize, MemberKind),
    /// Abstract delegation: the type-object singleton to dispatch to.
    /// Dispatched on the type object with the instance as delegator.
    Abstract(Value<'v>, MemberKind),
}

/// A member of a Do class's type-object namespace.
///
/// `inherited` distinguishes `class` members, which subclasses pick up, from
/// `static` members, which they do not. A static therefore shadows an inherited
/// class member of the same name without propagating any further. A class
/// method or property is called with the class it was reached through; a static
/// one is called without a receiver.
pub(crate) enum ClassTypeEntry<'v> {
    /// Class-level field: index into `type_fields` / `type_field_defaults`.
    Field { slot: usize, inherited: bool },
    /// A Do function value.
    Method { value: Value<'v>, inherited: bool },
    /// A getter/setter pair mediating access to the class object.
    Property {
        property: Property<'v>,
        inherited: bool,
    },
    /// Forward to the native type object while retaining the Do class as
    /// delegator. Always inherited: native supers have no static members.
    Delegate(Value<'v>, MemberKind),
}

impl<'v> ClassTypeEntry<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        match self {
            Self::Field { .. } => ControlFlow::Continue(()),
            Self::Method { value, .. } | Self::Delegate(value, _) => value.accept(visit),
            Self::Property { property, .. } => property.accept(visit),
        }
    }

    fn inherited(&self) -> bool {
        match self {
            Self::Field { inherited, .. }
            | Self::Method { inherited, .. }
            | Self::Property { inherited, .. } => *inherited,
            Self::Delegate(..) => true,
        }
    }

    /// # Safety
    /// As for [`Value::dup`]: the copy is loose.
    unsafe fn dup(&self) -> Self {
        // SAFETY (throughout): the caller's contract
        match self {
            Self::Field { slot, inherited } => Self::Field {
                slot: *slot,
                inherited: *inherited,
            },
            Self::Method { value, inherited } => Self::Method {
                value: unsafe { value.dup() },
                inherited: *inherited,
            },
            Self::Property {
                property,
                inherited,
            } => Self::Property {
                property: Property {
                    getter: property.getter.as_ref().map(|v| unsafe { v.dup() }),
                    setter: property.setter.as_ref().map(|v| unsafe { v.dup() }),
                },
                inherited: *inherited,
            },
            Self::Delegate(type_obj, kind) => Self::Delegate(unsafe { type_obj.dup() }, *kind),
        }
    }
}

/// The immutable half of a class object: everything fixed at class creation,
/// and the two flags that rule out each other later.
///
/// Living in the annex keeps member lookup and dispatch free of the runtime
/// borrow check; only class-level field storage needs one.
pub(crate) struct ClassObjectAnnex<'v> {
    members: alias::Box<[Member<'v, 'static>]>,
    type_members: alias::Box<[Member<'v, 'static>]>,
    // Class name (for debug formatting)
    pub(crate) name: alias::Box<str>,
    // Optional module name for debug formatting.
    pub(crate) module_name: Option<alias::Box<str>>,
    // Direct superclasses (for subtype checking); may be ClassObject or built-in type objects
    pub(crate) supers: alias::Box<[Value<'v>]>,
    // Unified MRO-ordered lookup table sorted by Sym. Symbols are rooted by `_symbols`.
    pub(crate) entries: alias::Box<[(Sym<'v, 'static>, ClassEntry<'v>)]>,
    // MRO-ordered type-object members, separate from the instance namespace.
    pub(crate) type_entries: alias::Box<[(Sym<'v, 'static>, ClassTypeEntry<'v>)]>,
    // Roots for the symbols used by `entries`.
    pub(crate) symbols: alias::Box<[GcObj<'v, SymObj>]>,
    // Default values for field slots (indexed by ClassEntry::Field(n))
    pub(crate) field_defaults: alias::Box<[FieldDefault<'v>]>,
    // Defaults for class-level field slots (indexed by ClassTypeEntry::Field(n)).
    // Retained so a subclass can re-run them into its own storage.
    pub(crate) type_field_defaults: alias::Box<[FieldDefault<'v>]>,
    // Non-abstract native supers, in transitive collection order (left-to-right).
    // Index in this slice == slot index in ClassInstance::native_slots.
    pub(crate) native_supers: alias::Box<[Value<'v>]>,
    // Abstract native supers, direct and inherited, in the same order. They answer
    // interface queries after the native slots.
    pub(crate) abstract_supers: alias::Box<[Value<'v>]>,
    // Set by `#[final]`; no class may then inherit from this one.
    pub(crate) is_final: Cell<bool>,
    // Set once a class inherits from this one; `#[final]` then fails.
    pub(crate) inherited: Cell<bool>,
}

/// The mutable half of a class object: storage for class-level fields.
pub(crate) struct ClassObject<'v> {
    pub(crate) type_fields: alias::Box<[Value<'v>]>,
}

impl<'v> ClassObjectAnnex<'v> {
    /// Look up an entry by symbol.
    pub(crate) fn entry(&self, sym: Sym<'v, '_>) -> Option<&ClassEntry<'v>> {
        self.entries
            .binary_search_by_key(&sym, |(s, _)| *s)
            .ok()
            .map(|idx| &self.entries[idx].1)
    }

    /// Look up an inherited type-object entry by symbol.
    pub(crate) fn type_entry(&self, sym: Sym<'v, '_>) -> Option<&ClassTypeEntry<'v>> {
        self.type_entries
            .binary_search_by_key(&sym, |(s, _)| *s)
            .ok()
            .map(|idx| &self.type_entries[idx].1)
    }

    /// Look up a method by symbol.
    pub(crate) fn method(&self, sym: Sym<'v, '_>) -> Option<&Value<'v>> {
        self.entry(sym).and_then(|entry| match entry {
            ClassEntry::Method(v) => Some(v),
            _ => None,
        })
    }

    /// Look up an entry by well-known symbol tag.
    pub(crate) fn entry_by_tag(&self, tag: sym::Tag) -> Option<&ClassEntry<'v>> {
        self.entry(Sym::well_known(tag))
    }
}

// Out of line, so a class reached through an instance needs no pin of its own
unsafe impl<'v> Collect for ClassObject<'v> {
    const CYCLIC: bool = true;
    type Annex = alias::Box<ClassObjectAnnex<'v>>;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for field in self.type_fields.iter() {
            field.accept(visit)?
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.type_fields.fill_with(|| Value::NIL);
    }
}

impl<'v> Annex for ClassObjectAnnex<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for sup in self.supers.iter() {
            sup.accept(visit)?;
        }
        for sym in self.symbols.iter() {
            sym.accept(visit)?;
        }
        for (_, entry) in self.entries.iter() {
            entry.accept(visit)?;
        }
        for (_, entry) in self.type_entries.iter() {
            entry.accept(visit)?;
        }
        for v in self
            .field_defaults
            .iter()
            .chain(self.type_field_defaults.iter())
        {
            v.accept(visit)?;
        }
        for v in self.native_supers.iter().chain(self.abstract_supers.iter()) {
            v.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&self) {
        // GC cannot safely clear annexes with outstanding immutable references
    }
}

impl<'v> ClassObject<'v> {
    fn type_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.annex();
        match me.type_entry(field) {
            // A static method takes no receiver, so it is its own value.
            Some(ClassTypeEntry::Method {
                value,
                inherited: false,
            }) => {
                strand.store(out, value);
                Ok(())
            }
            Some(ClassTypeEntry::Method { .. }) => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            Some(ClassTypeEntry::Field { slot, .. }) => {
                let slot = *slot;
                let borrow = this.borrow(strand)?;
                strand.store(out, &borrow.type_fields[slot]);
                Ok(())
            }
            Some(ClassTypeEntry::Property {
                property:
                    Property {
                        getter: Some(getter),
                        ..
                    },
                inherited,
            }) => strand.sync(async |strand| {
                isolated!(strand, {
                    if *inherited {
                        call!(strand, getter, &this, out)
                    } else {
                        call!(strand, getter, out)
                    }
                })
            }),
            Some(ClassTypeEntry::Property { .. }) => Err(Error::field(strand, field)),
            Some(ClassTypeEntry::Delegate(type_obj, _)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator).op_get(strand, field, out)
                })
            }
            None => Err(Error::field(strand, field)),
        }
    }
}

impl<'v> Protocol<'v> for ClassObject<'v> {
    fn op_inspect<'a>(this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        let annex = this.annex();
        Some(Inspect {
            is_abstract: false,
            is_final: annex.is_final.get(),
            members: &annex.members,
            type_members: &annex.type_members,
        })
    }

    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        ClassTypeProxy::create(strand, &this, out)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let this = this.annex();
        if let Some(module_name) = &this.module_name {
            crate::fmt!(strand, w, "<type {module_name}.{}>", this.name)
        } else {
            crate::fmt!(strand, w, "<type {}>", this.name)
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.annex();
        // Instance methods take priority, unbound
        match me.entry(field) {
            Some(ClassEntry::Method(v)) => {
                strand.store(&mut out, v);
                return Ok(());
            }
            // An inherited native method has no function of its own. Bind it to
            // the class, whose `op_mcall` takes the instance first, as a native
            // type object's unbound methods do.
            Some(
                ClassEntry::Abstract(_, MemberKind::Method)
                | ClassEntry::Delegate(_, MemberKind::Method),
            ) => {
                BoundMethod::create(strand, &this, field, out);
                return Ok(());
            }
            _ => (),
        }
        Self::type_get(this, strand, field, out)
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.annex();
        match me.type_entry(field) {
            Some(ClassTypeEntry::Field { slot, .. }) => {
                let slot = *slot;
                let mut borrow = this.borrow_mut(strand)?;
                // SAFETY: the value moves straight into the borrowed class
                borrow.type_fields[slot] = unsafe { value.take(strand) };
                Ok(())
            }
            Some(ClassTypeEntry::Property {
                property:
                    Property {
                        setter: Some(setter),
                        ..
                    },
                inherited,
            }) => with_slots!(strand, |mut discard| {
                strand.sync(async |strand| {
                    isolated!(strand, {
                        if *inherited {
                            call!(strand, setter, &this, value, &mut discard)
                        } else {
                            call!(strand, setter, value, &mut discard)
                        }
                    })
                })
            }),
            Some(ClassTypeEntry::Delegate(type_obj, _)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator).op_set(strand, field, value)
                })
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || this
                .annex()
                .supers
                .iter()
                .any(|sup| sup.op_subtype(strand, supertype))
            || supertype
                .eq(strand, &strand.singletons().value)
                .unwrap_or(false)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.annex();
        match method.tag() {
            sym::GET_METHOD => {
                unpack!(strand, args => obj, field);
                let field = field
                    .as_sym(strand)
                    .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
                if !obj.is_instance_of(strand, this.clone()) {
                    return Err(Error::type_error(strand, "invalid class object type"));
                }
                match me.entry(field) {
                    Some(ClassEntry::Field(_)) => {
                        let recv = obj
                            .downcast_ref(strand.builtin_types().class_instance)
                            .expect("object is a class instance");
                        let slot_idx = match recv.class().entry(field) {
                            Some(ClassEntry::Field(slot_idx)) => *slot_idx,
                            _ => {
                                return Err(Error::runtime(
                                    strand,
                                    "can't access plain superclass field that was overridden as a different member type in a dervied class",
                                ));
                            }
                        };
                        recv.field(strand, slot_idx, out);
                        Ok(())
                    }
                    Some(ClassEntry::Property(Property {
                        getter: Some(getter),
                        ..
                    })) => strand
                        .sync(async |strand| isolated!(strand, call!(strand, getter, obj, out))),
                    Some(ClassEntry::Method(_) | ClassEntry::Abstract(_, MemberKind::Method)) => {
                        BoundMethod::create(strand, obj, field, out);
                        Ok(())
                    }
                    Some(ClassEntry::Abstract(type_obj, _)) => {
                        Delegated::new(type_obj, &obj).op_get(strand, field, out)
                    }
                    Some(ClassEntry::Property { .. }) => Err(Error::field(strand, field)),
                    Some(ClassEntry::Delegate(_slot, _)) => {
                        let native = obj
                            .downcast_ref(strand.builtin_types().class_instance)
                            .ok_or_else(|| Error::type_error(strand, "invalid class object type"))?
                            .class()
                            .entry(field)
                            .and_then(|entry| match entry {
                                ClassEntry::Delegate(slot, _) => obj
                                    .downcast_ref(strand.builtin_types().class_instance)
                                    .and_then(|recv| recv.native(*slot)),
                                _ => None,
                            })
                            .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                        Delegated::new(native, &obj).op_get(strand, field, out)
                    }
                    _ => match me.entry_by_tag(sym::GET_METHOD) {
                        Some(ClassEntry::Method(v)) => {
                            strand.sync(async |strand| call!(strand, v, obj, field, out))
                        }
                        Some(ClassEntry::Delegate(slot, _)) => {
                            let recv = obj
                                .downcast_ref(strand.builtin_types().class_instance)
                                .ok_or_else(|| {
                                    Error::type_error(strand, "invalid class object type")
                                })?;
                            let native = recv.native(*slot).ok_or_else(|| {
                                Error::runtime(strand, "native slot uninitialized")
                            })?;
                            Delegated::new(native, &obj).op_get(strand, field, out)
                        }
                        _ => Err(Error::field(strand, field)),
                    },
                }
            }
            sym::SET_METHOD => {
                unpack!(strand, args => obj, field, value);
                let field = field
                    .as_sym(strand)
                    .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
                if !obj.is_instance_of(strand, this.clone()) {
                    return Err(Error::type_error(strand, "invalid class object type"));
                }
                match me.entry(field) {
                    Some(ClassEntry::Field(_)) => {
                        let recv = obj
                            .downcast_ref(strand.builtin_types().class_instance)
                            .ok_or_else(|| {
                                Error::type_error(strand, "invalid class object type")
                            })?;
                        let slot_idx = match recv.class().entry(field) {
                            Some(ClassEntry::Field(slot_idx)) => *slot_idx,
                            _ => return Err(Error::field(strand, field)),
                        };
                        recv.set_field(strand, slot_idx, value);
                        Ok(())
                    }
                    Some(ClassEntry::Property(Property {
                        setter: Some(setter),
                        ..
                    })) => with_slots!(strand, |mut tmp| {
                        strand.sync(async |strand| {
                            isolated!(strand, call!(strand, setter, obj, &value, &mut tmp))
                        })
                    }),
                    Some(
                        ClassEntry::Method(_)
                        | ClassEntry::Abstract(_, _)
                        | ClassEntry::Property { .. },
                    ) => Err(Error::field(strand, field)),
                    Some(ClassEntry::Delegate(slot, _)) => {
                        let recv = obj
                            .downcast_ref(strand.builtin_types().class_instance)
                            .ok_or_else(|| {
                                Error::type_error(strand, "invalid class object type")
                            })?;
                        let native = recv
                            .native(*slot)
                            .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                        Delegated::new(native, &obj).op_set(strand, field, value)
                    }
                    _ => match me.entry_by_tag(sym::SET_METHOD) {
                        Some(ClassEntry::Method(v)) => {
                            with_slots!(strand, |mut tmp| {
                                strand.sync(async |strand| {
                                    call!(strand, v, obj, field, &value, &mut tmp)
                                })
                            })
                        }
                        Some(ClassEntry::Delegate(slot, _)) => {
                            let recv = obj
                                .downcast_ref(strand.builtin_types().class_instance)
                                .ok_or_else(|| {
                                    Error::type_error(strand, "invalid class object type")
                                })?;
                            let native = recv.native(*slot).ok_or_else(|| {
                                Error::runtime(strand, "native slot uninitialized")
                            })?;
                            Delegated::new(native, &obj).op_set(strand, field, value)
                        }
                        _ => Err(Error::field(strand, field)),
                    },
                }
            }
            _ => {
                // Instance methods take priority, called unbound on an explicit instance.
                if let Some(v) = me.method(method) {
                    return v.op_call(strand, args, out).await;
                }
                let inherited = match me.entry(method) {
                    Some(ClassEntry::Abstract(type_obj, MemberKind::Method)) => {
                        Some((type_obj, false))
                    }
                    Some(ClassEntry::Delegate(slot, MemberKind::Method)) => {
                        Some((&me.native_supers[*slot], true))
                    }
                    _ => None,
                };
                if let Some((supertype, native)) = inherited {
                    unpack!(strand, args => instance, ...mut trailing);
                    if !instance.is_instance_of(strand, &this) {
                        return Err(Error::type_error(strand, "invalid class object type"));
                    }
                    if method.tag() == sym::INIT_METHOD {
                        trailing.prepend_self(strand, &instance);
                        return supertype.op_mcall(strand, method, trailing, out).await;
                    }
                    let receiver = if native {
                        // The instance may be of a subclass, whose native slots differ.
                        let borrow = instance
                            .downcast_ref(strand.builtin_types().class_instance)
                            .ok_or_else(|| {
                                Error::type_error(strand, "invalid class object type")
                            })?;
                        get_native_slot(borrow, supertype)
                            .ok_or_else(|| Error::type_error(strand, "native slot uninitialized"))?
                    } else {
                        supertype
                    };
                    return Delegated::new(receiver, &instance)
                        .op_mcall(strand, method, trailing, out)
                        .await;
                }
                match me.type_entry(method) {
                    // A class method receives the class it was reached through as
                    // its first argument. Inherited entries are copied into the
                    // subclass's own table, so `this` is already the derived class.
                    // A static method gets only its own arguments.
                    Some(ClassTypeEntry::Method { .. }) => {
                        let this = this.clone();
                        return with_slots!(strand, |mut func| {
                            let Some(ClassTypeEntry::Method { value, inherited }) =
                                this.annex().type_entry(method)
                            else {
                                unreachable!("checked above")
                            };
                            strand.store(Slot::reborrow(&mut func), value);
                            if *inherited {
                                args.prepend_self(strand, &this);
                            }
                            func.op_call(strand, args, out).await
                        });
                    }
                    // A class-level field or property holding a callable: read it,
                    // then call the result.
                    Some(ClassTypeEntry::Field { .. } | ClassTypeEntry::Property { .. }) => {
                        let this = this.clone();
                        return with_slots!(strand, |mut callee| {
                            Self::op_get(this, strand, method, Slot::reborrow(&mut callee))?;
                            callee.op_call(strand, args, out).await
                        });
                    }
                    Some(ClassTypeEntry::Delegate(type_obj, _)) => {
                        let delegator_obj = this.clone();
                        return with_slots!(strand, |mut ty, mut delegator| {
                            strand.store(Slot::reborrow(&mut ty), type_obj);
                            strand.store(Slot::reborrow(&mut delegator), &delegator_obj);
                            Delegated::new(&*ty, &delegator)
                                .op_mcall(strand, method, args, out)
                                .await
                        });
                    }
                    None => Err(Error::field(strand, method)),
                }
            }
        }
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // A class-level `(call)` replaces instantiation. `Type.(call) Class ...`
        // still reaches the default below, so an override can delegate to it.
        let call_sym = Sym::well_known(sym::CALL_METHOD);
        if matches!(
            this.annex().type_entry(call_sym),
            Some(ClassTypeEntry::Method { .. })
        ) {
            let this = this.clone();
            return with_slots!(strand, |mut func| {
                let Some(ClassTypeEntry::Method { value, inherited }) =
                    this.annex().type_entry(call_sym)
                else {
                    unreachable!("checked above")
                };
                strand.store(Slot::reborrow(&mut func), value);
                if *inherited {
                    args.prepend_self(strand, &this);
                }
                func.op_call(strand, args, out).await
            });
        }
        instantiate(this, strand, args, out).await
    }
}

/// Default instantiation: allocate the instance, seed its fields, and run `(init)`.
///
/// Reachable explicitly as `Type.(call) Class ...` even when the class overrides
/// `(call)` at the class level.
pub(crate) async fn instantiate<'v, 'a, 's>(
    this: Recv<'v, 'a, ClassObject<'v>>,
    strand: &'a mut Strand<'v, 's>,
    mut args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let me = this.annex();
    let native_slot_count = me.native_supers.len();
    let field_count = me.field_defaults.len();

    with_slots!(strand, |mut inst, tmp| {
        // The field values are rooted in slots while the thunks run
        with_slots_dynamic!(strand, field_count, |mut fields| {
            for (index, default) in me.field_defaults.iter().enumerate() {
                match default {
                    FieldDefault::Value(value) => strand.store(fields.at(index), value),
                    FieldDefault::Thunk(thunk) => call!(strand, thunk, fields.at(index))?,
                }
            }
            // Native super slots come first, uninitialized; the fields follow
            let ty = strand.builtin_types().class_instance;
            ty.create_tail(
                strand,
                ClassInstance(PhantomData),
                || ClassInstancePrefix {
                    // SAFETY: the instance is allocated, and the class, pinned as the
                    // receiver, is copied straight into its prefix
                    class: unsafe { this.to_strong() },
                    natives: (0..native_slot_count)
                        .map(|_| ValueCell::default())
                        .collect(),
                },
                // SAFETY: the instance is allocated; each field is copied from its
                // rooted slot into the annex with no allocation in between. The
                // slots release their references when the frame exits.
                fields
                    .iter_mut()
                    .map(|slot| unsafe { ValueCell::from_value(slot.dup()) }),
                &mut inst,
            );
        });

        let init = Sym::well_known(sym::INIT_METHOD);
        match me.entry(init) {
            Some(ClassEntry::Method(func)) => {
                args.prepend_self(strand, &inst);
                func.op_call(strand, args, tmp).await?;
            }
            Some(ClassEntry::Delegate(slot, MemberKind::Method)) => {
                // Call the native type's constructor before its instance slot
                // exists; `(init)` fills that slot on the derived instance.
                args.prepend_self(strand, &inst);
                me.native_supers[*slot]
                    .op_mcall(strand, init, args, tmp)
                    .await?;
            }
            Some(ClassEntry::Abstract(type_obj, MemberKind::Method)) => {
                args.prepend_self(strand, &inst);
                type_obj.op_mcall(strand, init, args, tmp).await?;
            }
            _ => {
                unpack!(strand, args =>);
            }
        }

        // Verify all native slots are initialized
        if inst
            .downcast_ref(strand.builtin_types().class_instance)
            .map(|inst| iter_natives(inst).count())
            .unwrap()
            != native_slot_count
        {
            return Err(Error::runtime(strand, "native supertypes not initialized"));
        }

        strand.store(out, inst);
        Ok(())
    })
}

/// What `type(SomeClass)` returns: a stand-in for the class's metaclass.
///
/// Its namespace is the class's class-level scope, which is the only way to name
/// an unbound class method (`type(Base).m $Derived`). Equality and hashing are
/// structural on the proxied class, so `type(A) == type(A)` holds without the
/// proxy having to be memoized.
pub(crate) struct ClassTypeProxy<'v> {
    class: Value<'v>,
}

impl<'v> ClassTypeProxy<'v> {
    pub(crate) fn create<'a>(
        strand: &mut Strand<'v, '_>,
        class: &Recv<'v, 'a, ClassObject<'v>>,
        out: Slot<'v, '_>,
    ) {
        let embryo = strand.vm().builtin_types().class_type_proxy.embryo(strand);
        // SAFETY: the proxy is allocated; the class, pinned as the receiver, is copied
        // straight into its payload
        let class = unsafe { Value::from_object(class.to_strong()) };
        embryo.finish_imm(strand, ClassTypeProxy { class }, out);
    }

    /// The proxied class's immutable half, for member lookup.
    fn class<'a>(&'a self, vm: &Vm<'v>) -> &'a ClassObjectAnnex<'v> {
        self.class
            .downcast_ref(vm.builtin_types().class_object)
            .expect("class type proxy wraps a class object")
            .annex()
    }
}

unsafe impl<'v> gc::ImmState for ClassTypeProxy<'v> {
    const CYCLIC: bool = true;
}

impl<'v> gc::Annex for ClassTypeProxy<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.class.accept(visit)
    }
}

pub(crate) type ClassTypeProxyObj<'v> = gc::Imm<ClassTypeProxy<'v>>;

impl<'v> Protocol<'v> for ClassTypeProxyObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        // Stop here rather than handing back a proxy of a proxy: `type(Type)` is
        // a fixpoint, so walking the type-of chain terminates.
        strand.store(out, &strand.singletons().type_obj)
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().type_obj)
                .unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().func)
                .unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().value)
                .unwrap_or(false)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().class_type_proxy) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let same = with_pin!(strand, other, |other| {
            other.get().class.repr_eq(&this.get().class)
        });
        strand.store(&mut out, Value::from_bool(same));
        Ok(())
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        this.get().class.op_hash(strand, hasher)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let class = this.get().class(strand.vm());
        if let Some(module_name) = &class.module_name {
            crate::fmt!(strand, w, "<type of {module_name}.{}>", class.name)
        } else {
            crate::fmt!(strand, w, "<type of {}>", class.name)
        }
    }

    /// Class-level methods and protocol accessors are unbound. Class fields
    /// themselves are reached through the class object, as `Class.field`.
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match this.get().class(strand.vm()).type_entry(field) {
            Some(ClassTypeEntry::Method { value, .. }) => {
                strand.store(out, value);
                Ok(())
            }
            Some(ClassTypeEntry::Delegate(_, MemberKind::Method)) => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let class = this.get().class(strand.vm());
        if matches!(method.tag(), sym::GET_METHOD | sym::SET_METHOD)
            && matches!(class.type_entry(method),
                Some(ClassTypeEntry::Delegate(supertype, MemberKind::Method))
                    if supertype.repr_eq(TypeObject::Type.value(strand.vm())))
        {
            unpack!(strand, args => receiver, field, ...trailing);
            let class = receiver
                .downcast_ref(strand.builtin_types().class_object)
                .ok_or_else(|| Error::type_error(strand, "invalid class object type"))?;
            if !receiver.op_subtype(strand, &this.get().class) {
                return Err(Error::type_error(strand, "invalid class object type"));
            }
            let field = field
                .as_sym(strand)
                .ok_or_else(|| Error::type_error(strand, "expected a symbol"))?;
            if method.tag() == sym::GET_METHOD {
                unpack!(strand, trailing =>);
                return with_pin!(strand, class, |class| {
                    ClassObject::type_get(Recv::new(class), strand, field, out)
                });
            }
            unpack!(strand, trailing => value);
            return with_slots!(strand, |mut slot| {
                strand.store(Slot::reborrow(&mut slot), value);
                with_pin!(strand, class, |class| {
                    ClassObject::op_set(Recv::new(class), strand, field, slot)
                })
            });
        }
        match class.type_entry(method) {
            Some(ClassTypeEntry::Method { value, .. }) => value.op_call(strand, args, out).await,
            Some(ClassTypeEntry::Delegate(supertype, MemberKind::Method)) => {
                unpack!(strand, args => receiver, ...trailing);
                if receiver
                    .downcast_ref(strand.builtin_types().class_object)
                    .is_none()
                    || !receiver.op_subtype(strand, &this.get().class)
                {
                    return Err(Error::type_error(strand, "invalid class object type"));
                }
                Delegated::new(supertype, &receiver)
                    .op_mcall(strand, method, trailing, out)
                    .await
            }
            _ => Err(Error::field(strand, method)),
        }
    }
}

/// The fixed part of a [`ClassInstance`] annex.
pub(crate) struct ClassInstancePrefix<'v> {
    pub(crate) class: GcObj<'v, ClassObject<'v>>,
    /// Native super slots, out of line so that a native found in one is reachable
    /// without pinning the instance.
    natives: alias::Box<[ValueCell<'v>]>,
}

/// A class instance's state: its class and native super slots, then fields as
/// cells in the tail.
pub(crate) type ClassInstanceAnnex<'v> = gc::Tail<ClassInstancePrefix<'v>, [ValueCell<'v>]>;

impl<'v> ClassInstanceAnnex<'v> {
    /// Native super slot `idx`, or `None` while uninitialized.
    ///
    /// The reference may be held across awaits: a native slot is written once,
    /// from nil, by [`Self::init_native`], and the collector never clears it.
    pub(crate) fn native(&self, idx: usize) -> Option<&Value<'v>> {
        native_value(&self.prefix.natives[idx])
    }

    /// The initialized native super slots.
    pub(crate) fn natives(&self) -> impl Iterator<Item = &Value<'v>> {
        self.prefix.natives.iter().filter_map(native_value)
    }

    /// Moves `native`, which must not be nil, into native super slot `idx`.
    /// Returns `false`, leaving `native` in place, if the slot is already initialized.
    fn init_native(&self, alloc: &mut impl Alloc<'v>, idx: usize, native: Slot<'v, '_>) -> bool {
        debug_assert!(!native.is_nil());
        if self.native(idx).is_some() {
            return false;
        }
        // Safety: an uninitialized slot is never borrowed
        alloc.store(unsafe { self.prefix.natives[idx].slot() }, native);
        true
    }

    /// Field `idx`'s cell, to read as an [`Input`].
    pub(crate) fn field(&self, idx: usize) -> &ValueCell<'v> {
        &self.tail[idx]
    }

    /// Stores `value` into field `idx`. The old value is dropped after the write.
    pub(crate) fn set_field(&self, alloc: &mut impl Alloc<'v>, idx: usize, value: impl Input<'v>) {
        // Safety: as for `field`
        alloc.store(unsafe { self.tail[idx].slot() }, value)
    }
}

impl<'v> gc::TailAnnex<ValueCell<'v>> for ClassInstancePrefix<'v> {
    fn accept(&self, tail: &[ValueCell<'v>], visit: &mut dyn Visit) -> ControlFlow<()> {
        self.class.accept(visit)?;
        for cell in self.natives.iter().chain(tail) {
            cell.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    // Only fields are cleared: native slots are borrowed across awaits. The class
    // is immutable.
    fn clear(&self, tail: &[ValueCell<'v>]) {
        for cell in tail {
            // Safety: the collector clears only unreachable instances
            unsafe { cell.clear() }
        }
    }
}

/// A class instance. Its state is in its annex.
pub(crate) struct ClassInstance<'v>(PhantomData<Value<'v>>);

unsafe impl<'v> Collect for ClassInstance<'v> {
    const CYCLIC: bool = true;
    type Annex = ClassInstanceAnnex<'v>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

/// Given a [`ClassInstance`] GC borrow (as returned by [`Value::downcast_ref`]) and a
/// native super's type object, returns a reference to the initialized native slot value,
/// if any.  The returned reference's lifetime is linked to that of the borrow.
pub(crate) fn get_native_slot<'v, 'a>(
    obj: &'a GcObj<'v, ClassInstance<'v>>,
    type_obj: &Value<'v>,
) -> Option<&'a Value<'v>> {
    let slot = obj
        .class()
        .native_supers
        .iter()
        .position(|s| s.repr_eq(type_obj))?;
    native_value(&native_slots(obj)[slot])
}

/// Returns an iterator over all initialized native values in a [`ClassInstance`].
/// The lifetime of each yielded `&Value<'v>` is linked to that of `obj`.
pub(crate) fn iter_natives<'v, 'a>(
    obj: &'a GcObj<'v, ClassInstance<'v>>,
) -> impl Iterator<Item = &'a Value<'v>> {
    native_slots(obj).iter().filter_map(native_value)
}

impl<'v> GcObj<'v, ClassInstance<'v>> {
    /// The annex of the instance's class, which lives out of line.
    pub(crate) fn class(&self) -> &ClassObjectAnnex<'v> {
        // SAFETY: a transient pin reaches the class's out-of-line annex, which stays
        // put and which the instance's immutable class reference keeps alive
        unsafe { &*ptr::from_ref(self.pin_unchecked().annex().prefix.class.annex()) }
    }

    /// Native super slot `idx`, or `None` while uninitialized; see
    /// [`ClassInstanceAnnex::native`].
    pub(crate) fn native(&self, idx: usize) -> Option<&Value<'v>> {
        native_value(&native_slots(self)[idx])
    }

    /// Copy field `idx` into `out`.
    pub(crate) fn field(&self, alloc: &mut impl Alloc<'v>, idx: usize, out: impl Output<'v>) {
        // SAFETY: a transient pin; duplicating a cell cannot collect
        let cell = unsafe { self.pin_unchecked() }.annex().field(idx);
        alloc.store(out, cell)
    }

    /// Stores `value` into field `idx`; see [`ClassInstanceAnnex::set_field`].
    pub(crate) fn set_field(&self, alloc: &mut impl Alloc<'v>, idx: usize, value: impl Input<'v>) {
        // SAFETY: a transient pin; dropping the old value may run a finalizer, which
        // has no strand and cannot collect
        unsafe { self.pin_unchecked() }
            .annex()
            .set_field(alloc, idx, value)
    }
}

/// The native super slots, which live out of line.
fn native_slots<'v, 'a>(obj: &'a GcObj<'v, ClassInstance<'v>>) -> &'a [ValueCell<'v>] {
    obj.project(|p| &p.annex().prefix.natives)
}

/// A native super slot's value, or `None` while uninitialized.
///
/// The reference may be held across awaits: a native slot is written once, from
/// nil, by [`ClassInstanceAnnex::init_native`], and the collector never clears it.
fn native_value<'a, 'v>(cell: &'a ValueCell<'v>) -> Option<&'a Value<'v>> {
    // Safety: see above; nothing writes an initialized native slot
    let value = unsafe { cell.as_ref() };
    (!value.is_nil()).then_some(value)
}

fn class_sync_binary_op<'v, 'a, 's>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    method_sym: sym::Tag,
    other: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let entry = this.annex().prefix.class.annex().entry_by_tag(method_sym);
    if let Some(entry) = entry {
        class_sync_binary_op_entry(this, strand, method_sym, other, entry, out)
    } else {
        Err(Error::unsupported(strand, "operator"))
    }
}

fn class_dispatch_binary_op<'v, 'a, 's>(
    receiver: impl Dispatch<'v, 'a>,
    strand: &'a mut Strand<'v, 's>,
    method_sym: sym::Tag,
    other: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match method_sym {
        sym::ADD_METHOD => receiver.op_add(strand, other, out),
        sym::RADD_METHOD => receiver.op_radd(strand, other, out),
        sym::SUB_METHOD => receiver.op_sub(strand, other, out),
        sym::RSUB_METHOD => receiver.op_rsub(strand, other, out),
        sym::MUL_METHOD => receiver.op_mul(strand, other, out),
        sym::RMUL_METHOD => receiver.op_rmul(strand, other, out),
        sym::DIV_METHOD => receiver.op_div(strand, other, out),
        sym::RDIV_METHOD => receiver.op_rdiv(strand, other, out),
        sym::EDIV_METHOD => receiver.op_ediv(strand, other, out),
        sym::REDIV_METHOD => receiver.op_rediv(strand, other, out),
        sym::MOD_METHOD => receiver.op_mod(strand, other, out),
        sym::RMOD_METHOD => receiver.op_rmod(strand, other, out),
        sym::BAND_METHOD => receiver.op_band(strand, other, out),
        sym::RBAND_METHOD => receiver.op_rband(strand, other, out),
        sym::BOR_METHOD => receiver.op_bor(strand, other, out),
        sym::RBOR_METHOD => receiver.op_rbor(strand, other, out),
        sym::BXOR_METHOD => receiver.op_bxor(strand, other, out),
        sym::RBXOR_METHOD => receiver.op_rbxor(strand, other, out),
        sym::SHL_METHOD => receiver.op_shl(strand, other, out),
        sym::RSHL_METHOD => receiver.op_rshl(strand, other, out),
        sym::SHR_METHOD => receiver.op_shr(strand, other, out),
        sym::RSHR_METHOD => receiver.op_rshr(strand, other, out),
        sym::EQ_METHOD => receiver.op_eq(strand, other, out),
        sym::REQ_METHOD => receiver.op_req(strand, other, out),
        sym::NE_METHOD => receiver.op_ne(strand, other, out),
        sym::RNE_METHOD => receiver.op_rne(strand, other, out),
        sym::LT_METHOD => receiver.op_lt(strand, other, out),
        _ => unreachable!(),
    }
}

fn class_sync_binary_op_entry<'v, 'a, 's>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    method_sym: sym::Tag,
    other: &Value<'v>,
    entry: &ClassEntry<'v>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let annex = this.annex();
    match entry {
        ClassEntry::Method(v) => {
            strand.sync(async |strand| call!(strand, v, &this, other, &mut out))
        }
        ClassEntry::Delegate(slot, _) => {
            let native = annex
                .native(*slot)
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
            with_slots!(strand, |mut delegator| {
                strand.store(Slot::reborrow(&mut delegator), &this);
                let native = Delegated::new(native, &delegator);
                class_dispatch_binary_op(native, strand, method_sym, other, out)
            })
        }
        ClassEntry::Abstract(type_obj, _) => with_slots!(strand, |mut delegator| {
            strand.store(Slot::reborrow(&mut delegator), &this);
            let abstract_type = Delegated::new(type_obj, &delegator);
            class_dispatch_binary_op(abstract_type, strand, method_sym, other, out)
        }),
        _ => Err(Error::runtime(
            strand,
            "invalid class binary operator entry",
        )),
    }
}

fn class_sync_commutative_op<'v, 'a, 's>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    reverse: sym::Tag,
    other: &Value<'v>,
    out: Slot<'v, '_>,
    fallback: for<'b> fn(
        Recv<'v, 'a, ClassInstance<'v>>,
        &mut Strand<'v, 's>,
        &Value<'v>,
        Slot<'v, 'b>,
    ) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    let entry = this.annex().prefix.class.annex().entry_by_tag(reverse);
    if let Some(entry) = entry {
        class_sync_binary_op_entry(this, strand, reverse, other, entry, out)
    } else {
        fallback(this, strand, other, out)
    }
}

// Inherited comparisons use the delegated operation's reflected fallback,
// which passes the native receiver when the other operand cannot compare
// against the Do subclass itself.
fn class_sync_reverse_comparison<'v, 'a, 's>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    other: &Value<'v>,
    inclusive: bool,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let annex = this.annex();
    if let Some(ClassEntry::Delegate(slot, _)) =
        annex.prefix.class.annex().entry_by_tag(sym::LT_METHOD)
    {
        let native = annex
            .native(*slot)
            .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
        return with_slots!(strand, |mut delegator| {
            strand.store(Slot::reborrow(&mut delegator), &this);
            let native = Delegated::new(native, &delegator);
            if inclusive {
                native.op_gte(strand, other, out)
            } else {
                native.op_gt(strand, other, out)
            }
        });
    }
    with_slots!(strand, |mut receiver| {
        strand.store(&mut receiver, this);
        if inclusive {
            other.op_lte_direct(strand, &receiver, out)
        } else {
            other.op_lt_direct(strand, &receiver, out)
        }
    })
}

fn class_sync_unary_op<'v, 'a, 's>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    method_sym: sym::Tag,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let annex = this.annex();
    match annex.prefix.class.annex().entry_by_tag(method_sym) {
        Some(ClassEntry::Method(v)) => {
            strand.sync(async |strand| call!(strand, v, &this, &mut out))
        }
        Some(ClassEntry::Delegate(slot, _)) => {
            let native = annex
                .native(*slot)
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
            with_slots!(strand, |mut delegator| {
                strand.store(Slot::reborrow(&mut delegator), &this);
                let native = Delegated::new(native, &delegator);
                match method_sym {
                    sym::NEG_METHOD => native.op_neg(strand, out),
                    sym::BNOT_METHOD => native.op_bnot(strand, out),
                    _ => unreachable!(),
                }
            })
        }
        _ => Err(Error::unsupported(strand, "operator")),
    }
}

/// Formats an instance of a class that defines no `(fmt)` method: the string
/// conversion selected by `spec.kind`, padded to the requested layout.
fn default_fmt<'v, 's>(
    this: Recv<'v, '_, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    spec: &Spec,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    use crate::value::fmt::{Fill, Kind, Pad};

    let kind = spec
        .kind
        .ok_or_else(|| crate::value::fmt::unresolved_kind(strand))?;
    if !kind.is_text() {
        let name = &this.annex().prefix.class.annex().name;
        return Err(Error::type_error(
            strand,
            format!("{name}: unsupported format kind `:{}`", kind.symbol()),
        ));
    }
    if spec.sign.is_some() || spec.alt || spec.fill == Fill::Zero {
        return Err(Error::type_error(strand, "unsupported format option"));
    }
    let mut pad = Pad::new(*spec, w);
    match kind {
        Kind::Str => ClassInstance::op_display(this, strand, &mut pad)?,
        Kind::Dbg => ClassInstance::op_debug(this, strand, &mut pad)?,
        Kind::Verbatim => ClassInstance::op_verbatim(this, strand, &mut pad)?,
        _ => unreachable!(),
    }
    pad.finish(strand)
}

impl<'v> Protocol<'v> for ClassInstance<'v> {
    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::FMT_METHOD) {
            Some(ClassEntry::Method(v)) => {
                with_slots!(strand, |mut result, mut reified| {
                    crate::stdlib::fmt::reify_spec(strand, *spec, Slot::reborrow(&mut reified));
                    strand.sync(async |strand| call!(strand, v, &this, &reified, &mut result))?;
                    let result = result
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected Str result"))?;
                    with_str!(strand, result, |result| crate::fmt!(strand, w, "{result}"))
                })
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator).op_fmt(strand, spec, w)
                })
            }
            _ => default_fmt(this, strand, spec, w),
        }
    }

    fn op_fill<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let idx = annex
            .prefix
            .class
            .annex()
            .native_supers
            .iter()
            .position(|sup| sup.repr_eq(type_obj))
            .ok_or_else(|| {
                Error::type_error(strand, "not a concrete native super of this class")
            })?;

        // Check if slot is already set with the same value (idempotent for diamond inheritance)
        if let Some(existing) = annex.native(idx) {
            if existing.repr_eq(&native) {
                strand.store(native, Nil);
                return Ok(());
            } else {
                return Err(Error::runtime(
                    strand,
                    "native slot already initialized with a different value",
                ));
            }
        }

        if annex.init_native(strand, idx, native) {
            Ok(())
        } else {
            Err(Error::state_error(
                strand,
                "native slot already initialized",
            ))
        }
    }

    fn op_provide<'a, 's>(
        this: Recv<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        let (strand, answer) = request.parts();
        with_slots!(strand, |mut delegator| {
            strand.store(Slot::reborrow(&mut delegator), &this);
            for native in this.annex().natives() {
                if answer.is_done() {
                    break;
                }
                Delegated::new(native, &delegator)
                    .op_provide(&mut Request::new(strand, &mut *answer))?;
            }
            for sup in this.annex().prefix.class.annex().abstract_supers.iter() {
                if answer.is_done() {
                    break;
                }
                Delegated::new(sup, &delegator)
                    .op_provide(&mut Request::new(strand, &mut *answer))?;
            }
            Ok(())
        })
    }

    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        // SAFETY: the reference moves straight into `out`
        let class = unsafe { Value::from_object(this.annex().prefix.class.dup()) };
        strand.store(out, class)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let res = {
            let w = &mut *w;
            let annex = this.annex();
            match annex.prefix.class.annex().entry_by_tag(sym::STR_METHOD) {
                Some(ClassEntry::Method(v)) => with_slots!(strand, |mut result| {
                    strand.sync(async |strand| call!(strand, v, &this, &mut result))?;
                    let result = result
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected Str result"))?;
                    with_str!(strand, result, |result| crate::fmt!(strand, w, "{result}"))?;
                    Ok(false)
                })?,
                Some(ClassEntry::Delegate(slot, _)) => {
                    let native = annex
                        .native(*slot)
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    with_slots!(strand, |mut delegator| {
                        strand.store(Slot::reborrow(&mut delegator), &this);
                        Delegated::new(native, &delegator).op_display(strand, w)
                    })?;
                    false
                }
                _ => true,
            }
        };
        if res {
            Self::op_debug(this, strand, w)
        } else {
            Ok(())
        }
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::DBG_METHOD) {
            Some(ClassEntry::Method(v)) => with_slots!(strand, |mut result| {
                strand.sync(async |strand| call!(strand, v, &this, &mut result))?;
                let result = result
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected Str result"))?;
                with_str!(strand, result, |result| crate::fmt!(strand, w, "{result}"))
            }),
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator).op_debug(strand, w)
                })
            }
            _ => {
                if let Some(module) = &annex.prefix.class.annex().module_name {
                    crate::fmt!(strand, w, "<{module}.{}>", annex.prefix.class.annex().name)
                } else {
                    crate::fmt!(strand, w, "<{}>", annex.prefix.class.annex().name)
                }
            }
        }
    }

    fn op_verbatim<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let res = {
            let w = &mut *w;
            let this = this.clone();
            let annex = this.annex();
            match annex
                .prefix
                .class
                .annex()
                .entry_by_tag(sym::VERBATIM_METHOD)
            {
                Some(ClassEntry::Method(v)) => with_slots!(strand, |mut result| {
                    strand.sync(async |strand| call!(strand, v, &this, &mut result))?;
                    let result = result
                        .as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected Str result"))?;
                    with_str!(strand, result, |result| {
                        crate::fmt!(strand, w, "{result}")
                    })?;
                    Ok(false)
                })?,
                Some(ClassEntry::Delegate(slot, _)) => {
                    let native = annex
                        .native(*slot)
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    with_slots!(strand, |mut delegator| {
                        strand.store(Slot::reborrow(&mut delegator), &this);
                        Delegated::new(native, &delegator).op_verbatim(strand, w)
                    })?;
                    false
                }
                _ => true,
            }
        };
        if res {
            Self::op_display(this, strand, w)
        } else {
            Ok(())
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry(field) {
            Some(ClassEntry::Field(slot_idx)) => {
                strand.store(out, annex.field(*slot_idx));
                Ok(())
            }
            Some(ClassEntry::Property(Property {
                getter: Some(getter),
                ..
            })) => strand.sync(async |strand| isolated!(strand, call!(strand, getter, &this, out))),
            Some(ClassEntry::Method(_) | ClassEntry::Abstract(_, MemberKind::Method)) => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            Some(ClassEntry::Abstract(type_obj, _)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator).op_get(strand, field, out)
                })
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator).op_get(strand, field, out)
                })
            }
            _ => match annex.prefix.class.annex().entry_by_tag(sym::GET_METHOD) {
                Some(ClassEntry::Method(v)) => {
                    strand.sync(async |strand| call!(strand, v, &this, field, out))
                }
                Some(ClassEntry::Delegate(slot, _)) => {
                    let native = annex
                        .native(*slot)
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    with_slots!(strand, |mut delegator| {
                        strand.store(Slot::reborrow(&mut delegator), &this);
                        Delegated::new(native, &delegator).op_get(strand, field, out)
                    })
                }
                _ => Err(Error::field(strand, field)),
            },
        }
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry(field) {
            Some(ClassEntry::Field(slot_idx)) => {
                annex.set_field(strand, *slot_idx, value);
                Ok(())
            }
            Some(ClassEntry::Property(Property {
                setter: Some(setter),
                ..
            })) => with_slots!(strand, |mut tmp| {
                strand.sync(async |strand| {
                    isolated!(strand, call!(strand, setter, &this, &value, &mut tmp))
                })
            }),
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator).op_set(strand, field, value)
                })
            }
            _ => match annex.prefix.class.annex().entry_by_tag(sym::SET_METHOD) {
                Some(ClassEntry::Method(v)) => with_slots!(strand, |mut tmp| {
                    strand.sync(async |strand| call!(strand, v, &this, field, &value, &mut tmp))
                }),
                Some(ClassEntry::Delegate(slot, _)) => {
                    let native = annex
                        .native(*slot)
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    with_slots!(strand, |mut delegator| {
                        strand.store(Slot::reborrow(&mut delegator), &this);
                        Delegated::new(native, &delegator).op_set(strand, field, value)
                    })
                }
                _ => Err(Error::field(strand, field)),
            },
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // `(get)` and `(set)` are field access by name, which falls back to the
        // class's own `(get)` and `(set)` only for a name it has no member of
        if matches!(method.tag(), sym::GET_METHOD | sym::SET_METHOD) {
            return instance_mcall_fallback(strand, &this, method, args, out);
        }
        let class = this.annex().prefix.class.annex();
        match class.entry(method) {
            Some(ClassEntry::Method(v)) => {
                args.prepend_self(strand, &this);
                return v.op_call(strand, args, out).await;
            }
            Some(ClassEntry::Field(slot_idx)) => {
                return with_slots!(strand, |mut callable| {
                    strand.store(&mut callable, this.annex().field(*slot_idx));
                    callable.op_call(strand, args, out).await
                });
            }
            Some(ClassEntry::Property(Property {
                getter: Some(getter),
                ..
            })) => {
                return with_slots!(strand, |mut callable| {
                    isolated!(strand, call!(strand, getter, &this, &mut callable))?;
                    callable.op_call(strand, args, out).await
                });
            }
            Some(ClassEntry::Property(Property { getter: None, .. })) => {}
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = this
                    .annex()
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                return with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_mcall(strand, method, args, out)
                        .await
                });
            }
            Some(ClassEntry::Abstract(type_obj, _)) => {
                return with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator)
                        .op_mcall(strand, method, args, out)
                        .await
                });
            }
            None => {}
        }

        if is_special_mcall(method.tag()) {
            return instance_mcall_fallback(strand, &this, method, args, out);
        }

        match class.entry_by_tag(sym::GET_METHOD) {
            Some(ClassEntry::Method(v)) => {
                with_slots!(strand, |mut callable| {
                    call!(strand, v, &this, method, &mut callable)?;
                    callable.op_call(strand, args, out).await
                })
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = this
                    .annex()
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_mcall(strand, method, args, out)
                        .await
                })
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::CALL_METHOD) {
            Some(ClassEntry::Method(v)) => {
                args.prepend_self(strand, &this);
                v.op_call(strand, args, out).await
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_call(strand, args, out)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "call")),
        }
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::UNPACK_METHOD) {
            Some(ClassEntry::Method(v)) => {
                with_slots!(strand, |mut proxy| {
                    call!(strand, v, &this, &mut proxy)?;
                    proxy.op_unpack(strand, unpack).await
                })
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_unpack(strand, unpack)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "unpacking")),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::ITER_METHOD) {
            Some(ClassEntry::Method(v)) => call!(strand, v, &this, out),
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_iter(strand, out)
                        .await
                })
            }
            Some(ClassEntry::Abstract(type_obj, MemberKind::Method)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator)
                        .op_iter(strand, out)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "iteration")),
        }
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::SINK_METHOD) {
            Some(ClassEntry::Method(v)) => call!(strand, v, &this, out),
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_sink(strand, out)
                        .await
                })
            }
            Some(ClassEntry::Abstract(type_obj, MemberKind::Method)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator)
                        .op_sink(strand, out)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "sink")),
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::SPREAD_METHOD) {
            Some(ClassEntry::Method(v)) => {
                with_slots!(strand, |mut proxy| {
                    call!(strand, v, &this, &mut proxy)?;
                    proxy.op_spread(strand, context, sink).await
                })
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_spread(strand, context, sink)
                        .await
                })
            }
            Some(ClassEntry::Abstract(type_obj, MemberKind::Method)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator)
                        .op_spread(strand, context, sink)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "spread")),
        }
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::NEXT_METHOD) {
            Some(ClassEntry::Method(v)) => match call!(strand, v, &this, out) {
                Ok(()) => Ok(true),
                Err(err) if err.kind() == ErrorKind::IterStop => Ok(false),
                Err(err) => Err(err),
            },
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_next(strand, out)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "`next`")),
        }
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::PUT_METHOD) {
            Some(ClassEntry::Method(v)) => {
                with_slots!(strand, |tmp| call!(strand, v, &this, &item, tmp))
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator)
                        .op_put(strand, item)
                        .await
                })
            }
            Some(ClassEntry::Abstract(type_obj, MemberKind::Method)) => {
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(type_obj, &delegator)
                        .op_put(strand, item)
                        .await
                })
            }
            _ => Err(Error::unsupported(strand, "`put`")),
        }
    }

    fn op_add<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::ADD_METHOD, other, out)
    }
    fn op_radd<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::RADD_METHOD, other, out, Self::op_add)
    }

    fn op_sub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::SUB_METHOD, other, out)
    }

    fn op_rsub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::RSUB_METHOD, other, out)
    }

    fn op_mul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::MUL_METHOD, other, out)
    }
    fn op_rmul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::RMUL_METHOD, other, out, Self::op_mul)
    }

    fn op_div<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::DIV_METHOD, other, out)
    }

    fn op_rdiv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::RDIV_METHOD, other, out)
    }

    fn op_ediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::EDIV_METHOD, other, out)
    }

    fn op_rediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::REDIV_METHOD, other, out)
    }

    fn op_mod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::MOD_METHOD, other, out)
    }

    fn op_rmod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::RMOD_METHOD, other, out)
    }

    fn op_band<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::BAND_METHOD, other, out)
    }
    fn op_rband<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::RBAND_METHOD, other, out, Self::op_band)
    }

    fn op_bor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::BOR_METHOD, other, out)
    }
    fn op_rbor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::RBOR_METHOD, other, out, Self::op_bor)
    }

    fn op_bxor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::BXOR_METHOD, other, out)
    }
    fn op_rbxor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::RBXOR_METHOD, other, out, Self::op_bxor)
    }

    fn op_shl<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::SHL_METHOD, other, out)
    }
    fn op_rshl<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::RSHL_METHOD, other, out)
    }

    fn op_shr<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::SHR_METHOD, other, out)
    }
    fn op_rshr<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::RSHR_METHOD, other, out)
    }

    fn op_neg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_unary_op(this, strand, sym::NEG_METHOD, out)
    }

    fn op_bnot<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_unary_op(this, strand, sym::BNOT_METHOD, out)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::EQ_METHOD, other, out)
    }
    fn op_ne<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let class = this.annex().prefix.class.annex();
        let has_method = matches!(
            class.entry_by_tag(sym::NE_METHOD),
            Some(ClassEntry::Method(_))
        );
        let has_native = matches!(
            class.entry_by_tag(sym::NE_METHOD),
            Some(ClassEntry::Delegate(_, _))
        );
        let overrides_eq = matches!(
            class.entry_by_tag(sym::EQ_METHOD),
            Some(ClassEntry::Method(_))
        );
        if has_method || (has_native && !overrides_eq) {
            class_sync_binary_op(this, strand, sym::NE_METHOD, other, out)
        } else {
            Self::op_eq(this, strand, other, Slot::reborrow(&mut out))?;
            let equal = out.op_bool(strand)?;
            strand.store(&mut out, Value::from_bool(!equal));
            Ok(())
        }
    }
    fn op_rne<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::RNE_METHOD, other, out, Self::op_ne)
    }
    fn op_req<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_commutative_op(this, strand, sym::REQ_METHOD, other, out, Self::op_eq)
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_binary_op(this, strand, sym::LT_METHOD, other, out)
    }

    fn op_gt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_reverse_comparison(this, strand, other, false, out)
    }

    fn op_gte<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        class_sync_reverse_comparison(this, strand, other, true, out)
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        let this = this.clone();
        let annex = this.annex();
        match annex.prefix.class.annex().entry_by_tag(sym::BOOL_METHOD) {
            Some(ClassEntry::Method(v)) => with_slots!(strand, |mut result| {
                strand.sync(async |strand| call!(strand, v, &this, &mut result))?;
                result.op_bool(strand)
            }),
            Some(ClassEntry::Delegate(slot, _)) => annex
                .native(*slot)
                .map(|native| {
                    with_slots!(strand, |mut delegator| {
                        strand.store(Slot::reborrow(&mut delegator), &this);
                        Delegated::new(native, &delegator).op_bool(strand)
                    })
                })
                .unwrap_or(Ok(true)),
            _ => Ok(true),
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex
            .prefix
            .class
            .annex()
            .entry(Sym::well_known(sym::INDEX_METHOD))
        {
            Some(ClassEntry::Method(v)) => {
                strand.sync(async |strand| call!(strand, v, &this, index, out))
            }
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator).op_index(strand, index, out)
                })
            }
            _ => Err(Error::type_error(strand, "indexing not supported")),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex
            .prefix
            .class
            .annex()
            .entry(Sym::well_known(sym::ASSIGN_METHOD))
        {
            Some(ClassEntry::Method(v)) => with_slots!(strand, |mut tmp| {
                strand.sync(async |strand| call!(strand, v, &this, &index, &value, &mut tmp))
            }),
            Some(ClassEntry::Delegate(slot, _)) => {
                let native = annex
                    .native(*slot)
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                with_slots!(strand, |mut delegator| {
                    strand.store(Slot::reborrow(&mut delegator), &this);
                    Delegated::new(native, &delegator).op_assign(strand, index, value)
                })
            }
            _ => Err(Error::type_error(strand, "index assignment not supported")),
        }
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        use std::hash::Hash;
        let did_hash = {
            let hasher = &mut *hasher;
            let this = this.clone();
            let annex = this.annex();
            match annex
                .prefix
                .class
                .annex()
                .entry(Sym::well_known(sym::HASH_METHOD))
            {
                Some(ClassEntry::Method(v)) => with_slots!(strand, |mut result| {
                    strand.sync(async |strand| call!(strand, v, &this, &mut result))?;
                    let v = result.to_int(strand).map_err(|_| {
                        Error::type_error(strand, "expected Int result from (hash)")
                    })?;
                    v.hash(hasher);
                    Ok(true)
                })?,
                Some(ClassEntry::Delegate(slot, _)) => {
                    let native = annex
                        .native(*slot)
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    with_slots!(strand, |mut delegator| {
                        strand.store(Slot::reborrow(&mut delegator), &this);
                        Delegated::new(native, &delegator).op_hash(strand, hasher)
                    })?;
                    true
                }
                _ => false,
            }
        };
        if !did_hash {
            this.receiver.into_raw().hash(hasher);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_support::{with_cells, with_vm},
        vm::AllocExt,
    };

    #[test]
    fn create_roots_its_tables_across_collections() {
        with_vm::<4, _>(
            async |strand, [mut method, mut field, mut class, mut out]| {
                strand.intern_sym("m", Slot::reborrow(&mut method));

                strand.intern_sym("x", Slot::reborrow(&mut field));

                with_cells!(strand, 7, |slots| {
                    strand.store(unsafe { slots[1].slot() }, "C");
                    strand.store(unsafe { slots[2].slot() }, "test");
                    strand.store(unsafe { slots[3].slot() }, &method);
                    strand.native_function(
                        async |strand, args, out| {
                            unpack!(strand, args =>);
                            strand.store(out, Nil);
                            Ok(())
                        },
                        unsafe { slots[4].slot() },
                    );
                    strand.store(unsafe { slots[5].slot() }, &field);
                    strand.native_function(
                        async |strand, args, out| {
                            unpack!(strand, args =>);
                            // The method was taken from its argument, so only the class's
                            // tables hold it
                            strand.vm().collect_full();
                            strand.new_array(out);
                            Ok(())
                        },
                        unsafe { slots[6].slot() },
                    );
                    let sig = [
                        None,
                        None,
                        Some(Sym::well_known(sym::METHOD)),
                        None,
                        Some(Sym::well_known(sym::CLASS_FIELD_THUNK)),
                        None,
                    ];
                    let args = unsafe { Args::new(slots, &sig, 1) };
                    create(strand, args, Slot::reborrow(&mut class))
                        .await
                        .unwrap();
                });
                strand.vm().collect_full();

                let method = method.as_sym(strand).unwrap();
                let cls = class
                    .downcast_ref(strand.builtin_types().class_object)
                    .unwrap();
                assert!(matches!(
                    cls.annex().entry(method),
                    Some(ClassEntry::Method(func)) if is_function(strand, func)
                ));
                let field = field.as_sym(strand).unwrap();
                class.get(strand, field, &mut out).unwrap();
                assert!(out.as_array(strand).is_some());
            },
        );
    }
}
