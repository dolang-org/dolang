use dolang::{
    compile::Compiler,
    runtime::{
        Error, Object, Output, State, method,
        object::{Mut, Ref, TypeBuilder},
        unpack,
        value::{Empty, View},
        vm::Builder,
    },
};
use dolang_shell_vfs::{SecurityInfo, UnixSecurityInfo, WindowsTokenInfo};

use crate::{error, global::Global};

pub(crate) fn configure_compiler<'a>(_compiler: &mut Compiler<'a>) {}

pub(crate) struct UnixInfo;

impl<'v> Object<'v> for UnixInfo {
    const NAME: &'v str = "UnixInfo";
    const MODULE: &'v str = "security";
    const SLOTS: usize = 1;
    type Annex = UnixSecurityInfo;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("uid", |this, strand, out| {
                Output::set(strand, out, this.annex().uid);
                Ok(())
            })
            .get("gid", |this, strand, out| {
                Output::set(strand, out, this.annex().gid);
                Ok(())
            })
            .get("euid", |this, strand, out| {
                Output::set(strand, out, this.annex().euid);
                Ok(())
            })
            .get("egid", |this, strand, out| {
                Output::set(strand, out, this.annex().egid);
                Ok(())
            })
            .get("group_ids", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                Ok(())
            })
    }
}

pub(crate) struct TokenInfo;

impl<'v> Object<'v> for TokenInfo {
    const NAME: &'v str = "TokenInfo";
    const MODULE: &'v str = "security";
    type Annex = WindowsTokenInfo;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("is_elevated", |this, strand, out| {
            Output::set(strand, out, this.annex().is_elevated);
            Ok(())
        })
    }
}

fn security_info<'v, 's>(
    strand: &mut dolang::runtime::Strand<'v, 's>,
    global: State<'v, Global<'v>>,
) -> dolang::runtime::Result<'v, 's, SecurityInfo> {
    if let Some(security) = global.local.get(strand).security() {
        return Ok(security);
    }
    let security = error::io_result(strand, SecurityInfo::current())?;
    global
        .local
        .get(strand)
        .replace_security(Some(security.clone()));
    Ok(security)
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let tuple = builder.sym("tuple");

    builder
        .module("security")
        .function_with_slots(
            "unix_info",
            async move |strand, args, mut out, [mut std, mut group_ids, mut tmp]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let SecurityInfo::Unix(info) = security_info(strand, global)? else {
                    return Err(Error::not_supported(strand));
                };

                strand.import("std", &mut std).await?;
                Output::set(strand, &mut group_ids, Empty::Array);
                {
                    let View::Array(group_ids_view) = group_ids.view(strand.vm()) else {
                        unreachable!("Empty::Array did not create an array")
                    };
                    for group_id in &info.group_ids {
                        group_ids_view.push(strand, *group_id)?;
                    }
                }
                method!(strand, &std, tuple, &mut tmp, &group_ids).await?;

                global
                    .types
                    .unix_info
                    .create_with_annex(strand, UnixInfo, info, &mut out);
                let this = global.types.unix_info.downcast(&out).unwrap();
                Output::set(
                    strand,
                    Mut::slot_mut::<0>(&mut this.borrow_mut_unwrap()),
                    &tmp,
                );
                Ok(())
            },
        )
        .function("token_info", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let SecurityInfo::Windows(info) = security_info(strand, global)? else {
                return Err(Error::not_supported(strand));
            };
            global
                .types
                .token_info
                .create_with_annex(strand, TokenInfo, info, out);
            Ok(())
        })
        .value("UnixInfo", global.types.unix_info)
        .value("TokenInfo", global.types.token_info)
        .commit();
}
