use crate::global::Global;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::TypeBuilder,
    unpack,
    value::{Nil, TypeObject},
    vm::ModuleBuilder,
};
use dolang_ext_shell::{ResultExt, as_windows_path, sec_desc_from_value};
use dolang_vfs_winnet::share;

pub(crate) struct Share(pub(crate) Option<share::Share>);

pub(crate) struct Shares(pub(crate) share::Shares);

pub(crate) struct ShareInfo;

pub(crate) struct ShareInfoAnnex<'v> {
    global: State<'v, Global<'v>>,
    info: share::Info,
}

fn make_share<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    share: share::Share,
    out: impl Output<'v>,
) {
    global.types.share.create(strand, Share(Some(share)), out);
}

fn make_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    info: share::Info,
    out: impl Output<'v>,
) {
    global.types.share_info.create_with_annex(
        strand,
        ShareInfo,
        ShareInfoAnnex { global, info },
        out,
    );
}

/// The share capability of a borrowed receiver that has not been deleted.
fn capability<'a, 'v, 's>(
    this: &'a Share,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, &'a share::Share> {
    this.0
        .as_ref()
        .ok_or_else(|| Error::state_error(strand, "share was deleted"))
}

fn nullable_str<'v>(strand: &mut Strand<'v, '_>, value: Option<&str>, out: impl Output<'v>) {
    match value {
        Some(v) => Output::set(strand, out, v),
        None => Output::set(strand, out, Nil),
    }
}

/// Coerces a `Str` or `nil` argument.
fn optional_str<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, Option<String>> {
    if value.is_nil() {
        return Ok(None);
    }
    value
        .as_str(strand)
        .map(Into::into)
        .map(Some)
        .ok_or_else(|| Error::type_error(strand, format!("{name} must be a Str or nil")))
}

/// Coerces a connection limit, where `nil` selects unlimited use.
fn max_uses<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, Option<u32>> {
    if value.is_nil() {
        return Ok(None);
    }
    let n = value
        .as_int(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{name} must be an Int or nil")))?;
    u32::try_from(n).map(Some).map_err(|_| {
        Error::value(
            strand,
            format!("{name} is outside the range 0..=4294967295"),
        )
    })
}

fn kind<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, share::Kind> {
    match value.as_sym(strand) {
        Some(v) if v == global.syms.disktree => Ok(share::Kind::DiskTree),
        Some(v) if v == global.syms.printq => Ok(share::Kind::PrintQueue),
        Some(v) if v == global.syms.device => Ok(share::Kind::Device),
        Some(v) if v == global.syms.ipc => Ok(share::Kind::Ipc),
        _ => Err(Error::value(
            strand,
            "kind must be :DISKTREE:, :PRINTQ:, :DEVICE:, or :IPC:",
        )),
    }
}

impl<'v> Object<'v> for Share {
    const NAME: &'v str = "Share";
    const MODULE: &'v str = "winnet";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let share = capability(&borrow, strand)?;
                Output::set(strand, out, share.name());
                Ok(())
            })
            .method("info", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                let borrow = this.borrow(strand)?;
                let info = capability(&borrow, strand)?.info().await.into_sys(strand)?;
                make_info(strand, global, info, out);
                Ok(())
            })
            .method("update", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let comment_sym = global.syms.comment;
                let max_uses_sym = global.syms.max_uses;
                let sec_desc_sym = global.syms.sec_desc;
                let ([], [comment, maximum, descriptor]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    comment_sym = None,
                    max_uses_sym = None,
                    sec_desc_sym = None
                )?;
                let mut update = share::Update::default();
                if let Some(v) = comment {
                    update = update.comment(optional_str(strand, &v, "comment")?);
                }
                if let Some(v) = maximum {
                    update = update.max_uses(max_uses(strand, &v, "max_uses")?);
                }
                if let Some(v) = descriptor {
                    update = update.sec_desc(sec_desc_from_value(strand, &v, "sec_desc").await?);
                }
                let borrow = this.borrow(strand)?;
                let info = capability(&borrow, strand)?
                    .update(update)
                    .await
                    .into_sys(strand)?;
                make_info(strand, global, info, out);
                Ok(())
            })
            .method("delete", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                capability(&borrow, strand)?
                    .delete()
                    .await
                    .into_sys(strand)?;
                drop(borrow);
                this.borrow_mut(strand)?.0 = None;
                Output::set(strand, out, Nil);
                Ok(())
            })
    }
}

impl<'v> Object<'v> for ShareInfo {
    const NAME: &'v str = "ShareInfo";
    const MODULE: &'v str = "winnet";
    type Annex = ShareInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().info.name());
                Ok(())
            })
            .get("kind", |this, strand, out| {
                let a = this.annex();
                let v = match a.info.kind() {
                    share::Kind::DiskTree => a.global.syms.disktree,
                    share::Kind::PrintQueue => a.global.syms.printq,
                    share::Kind::Device => a.global.syms.device,
                    share::Kind::Ipc => a.global.syms.ipc,
                };
                Output::set(strand, out, v);
                Ok(())
            })
            .get("special", |this, strand, out| {
                Output::set(strand, out, this.annex().info.special());
                Ok(())
            })
            .get("temporary", |this, strand, out| {
                Output::set(strand, out, this.annex().info.temporary());
                Ok(())
            })
            .get("comment", |this, strand, out| {
                nullable_str(strand, this.annex().info.comment(), out);
                Ok(())
            })
            .get("max_uses", |this, strand, out| {
                match this.annex().info.max_uses() {
                    Some(v) => Output::set(strand, out, u64::from(v)),
                    None => Output::set(strand, out, Nil),
                };
                Ok(())
            })
            .get("current_uses", |this, strand, out| {
                Output::set(strand, out, u64::from(this.annex().info.current_uses()));
                Ok(())
            })
            .get("path", |this, strand, out| match this.annex().info.path() {
                Some(path) => dolang_ext_shell::windows_path(strand, path.as_str(), out),
                None => Err(Error::value(
                    strand,
                    "share path does not use Windows path syntax",
                )),
            })
            .get("sec_desc", |this, strand, out| {
                match this.annex().info.sec_desc() {
                    Some(descriptor) => {
                        dolang_ext_shell::create_sec_desc(strand, descriptor.clone(), out)
                    }
                    None => Output::set(strand, out, Nil),
                };
                Ok(())
            })
    }
}
impl<'v> Object<'v> for Shares {
    const NAME: &'v str = "Shares";
    const MODULE: &'v str = "winnet";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(b: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        b.supertype(TypeObject::Iter)
    }

    async fn iter<'a, 's>(
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if let Some(v) = this
            .borrow_mut(strand)?
            .0
            .next_entry()
            .await
            .into_sys(strand)?
        {
            make_info(strand, *this.annex(), v, out);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(crate) fn configure_module<'v, 'a>(
    module: ModuleBuilder<'v, 'a>,
    global: State<'v, Global<'v>>,
) -> ModuleBuilder<'v, 'a> {
    module
        .value("Share", global.types.share)
        .value("ShareInfo", global.types.share_info)
        .function("share", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let vfs = dolang_ext_shell::vfs(strand);
            let share = if let Some(info) = global.types.share_info.cast(&value) {
                Ok(info.enter_sync(strand, |_, v| share::from_info(&vfs, &v.annex().info)))
            } else if let Some(name) = value.as_str(strand) {
                let name = name.to_string();
                share::by_name(&vfs, &name).await
            } else {
                return Err(Error::type_error(
                    strand,
                    "argument must be a share name or ShareInfo",
                ));
            }
            .into_sys(strand)?;
            make_share(strand, global, share, out);
            Ok(())
        })
        .function("shares", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let shares = share::enumerate(&dolang_ext_shell::vfs(strand));
            global
                .types.shares
                .create_with_annex(strand, Shares(shares), global, out);
            Ok(())
        })
        .function("create_share", async move |strand, args, out| {
            let path_sym = global.syms.path;
            let kind_sym = global.syms.kind;
            let comment_sym = global.syms.comment;
            let max_uses_sym = global.syms.max_uses;
            let special_sym = global.syms.special;
            let temporary_sym = global.syms.temporary;
            let sec_desc_sym = global.syms.sec_desc;
            let ([name, path], [kind_arg, comment, maximum, special, temporary, descriptor]) = unpack!(
                strand,
                args,
                1,
                0,
                path_sym,
                kind_sym = None,
                comment_sym = None,
                max_uses_sym = None,
                special_sym = None,
                temporary_sym = None,
                sec_desc_sym = None
            )?;
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name must be a Str"))?
                .into();
            let path = as_windows_path(strand, &path)
                .ok_or_else(|| Error::type_error(strand, "path must be an fs.windows.Path"))?;
            let mut create = share::Create::new(name, path);
            if let Some(v) = kind_arg {
                create = create.kind(kind(strand, global, &v)?)
            }
            if let Some(v) = comment {
                create = create.comment(optional_str(strand, &v, "comment")?)
            }
            if let Some(v) = maximum {
                create = create.max_uses(max_uses(strand, &v, "max_uses")?)
            }
            if let Some(v) = special {
                create = create.special(
                    v.as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "special must be a Bool"))?,
                )
            }
            if let Some(v) = temporary {
                create = create.temporary(
                    v.as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "temporary must be a Bool"))?,
                )
            }
            if let Some(v) = descriptor {
                create = create.sec_desc(sec_desc_from_value(strand, &v, "sec_desc").await?)
            }
            let share = share::create(&dolang_ext_shell::vfs(strand), create)
                .await
                .into_sys(strand)?;
            make_share(strand, global, share, out);
            Ok(())
        })
}
