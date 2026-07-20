use std::{collections::HashMap, rc::Rc};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
    object::{DictLike, DictView, DictViewSink, Spread, SpreadContext, TypeBuilder, Unpack},
    unpack,
    value::View,
};

use crate::{global::Global, local};

pub(crate) struct Env<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

struct EnvView;

impl<'v> DictLike<'v> for EnvView {
    type Object = Env<'v>;
    const MODULE: &'v str = "shell";
    const NAME: &'v str = "Env";

    fn len(this: Instance<'v, '_, Env<'v>>, strand: &mut Strand<'v, '_>) -> usize {
        this.borrow_unwrap()
            .global
            .local
            .get(strand)
            .env()
            .effective_map()
            .len()
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, Env<'v>>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let Some(key) = key.as_str(strand) else {
            return Ok(false);
        };
        let global = this.borrow(strand)?.global;
        let env = global.local.get(strand).env();
        if let Some(value) = strand.access(|x| env.get(key.as_str(x))) {
            Output::set(strand, out, value.as_ref());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn set<'a, 's>(
        this: Instance<'v, '_, Env<'v>>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        <Env<'v> as Object>::assign(this, strand, key, value)
    }

    fn flatten<'s>(
        this: Instance<'v, '_, Env<'v>>,
        strand: &mut Strand<'v, 's>,
        sink: &mut DictViewSink<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let global = this.borrow(strand)?.global;
        for (key, value) in global.local.get(strand).env().effective_map() {
            sink.push(strand, key.as_str(), value.as_str());
        }
        Ok(())
    }
}

impl<'v> Object<'v> for Env<'v> {
    const NAME: &'v str = "env";
    const MODULE: &'v str = "shell";
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
        DictView::<EnvView>::input(this, strand, out)
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        DictView::<EnvView>::spread(this, strand, context, sink)
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        DictView::<EnvView>::unpack(this, strand, unpack)
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut vars = HashMap::new();
        let mut first_positional = true;
        let func = loop {
            match args.next() {
                None => return Err(Error::missing_positional(strand, 0)),
                Some(Arg::Pos(slot)) if first_positional => {
                    first_positional = false;
                    let View::Dict(dict) = slot.view(strand) else {
                        break slot;
                    };
                    let mut pairs = dict.pairs();
                    strand.with_slots_sync(|strand, [mut key, mut value]| {
                        while pairs.next(strand, &mut key, &mut value)? {
                            let key = match key.view(strand) {
                                View::Str(key) => key.to_string(),
                                View::Sym(key) => key.as_str(strand).to_string(),
                                _ => {
                                    return Err(Error::type_error(
                                        strand,
                                        "env key: expected str or sym",
                                    ));
                                }
                            };
                            let value = if value.is_nil() {
                                None
                            } else if value.as_sym(strand)
                                == Some(this.borrow(strand)?.global.syms.inherit)
                            {
                                this.borrow(strand)?
                                    .global
                                    .local
                                    .get(strand)
                                    .env()
                                    .get(&key)
                                    .map(|value| value.into_owned())
                            } else {
                                Some(value.to_string(strand)?)
                            };
                            vars.insert(key, value);
                        }
                        Ok(())
                    })?;
                }
                Some(Arg::Pos(slot)) => break slot,
                Some(Arg::Key(sym, slot)) => {
                    vars.insert(
                        sym.as_str(strand).to_string(),
                        if slot.is_nil() {
                            None
                        } else {
                            Some(slot.to_string(strand)?)
                        },
                    );
                }
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
