use std::{collections::HashMap, rc::Rc};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
    object::TypeBuilder, unpack,
};

use crate::{global::Global, local};

pub(crate) struct Env<'v> {
    pub(crate) global: State<'v, Global<'v>>,
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
        if let Some(value) = env.get(index.as_str(strand).ok_or_else(|| Error::index(strand))?) {
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
            if let Some(value) = env.get(key.as_str(strand).ok_or_else(|| Error::index(strand))?) {
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
