use std::time::{Duration, SystemTime};

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value,
    object::TypeBuilder,
    unpack,
    value::{Nil, TypeObject},
    vm::ModuleBuilder,
};
use dolang_ext_shell::ResultExt;
use dolang_vfs_winnet::user;

use crate::{
    global::Global,
    rights::{self, Principal},
};

pub(crate) struct User(pub(crate) Option<user::User>);

pub(crate) struct Users(pub(crate) user::Users);

pub(crate) struct UserInfo;

pub(crate) struct UserFlags;

pub(crate) struct UserInfoAnnex<'v> {
    global: State<'v, Global<'v>>,
    info: user::Info,
}

fn make_user<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    user: user::User,
    out: impl Output<'v>,
) {
    global.types.user.create(strand, User(Some(user)), out);
}

fn make_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    info: user::Info,
    out: impl Output<'v>,
) {
    global
        .types
        .info
        .create_with_annex(strand, UserInfo, UserInfoAnnex { global, info }, out);
}

fn nullable_str<'v>(value: Option<&str>, out: impl Output<'v>, strand: &mut Strand<'v, '_>) {
    match value {
        Some(v) => Output::set(strand, out, v),
        None => Output::set(strand, out, Nil),
    }
}

fn nullable_windows_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<dolang_vfs::path::Path<'_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match value {
        Some(path) => dolang_ext_shell::windows_path(strand, path.as_str(), out),
        None => {
            Output::set(strand, out, Nil);
            Ok(())
        }
    }
}

fn nullable_time<'v, 's>(
    strand: &mut Strand<'v, 's>,
    seconds: Option<u64>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match seconds {
        Some(seconds) => dolang_ext_time::datetime(
            strand,
            SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
            out,
        )
        .map_err(|e| Error::runtime(strand, e)),
        None => {
            Output::set(strand, out, Nil);
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_from_slots<'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: Option<Slot<'v, '_>>,
    password: Option<Slot<'v, '_>>,
    full_name: Option<Slot<'v, '_>>,
    comment: Option<Slot<'v, '_>>,
    user_comment: Option<Slot<'v, '_>>,
    home_dir: Option<Slot<'v, '_>>,
    home_dir_drive: Option<Slot<'v, '_>>,
    profile: Option<Slot<'v, '_>>,
    script_path: Option<Slot<'v, '_>>,
    account_expires: Option<Slot<'v, '_>>,
    disabled: Option<Slot<'v, '_>>,
    password_never_expires: Option<Slot<'v, '_>>,
    password_cannot_change: Option<Slot<'v, '_>>,
) -> Result<'v, 's, user::Update> {
    let mut update = user::Update::default();
    if let Some(value) = name {
        update = update.name(
            value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name must be a Str"))?
                .to_string(),
        );
    }
    macro_rules! nullable_text {
        ($slot:expr, $method:ident, $name:literal) => {
            if let Some(value) = $slot {
                update = if value.is_nil() {
                    update.$method(None)
                } else {
                    update.$method(Some(
                        value
                            .as_str(strand)
                            .ok_or_else(|| {
                                Error::type_error(strand, concat!($name, " must be a Str or nil"))
                            })?
                            .to_string(),
                    ))
                };
            }
        };
    }

    if let Some(value) = password {
        update = update.password(
            value
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "password must be a Str"))?
                .to_string(),
        );
    }
    nullable_text!(full_name, full_name, "full_name");
    nullable_text!(comment, comment, "comment");
    nullable_text!(user_comment, user_comment, "user_comment");
    nullable_text!(home_dir_drive, home_dir_drive, "home_dir_drive");
    macro_rules! nullable_windows_path {
        ($slot:expr, $method:ident, $name:literal) => {
            if let Some(value) = $slot {
                update = if value.is_nil() {
                    update.$method(None)
                } else {
                    let path =
                        dolang_ext_shell::as_windows_path(strand, &value).ok_or_else(|| {
                            Error::type_error(
                                strand,
                                concat!($name, " must be an fs.windows.Path or nil"),
                            )
                        })?;
                    update.$method(Some(path))
                };
            }
        };
    }
    nullable_windows_path!(home_dir, home_dir, "home_dir");
    nullable_windows_path!(profile, profile, "profile");
    nullable_windows_path!(script_path, script_path, "script_path");
    if let Some(value) = account_expires {
        update = if value.is_nil() {
            update.account_expires(None)
        } else {
            let time = dolang_ext_time::as_datetime(strand, &value).ok_or_else(|| {
                Error::type_error(strand, "account_expires must be a time.DateTime or nil")
            })?;
            let seconds = time
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| Error::value(strand, "account_expires precedes the Unix epoch"))?
                .as_secs();
            update.account_expires(Some(seconds))
        };
    }
    macro_rules! boolean {
        ($slot:expr, $method:ident, $name:literal) => {
            if let Some(value) = $slot {
                let value = value
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, concat!($name, " must be a Bool")))?;
                update = update.$method(value);
            }
        };
    }
    boolean!(disabled, disabled, "disabled");
    boolean!(
        password_never_expires,
        password_never_expires,
        "password_never_expires"
    );
    boolean!(
        password_cannot_change,
        password_cannot_change,
        "password_cannot_change"
    );
    Ok(update)
}

impl Principal for User {
    const NAME: &'static str = "user";

    fn sid(&self) -> Option<&dolang_winterop::security::Sid> {
        self.0.as_ref().map(user::User::sid)
    }
}

impl<'v> Object<'v> for User {
    const NAME: &'v str = "User";
    const MODULE: &'v str = "winnet";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = rights::build(builder);
        builder
            .get("sid", |this, strand, mut out| {
                let b = this.borrow(strand)?;
                let u =
                    b.0.as_ref()
                        .ok_or_else(|| Error::state_error(strand, "user was deleted"))?;
                dolang_ext_shell::windows_sid(strand, u.sid().clone(), &mut out);
                Ok(())
            })
            .method("info", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                let info = {
                    let mut b = this.borrow_mut(strand)?;
                    b.0.as_mut()
                        .ok_or_else(|| Error::state_error(strand, "user was deleted"))?
                        .info()
                        .await
                        .into_sys(strand)?
                };
                make_info(strand, global, info, out);
                Ok(())
            })
            .method("update", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let name_sym = global.syms.name;
                let password_sym = global.syms.password;
                let full_name_sym = global.syms.full_name;
                let comment_sym = global.syms.comment;
                let user_comment_sym = global.syms.user_comment;
                let home_dir_sym = global.syms.home_dir;
                let home_dir_drive_sym = global.syms.home_dir_drive;
                let profile_sym = global.syms.profile;
                let script_path_sym = global.syms.script_path;
                let account_expires_sym = global.syms.account_expires;
                let disabled_sym = global.syms.disabled;
                let password_never_expires_sym = global.syms.password_never_expires;
                let password_cannot_change_sym = global.syms.password_cannot_change;
                let (
                    [],
                    [
                        name,
                        password,
                        full_name,
                        comment,
                        user_comment,
                        home_dir,
                        home_dir_drive,
                        profile,
                        script_path,
                        account_expires,
                        disabled,
                        password_never_expires,
                        password_cannot_change,
                    ],
                ) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    name_sym = None,
                    password_sym = None,
                    full_name_sym = None,
                    comment_sym = None,
                    user_comment_sym = None,
                    home_dir_sym = None,
                    home_dir_drive_sym = None,
                    profile_sym = None,
                    script_path_sym = None,
                    account_expires_sym = None,
                    disabled_sym = None,
                    password_never_expires_sym = None,
                    password_cannot_change_sym = None
                )?;
                let update = update_from_slots(
                    strand,
                    name,
                    password,
                    full_name,
                    comment,
                    user_comment,
                    home_dir,
                    home_dir_drive,
                    profile,
                    script_path,
                    account_expires,
                    disabled,
                    password_never_expires,
                    password_cannot_change,
                )?;
                let info = {
                    let mut b = this.borrow_mut(strand)?;
                    b.0.as_mut()
                        .ok_or_else(|| Error::state_error(strand, "user was deleted"))?
                        .update(update)
                        .await
                        .into_sys(strand)?
                };
                make_info(strand, global, info, out);
                Ok(())
            })
            .method("delete", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let user = this
                    .borrow_mut(strand)?
                    .0
                    .take()
                    .ok_or_else(|| Error::state_error(strand, "user was deleted"))?;
                user.delete().await.into_sys(strand)?;
                Output::set(strand, out, Nil);
                Ok(())
            })
    }
}

impl<'v> Object<'v> for Users {
    const NAME: &'v str = "Users";
    const MODULE: &'v str = "winnet";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
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
        let user = this
            .borrow_mut(strand)?
            .0
            .next_entry()
            .await
            .into_sys(strand)?;
        if let Some(user) = user {
            make_info(strand, *this.annex(), user, out);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl<'v> Object<'v> for UserFlags {
    const NAME: &'v str = "UserFlags";
    const MODULE: &'v str = "winnet";
    type Annex = user::Flags;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.get("int", |this, strand, out| {
            Output::set(strand, out, u64::from(this.annex().bits()));
            Ok(())
        })
    }
}

impl<'v> Object<'v> for UserInfo {
    const NAME: &'v str = "UserInfo";
    const MODULE: &'v str = "winnet";
    type Annex = UserInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("sid", |this, strand, mut out| {
                dolang_ext_shell::windows_sid(strand, this.annex().info.sid().clone(), &mut out);
                Ok(())
            })
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().info.name());
                Ok(())
            })
            .get("full_name", |this, strand, out| {
                nullable_str(this.annex().info.full_name(), out, strand);
                Ok(())
            })
            .get("comment", |this, strand, out| {
                nullable_str(this.annex().info.comment(), out, strand);
                Ok(())
            })
            .get("user_comment", |this, strand, out| {
                nullable_str(this.annex().info.user_comment(), out, strand);
                Ok(())
            })
            .get("home_dir", |this, strand, out| {
                nullable_windows_path(strand, this.annex().info.home_dir(), out)
            })
            .get("home_dir_drive", |this, strand, out| {
                nullable_str(this.annex().info.home_dir_drive(), out, strand);
                Ok(())
            })
            .get("profile", |this, strand, out| {
                nullable_windows_path(strand, this.annex().info.profile(), out)
            })
            .get("script_path", |this, strand, out| {
                nullable_windows_path(strand, this.annex().info.script_path(), out)
            })
            .get("flags", |this, strand, out| {
                let a = this.annex();
                a.global
                    .types
                    .flags
                    .create_with_annex(strand, UserFlags, a.info.flags(), out);
                Ok(())
            })
            .get("disabled", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    this.annex()
                        .info
                        .flags()
                        .contains(user::Flags::ACCOUNT_DISABLED),
                );
                Ok(())
            })
            .get("password_never_expires", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    this.annex()
                        .info
                        .flags()
                        .contains(user::Flags::PASSWORD_NEVER_EXPIRES),
                );
                Ok(())
            })
            .get("password_cannot_change", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    this.annex()
                        .info
                        .flags()
                        .contains(user::Flags::PASSWORD_CANNOT_CHANGE),
                );
                Ok(())
            })
            .get("password_age", |this, strand, out| {
                dolang_ext_time::duration(
                    strand,
                    Duration::from_secs(this.annex().info.password_age()),
                    out,
                )
            })
            .get("password_expired", |this, strand, out| {
                Output::set(strand, out, this.annex().info.password_expired());
                Ok(())
            })
            .get("last_logon", |this, strand, out| {
                nullable_time(strand, this.annex().info.last_logon(), out)
            })
            .get("account_expires", |this, strand, out| {
                nullable_time(strand, this.annex().info.account_expires(), out)
            })
            .get("bad_password_count", |this, strand, out| {
                Output::set(strand, out, this.annex().info.bad_password_count());
                Ok(())
            })
            .get("logon_count", |this, strand, out| {
                Output::set(strand, out, this.annex().info.logon_count());
                Ok(())
            })
    }
}

fn principal<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, ResultPrincipal> {
    if let Some(info) = global.types.info.cast(value) {
        return Ok(ResultPrincipal::Info(Box::new(
            info.enter_sync(strand, |_, info| info.annex().info.clone()),
        )));
    }
    if let Some(name) = value.as_str(strand) {
        return Ok(ResultPrincipal::Name(name.to_string()));
    }
    if let Some(sid) = dolang_ext_shell::as_windows_sid(strand, value) {
        return Ok(ResultPrincipal::Sid(sid));
    }
    Err(Error::type_error(
        strand,
        "principal must be an account name or security.windows.Sid",
    ))
}
enum ResultPrincipal {
    Name(String),
    Sid(dolang_winterop::security::Sid),
    Info(Box<user::Info>),
}

pub(crate) fn configure_module<'v, 'a>(
    module: ModuleBuilder<'v, 'a>,
    global: State<'v, Global<'v>>,
) -> ModuleBuilder<'v, 'a> {
    module
        .value("User", global.types.user)
        .value("UserInfo", global.types.info)
        .value("UserFlags", global.types.flags)
        .function("user", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let vfs = dolang_ext_shell::vfs(strand);
            let user = match principal(strand, global, &value)? {
                ResultPrincipal::Name(name) => user::by_name(&vfs, &name).await,
                ResultPrincipal::Sid(sid) => user::by_sid(&vfs, &sid).await,
                ResultPrincipal::Info(info) => Ok(user::from_info(&vfs, &info)),
            }
            .into_sys(strand)?;
            make_user(strand, global, user, out);
            Ok(())
        })
        .function("users", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            global.types.users.create_with_annex(
                strand,
                Users(user::enumerate(&dolang_ext_shell::vfs(strand))),
                global,
                out,
            );
            Ok(())
        })
        .function("create_user", async move |strand, args, out| {
            let password_sym = global.syms.password;
            let full_name_sym = global.syms.full_name;
            let comment_sym = global.syms.comment;
            let user_comment_sym = global.syms.user_comment;
            let home_dir_sym = global.syms.home_dir;
            let home_dir_drive_sym = global.syms.home_dir_drive;
            let profile_sym = global.syms.profile;
            let script_path_sym = global.syms.script_path;
            let account_expires_sym = global.syms.account_expires;
            let disabled_sym = global.syms.disabled;
            let password_never_expires_sym = global.syms.password_never_expires;
            let password_cannot_change_sym = global.syms.password_cannot_change;
            let (
                [name, password],
                [
                    full_name,
                    comment,
                    user_comment,
                    home_dir,
                    home_dir_drive,
                    profile,
                    script_path,
                    account_expires,
                    disabled,
                    password_never_expires,
                    password_cannot_change,
                ],
            ) = unpack!(
                strand,
                args,
                1,
                0,
                password_sym,
                full_name_sym = None,
                comment_sym = None,
                user_comment_sym = None,
                home_dir_sym = None,
                home_dir_drive_sym = None,
                profile_sym = None,
                script_path_sym = None,
                account_expires_sym = None,
                disabled_sym = None,
                password_never_expires_sym = None,
                password_cannot_change_sym = None
            )?;
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name must be a Str"))?
                .to_string();
            let password = password
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "password must be a Str"))?
                .to_string();
            let update = update_from_slots(
                strand,
                None,
                None,
                full_name,
                comment,
                user_comment,
                home_dir,
                home_dir_drive,
                profile,
                script_path,
                account_expires,
                disabled,
                password_never_expires,
                password_cannot_change,
            )?;
            let user = user::create(
                &dolang_ext_shell::vfs(strand),
                user::Create::new(name, password).update(update),
            )
            .await
            .into_sys(strand)?;
            make_user(strand, global, user, out);
            Ok(())
        })
}
