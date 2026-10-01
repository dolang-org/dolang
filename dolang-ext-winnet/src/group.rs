use crate::global::Global;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::TypeBuilder,
    unpack,
    value::{Nil, TypeObject},
    vm::ModuleBuilder,
};
use dolang_ext_shell::ResultExt;
use dolang_vfs_winnet::group;

use crate::rights::{self, Principal};

pub(crate) struct Group(pub(crate) Option<group::Group>);

impl Principal for Group {
    const NAME: &'static str = "group";

    fn sid(&self) -> Option<&dolang_winterop::security::Sid> {
        self.0.as_ref().map(group::Group::sid)
    }
}

pub(crate) struct Groups(pub(crate) group::Groups);
pub(crate) struct GroupMembers(pub(crate) group::Members);
pub(crate) struct GroupInfo;
pub(crate) struct GroupInfoAnnex(pub(crate) group::Info);

fn make_group<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    group: group::Group,
    out: impl Output<'v>,
) {
    global.types.group.create(strand, Group(Some(group)), out);
}
fn make_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    info: group::Info,
    out: impl Output<'v>,
) {
    global
        .types
        .group_info
        .create_with_annex(strand, GroupInfo, GroupInfoAnnex(info), out);
}
fn principal<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, ResultPrincipal> {
    if let Some(info) = global.types.group_info.cast(value) {
        return Ok(ResultPrincipal::Info(
            info.enter_sync(strand, |_, info| info.annex().0.clone()),
        ));
    }
    if let Some(v) = value.as_str(strand) {
        return Ok(ResultPrincipal::Name(v.into()));
    }
    if let Some(v) = dolang_ext_shell::as_windows_sid(strand, value) {
        return Ok(ResultPrincipal::Sid(v));
    }
    Err(Error::type_error(
        strand,
        "principal must be an account name or security.windows.Sid",
    ))
}
enum ResultPrincipal {
    Name(String),
    Sid(dolang_winterop::security::Sid),
    Info(group::Info),
}
async fn member_sid<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, dolang_winterop::security::Sid> {
    match principal(strand, strand.state::<Global<'v>>(), value)? {
        ResultPrincipal::Sid(v) => Ok(v),
        ResultPrincipal::Name(v) => Ok(dolang_ext_shell::vfs(strand)
            .account_name(&v)
            .await
            .into_sys(strand)?
            .sid()
            .clone()),
        ResultPrincipal::Info(_) => Err(Error::type_error(
            strand,
            "member principal must be an account name or security.windows.Sid",
        )),
    }
}

impl<'v> Object<'v> for Group {
    const NAME: &'v str = "Group";
    const MODULE: &'v str = "winnet";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = rights::build(builder);
        builder
            .get("sid", |this, strand, mut out| {
                let b = this.borrow(strand)?;
                let g =
                    b.0.as_ref()
                        .ok_or_else(|| Error::state_error(strand, "group was deleted"))?;
                dolang_ext_shell::windows_sid(strand, g.sid().clone(), &mut out);
                Ok(())
            })
            .method("info", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                let info = this
                    .borrow_mut(strand)?
                    .0
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "group was deleted"))?
                    .info()
                    .await
                    .into_sys(strand)?;
                make_info(strand, global, info, out);
                Ok(())
            })
            .method("update", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let name_sym = global.syms.name;
                let comment_sym = global.syms.comment;
                let ([], [name, comment]) =
                    unpack!(strand, args, 0, 0, name_sym = None, comment_sym = None)?;
                let mut update = group::Update::default();
                if let Some(v) = name {
                    update = update.name(
                        v.as_str(strand)
                            .ok_or_else(|| Error::type_error(strand, "name must be a Str"))?
                            .into(),
                    );
                }
                if let Some(v) = comment {
                    update = if v.is_nil() {
                        update.comment(None)
                    } else {
                        update.comment(Some(
                            v.as_str(strand)
                                .ok_or_else(|| {
                                    Error::type_error(strand, "comment must be a Str or nil")
                                })?
                                .into(),
                        ))
                    };
                }
                let info = this
                    .borrow_mut(strand)?
                    .0
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "group was deleted"))?
                    .update(update)
                    .await
                    .into_sys(strand)?;
                make_info(strand, global, info, out);
                Ok(())
            })
            .method("members", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                let members = this
                    .borrow(strand)?
                    .0
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "group was deleted"))?
                    .members();
                global.types.group_members.create_with_annex(
                    strand,
                    GroupMembers(members),
                    global,
                    out,
                );
                Ok(())
            })
            .method("add_member", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let sid = member_sid(strand, &value).await?;
                this.borrow_mut(strand)?
                    .0
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "group was deleted"))?
                    .add_member(sid)
                    .await
                    .into_sys(strand)?;
                Output::set(strand, out, Nil);
                Ok(())
            })
            .method("remove_member", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let sid = member_sid(strand, &value).await?;
                this.borrow_mut(strand)?
                    .0
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "group was deleted"))?
                    .remove_member(sid)
                    .await
                    .into_sys(strand)?;
                Output::set(strand, out, Nil);
                Ok(())
            })
            .method("delete", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let group = this
                    .borrow_mut(strand)?
                    .0
                    .take()
                    .ok_or_else(|| Error::state_error(strand, "group was deleted"))?;
                group.delete().await.into_sys(strand)?;
                Output::set(strand, out, Nil);
                Ok(())
            })
    }
}
impl<'v> Object<'v> for GroupInfo {
    const NAME: &'v str = "GroupInfo";
    const MODULE: &'v str = "winnet";
    type Annex = GroupInfoAnnex;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("sid", |this, strand, mut out| {
                dolang_ext_shell::windows_sid(strand, this.annex().0.sid().clone(), &mut out);
                Ok(())
            })
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().0.name());
                Ok(())
            })
            .get("comment", |this, strand, out| {
                match this.annex().0.comment() {
                    Some(v) => Output::set(strand, out, v),
                    None => Output::set(strand, out, Nil),
                };
                Ok(())
            })
    }
}
impl<'v> Object<'v> for Groups {
    const NAME: &'v str = "Groups";
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
impl<'v> Object<'v> for GroupMembers {
    const NAME: &'v str = "GroupMembers";
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if let Some(v) = this
            .borrow_mut(strand)?
            .0
            .next_entry()
            .await
            .into_sys(strand)?
        {
            dolang_ext_shell::windows_sid_name(strand, v, &mut out);
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
        .value("Group", global.types.group)
        .value("GroupInfo", global.types.group_info)
        .function("group", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let vfs = dolang_ext_shell::vfs(strand);
            let group = match principal(strand, global, &value)? {
                ResultPrincipal::Name(v) => group::by_name(&vfs, &v).await,
                ResultPrincipal::Sid(v) => group::by_sid(&vfs, &v).await,
                ResultPrincipal::Info(info) => Ok(group::from_info(&vfs, &info)),
            }
            .into_sys(strand)?;
            make_group(strand, global, group, out);
            Ok(())
        })
        .function("groups", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            global.types.groups.create_with_annex(
                strand,
                Groups(group::enumerate(&dolang_ext_shell::vfs(strand))),
                global,
                out,
            );
            Ok(())
        })
        .function("create_group", async move |strand, args, out| {
            let comment_sym = global.syms.comment;
            let ([name], [comment]) = unpack!(strand, args, 1, 0, comment_sym = None)?;
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name must be a Str"))?
                .into();
            let comment = match comment {
                Some(v) if v.is_nil() => None,
                Some(v) => Some(
                    v.as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "comment must be a Str or nil"))?
                        .into(),
                ),
                None => None,
            };
            let group = group::create(
                &dolang_ext_shell::vfs(strand),
                group::Create::new(name).comment(comment),
            )
            .await
            .into_sys(strand)?;
            make_group(strand, global, group, out);
            Ok(())
        })
}
