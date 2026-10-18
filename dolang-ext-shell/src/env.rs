use std::{collections::HashMap, rc::Rc};

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
    object::{DictLike, DictView, DictViewSink, Spread, SpreadContext, TypeBuilder, Unpack},
    unpack,
    value::{TypeObject, View},
};

use crate::{global::ShellGlobal, local};

pub(crate) struct Env<'v> {
    pub(crate) global: State<'v, ShellGlobal<'v>>,
}

struct EnvView;

impl<'v> DictLike<'v> for EnvView {
    type Object = Env<'v>;
    const MODULE: &'v str = "shell";
    const NAME: &'v str = "Env";

    fn len(&self, this: Instance<'v, '_, Env<'v>>, strand: &mut Strand<'v, '_>) -> usize {
        this.borrow_unwrap()
            .global
            .local
            .get(strand)
            .env()
            .effective_map()
            .len()
    }

    fn get<'a, 's>(
        &self,
        this: Instance<'v, '_, Env<'v>>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        // Environment variables are single-valued.
        if !matches!(instance, 0 | -1) {
            return Ok(false);
        }
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
        &self,
        this: Instance<'v, '_, Env<'v>>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        <Env<'v> as Object>::assign(this, strand, key, value)
    }

    fn delete<'s>(
        &self,
        this: Instance<'v, '_, Env<'v>>,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let key = key
            .as_str(strand)
            .ok_or_else(|| Error::index(strand))?
            .to_string();
        let global = this.borrow(strand)?.global;
        let local = global.local.get(strand);
        let current = local.env();
        let present = current.get(&key).is_some();
        if present {
            let mut env = (*current).clone();
            env.insert(key, None);
            local.replace_env(Rc::new(env));
        }
        Ok(present)
    }

    fn pop<'a, 's>(
        &self,
        this: Instance<'v, '_, Env<'v>>,
        strand: &'a mut Strand<'v, 's>,
        key: &Value<'v>,
        instance: i64,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if !self.get(this, strand, key, instance, Slot::reborrow(&mut out))? {
            return Ok(false);
        }
        self.delete(this, strand, key)
    }

    fn clear<'s>(
        &self,
        this: Instance<'v, '_, Env<'v>>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let global = this.borrow(strand)?.global;
        let local = global.local.get(strand);
        let current = local.env();
        let mut env = (*current).clone();
        for key in current.effective_map().into_keys() {
            env.insert(key, None);
        }
        local.replace_env(Rc::new(env));
        Ok(())
    }

    fn flatten<'s>(
        &self,
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
        if value.is_nil() {
            return Err(Error::type_error(
                strand,
                "use env.delete to unset a variable",
            ));
        }
        let value = Some(value.to_string(strand)?);
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
        for name in [
            "get", "keys", "values", "pairs", "contains", "count", "delete", "pop", "clear",
        ] {
            let method = builder.sym(name);
            builder = builder.method(name, async move |this, strand, args, out| {
                DictView::method(this, EnvView, strand, method, args, out).await
            });
        }
        builder
            .supertype(TypeObject::Func)
            .supertype(TypeObject::Iterable)
            .get("len", |this, strand, out| {
                let len = EnvView.len(this, strand);
                Output::set(strand, out, len);
                Ok(())
            })
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        DictView::iter(this, &EnvView, strand, out)
    }

    async fn spread<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        DictView::spread(this, &EnvView, strand, context, sink)
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        DictView::unpack(this, &EnvView, strand, unpack)
    }

    async fn call<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut vars = HashMap::new();
        let ([first], [second], mut args) = unpack!(strand, args, 1, 1, ...)?;
        let func = if let Some(func) = second {
            let Some(dict) = first.as_dict(strand) else {
                return Err(Error::type_error(strand, "expected `Dict`"));
            };
            let mut pairs = dict.pairs();
            strand.with_slots_sync(|strand, [mut key, mut value]| {
                while pairs.next(strand, &mut key, &mut value)? {
                    let key = match key.view(strand) {
                        View::Str(key) => key.to_string(),
                        View::Sym(key) => key.as_str(strand).to_string(),
                        _ => {
                            return Err(Error::type_error(strand, "key: expected Str or Sym"));
                        }
                    };
                    let value = if value.is_nil() {
                        None
                    } else if value.as_sym(strand) == Some(this.borrow(strand)?.global.syms.inherit)
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
            match args.next() {
                None => (),
                Some(Arg::Pos(_)) => return Err(Error::unexpected_positional(strand, 2)),
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
            }
            func
        } else {
            for arg in args {
                match arg {
                    Arg::Pos(_) => return Err(Error::unexpected_positional(strand, 1)),
                    Arg::Key(sym, slot) => {
                        vars.insert(
                            sym.as_str(strand).to_string(),
                            if slot.is_nil() {
                                None
                            } else {
                                Some(slot.to_verbatim(strand)?)
                            },
                        );
                    }
                };
            }
            first
        };

        let me = this.borrow(strand)?;
        let local = me.global.local.get(strand);
        let env = local.replace_env(Rc::new(local::Env::derived(local.env(), vars)));
        let res = call!(strand, func, out).await;
        let local = me.global.local.get(strand);
        let _ = local.replace_env(env);
        res
    }
}
