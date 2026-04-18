use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Value, call,
    object::{TypeBuilder, Unpack, UnpackItem},
    unpack,
    value::TypeObject,
};

use crate::{global::Global, local};

pub(crate) struct Env<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

pub(crate) struct EnvIter {
    items: HashMap<String, String>,
}

impl EnvIter {
    fn new(items: HashMap<String, String>) -> Self {
        Self { items }
    }
}

fn set_pair<'v, 's>(strand: &mut Strand<'v, 's>, out: &mut Slot<'v, '_>, key: &str, value: &str) {
    Output::set(strand, out, (key, value))
}

fn take_any<'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &mut HashMap<String, String>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    let Some(key) = items.keys().next().cloned() else {
        return Ok(false);
    };
    let value = items.remove(&key).unwrap();
    set_pair(strand, out, &key, &value);
    Ok(true)
}

fn get_sym<'a, 'v>(
    strand: &Strand<'v, '_>,
    items: &'a HashMap<String, String>,
    key: Sym<'v, '_>,
) -> Option<&'a str> {
    items.get(key.as_str(strand)).map(String::as_str)
}

fn get_value_key<'a, 'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &'a HashMap<String, String>,
    key: &Value<'v>,
) -> std::result::Result<Option<(&'a str, &'a str)>, Error<'v, 's>> {
    let key = key
        .as_str(strand)
        .ok_or_else(|| Error::missing_key(strand, key))?
        .to_string();
    Ok(items
        .get_key_value(&key)
        .map(|(key, value)| (key.as_str(), value.as_str())))
}

fn unpack_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &mut HashMap<String, String>,
    mut unpack: Unpack<'v, '_>,
    mut rest_out: impl FnMut(
        &mut Strand<'v, 's>,
        Slot<'v, '_>,
        HashMap<String, String>,
    ) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    if unpack.required() != 0 {
        return Err(Error::missing_positional(strand, 0));
    }

    let mut matched = HashSet::new();
    let mut rest_slot = None;

    for item in unpack.iter() {
        match item {
            UnpackItem::Pos { slot, default } => {
                Output::set(
                    strand,
                    slot,
                    default.expect("required positionals rejected"),
                );
            }
            UnpackItem::SymKey {
                key,
                mut slot,
                default,
            } => {
                let key_str = key.as_str(strand);
                if !matched.insert(key_str.to_string()) {
                    return Err(Error::unexpected_key(strand, key));
                }
                if let Some(value) = get_sym(strand, items, key) {
                    Output::set(strand, Slot::reborrow(&mut slot), value);
                } else if let Some(default) = default {
                    Output::set(strand, slot, default);
                } else {
                    return Err(Error::missing_key(strand, key));
                }
            }
            UnpackItem::ConstKey {
                key,
                mut slot,
                default,
            } => {
                let Some(key_str) = key.as_str(strand) else {
                    return Err(Error::missing_key(strand, key));
                };
                if !matched.insert(key_str.to_string()) {
                    return Err(Error::unexpected_key(strand, key));
                }
                if let Some((_, value)) = get_value_key(strand, items, key)? {
                    Output::set(strand, Slot::reborrow(&mut slot), value);
                } else if let Some(default) = default {
                    Output::set(strand, slot, default);
                } else {
                    return Err(Error::missing_key(strand, key));
                }
            }
            UnpackItem::Rest { slot } => {
                rest_slot = Some(slot);
            }
        }
    }

    if unpack.exhaustive()
        && let Some(key) = items.keys().find(|key| !matched.contains(*key))
    {
        return Err(Error::unexpected_key(strand, key.as_str()));
    }

    for key in &matched {
        let _ = items.remove(key);
    }

    if let Some(slot) = rest_slot {
        let rest = std::mem::take(items);
        return rest_out(strand, slot, rest);
    }

    Ok(())
}

impl<'v> Object<'v> for Env<'v> {
    const NAME: &'v str = "env";
    const MODULE: &'v str = "sys";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn assign<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let me = this.borrow(strand)?;
        let key = index.as_str(strand).ok_or_else(|| Error::index(strand))?;
        let value = if value.is_nil() {
            None
        } else {
            Some(value.to_string(strand)?)
        };
        let local = me.global.local.get(strand);
        let mut env = (*local.env()).clone();
        env.insert(key.to_string(), value);
        let _ = local.replace_env(Rc::new(env));
        Ok(())
    }

    fn index<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.borrow(strand)?;
        let env = me.global.local.get(strand).env();
        let index = index.as_str(strand).ok_or_else(|| Error::index(strand))?;
        if let Some(value) = strand.access(|x| env.get(index.as_str(x))) {
            Output::set(strand, out, value.as_ref());
            Ok(())
        } else {
            Err(Error::index(strand))
        }
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let else_sym = builder.sym("else");
        let default = builder.sym("default");
        builder.method("get", async move |this, strand, args, out| {
            let ([key], [else_sym, default]) =
                unpack!(strand, args, 1, 0, else_sym = None, default = None)?;
            let borrow = this.borrow(strand)?;
            let env = borrow.global.local.get(strand).env();
            let key = key.as_str(strand).ok_or_else(|| Error::index(strand))?;
            if let Some(value) = strand.access(|x| env.get(key.as_str(x))) {
                Output::set(strand, out, value.as_ref());
                return Ok(());
            }
            if let Some(default) = default {
                Output::set(strand, out, default);
                return Ok(());
            }
            if let Some(thunk) = else_sym {
                return call!(strand, thunk, out).await;
            }
            Ok(())
        })
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = this.borrow(strand)?.global;
        let items = global.local.get(strand).env().effective_map();
        global
            .types
            .env_iter
            .create(strand, EnvIter::new(items), out);
        Ok(())
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = this.borrow(strand)?.global;
        let mut items = global.local.get(strand).env().effective_map();
        unpack_items(strand, &mut items, unpack, |strand, out, rest| {
            global
                .types
                .env_iter
                .create(strand, EnvIter::new(rest), out);
            Ok(())
        })
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut vars = HashMap::new();
        let func = loop {
            match args.next() {
                None => return Err(Error::missing_positional(strand, 0)),
                Some(Arg::Pos(slot)) => break slot,
                Some(Arg::Key(sym, slot)) => vars.insert(
                    sym.as_str(strand).to_string(),
                    if slot.is_nil() {
                        None
                    } else {
                        Some(slot.to_string(strand)?)
                    },
                ),
            };
        };
        let me = this.borrow(strand)?;
        let local = me.global.local.get(strand);
        let env = local.replace_env(Rc::new(local::Env::derived(local.env(), vars)));
        let res = func.call(strand, args, out).await;
        let local = me.global.local.get(strand);
        let _ = local.replace_env(env);
        res
    }
}

impl<'v> Object<'v> for EnvIter {
    const NAME: &'v str = "EnvIter";
    const MODULE: &'v str = "sys";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let items = &mut this.borrow_mut(strand)?.items;
        take_any(strand, items, &mut out)
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut items = this.borrow(strand)?.items.clone();
        unpack_items(strand, &mut items, unpack, |strand, out, rest| {
            this.borrow_mut_unwrap().items = rest;
            Output::set(strand, out, this);
            Ok(())
        })?;
        this.borrow_mut_unwrap().items = items;
        Ok(())
    }
}
