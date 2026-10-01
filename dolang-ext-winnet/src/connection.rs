use crate::global::Global;
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value, call,
    object::TypeBuilder,
    strand::InterruptMask,
    unpack,
    value::{Nil, TypeObject},
    vm::ModuleBuilder,
};
use dolang_ext_shell::{ResultExt, as_windows_path};
use dolang_vfs_winnet::connection;

pub(crate) struct Connection(pub(crate) Option<connection::Connection>);

pub(crate) struct Connections(pub(crate) connection::Connections);

pub(crate) struct ConnectionInfo;

pub(crate) struct ConnectionInfoAnnex<'v> {
    global: State<'v, Global<'v>>,
    info: connection::Info,
}

fn make_connection<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    connection: connection::Connection,
    out: impl Output<'v>,
) {
    global
        .types
        .connection
        .create(strand, Connection(Some(connection)), out);
}

fn make_info<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    info: connection::Info,
    out: impl Output<'v>,
) {
    global.types.connection_info.create_with_annex(
        strand,
        ConnectionInfo,
        ConnectionInfoAnnex { global, info },
        out,
    );
}

/// The connection capability of a borrowed receiver that is still connected.
fn capability<'a, 'v, 's>(
    this: &'a Connection,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, &'a connection::Connection> {
    this.0
        .as_ref()
        .ok_or_else(|| Error::state_error(strand, "connection was disconnected"))
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

fn expect_bool<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, bool> {
    value
        .as_bool(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{name} must be a Bool")))
}

fn kind<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, connection::Kind> {
    match value.as_sym(strand) {
        Some(v) if v == global.syms.disk => Ok(connection::Kind::Disk),
        Some(v) if v == global.syms.print => Ok(connection::Kind::Print),
        Some(v) if v == global.syms.any => Ok(connection::Kind::Any),
        _ => Err(Error::value(
            strand,
            "kind must be :DISK:, :PRINT:, or :ANY:",
        )),
    }
}

impl Connection {
    /// Disconnects, tolerating a receiver that is already disconnected.
    ///
    /// Idempotence is what lets a scoped connection be disconnected explicitly
    /// inside its own block without the scope exit failing or disconnecting a
    /// second time. A failed disconnect restores the capability so it can be
    /// retried.
    async fn teardown<'v, 's>(
        this: Instance<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
        force: bool,
        forget_credentials: Option<bool>,
    ) -> Result<'v, 's, ()> {
        let Some(connection) = this.borrow_mut(strand)?.0.take() else {
            return Ok(());
        };
        let result = connection
            .disconnect(force, forget_credentials)
            .await
            .into_sys(strand);
        if result.is_err() {
            this.borrow_mut(strand)?.0 = Some(connection);
        }
        result
    }
}

impl<'v> Object<'v> for Connection {
    const NAME: &'v str = "Connection";
    const MODULE: &'v str = "winnet";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                let connection = capability(&borrow, strand)?;
                Output::set(strand, out, connection.name());
                Ok(())
            })
            .get("connected", |this, strand, out| {
                let connected = this.borrow(strand)?.0.is_some();
                Output::set(strand, out, connected);
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
            .method("path", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                let info = capability(&borrow, strand)?.info().await.into_sys(strand)?;
                // A redirected device is reachable as a drive root; a deviceless
                // connection is only reachable by its UNC name.
                let path = match info.local() {
                    Some(local) => format!(r"{local}\"),
                    None => info.remote().to_owned(),
                };
                dolang_ext_shell::windows_path(strand, path, out)
            })
            .method("disconnect", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let force_sym = global.syms.force;
                let forget_sym = global.syms.forget_credentials;
                let ([], [force, forget]) =
                    unpack!(strand, args, 0, 0, force_sym = None, forget_sym = None)?;
                let force = match force {
                    Some(v) => expect_bool(strand, &v, "force")?,
                    None => false,
                };
                let forget = match forget {
                    Some(v) if v.is_nil() => None,
                    Some(v) => Some(expect_bool(strand, &v, "forget_credentials")?),
                    None => None,
                };
                strand
                    .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                        Connection::teardown(this, strand, force, forget).await
                    })
                    .await?;
                Output::set(strand, out, Nil);
                Ok(())
            })
    }
}

impl<'v> Object<'v> for ConnectionInfo {
    const NAME: &'v str = "ConnectionInfo";
    const MODULE: &'v str = "winnet";
    type Annex = ConnectionInfoAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("local", |this, strand, out| {
                nullable_str(strand, this.annex().info.local(), out);
                Ok(())
            })
            .get("remote", |this, strand, out| {
                Output::set(strand, out, this.annex().info.remote());
                Ok(())
            })
            .get("provider", |this, strand, out| {
                nullable_str(strand, this.annex().info.provider(), out);
                Ok(())
            })
            .get("user", |this, strand, out| {
                nullable_str(strand, this.annex().info.user(), out);
                Ok(())
            })
            .get("kind", |this, strand, out| {
                let a = this.annex();
                let v = match a.info.kind() {
                    connection::Kind::Disk => a.global.syms.disk,
                    connection::Kind::Print => a.global.syms.print,
                    connection::Kind::Any => a.global.syms.any,
                };
                Output::set(strand, out, v);
                Ok(())
            })
            .get("state", |this, strand, out| {
                let a = this.annex();
                let v = match a.info.state() {
                    connection::State::Connected => a.global.syms.connected,
                    connection::State::Remembered => a.global.syms.remembered,
                };
                Output::set(strand, out, v);
                Ok(())
            })
            .get("persistent", |this, strand, out| {
                Output::set(strand, out, this.annex().info.persistent());
                Ok(())
            })
    }
}

impl<'v> Object<'v> for Connections {
    const NAME: &'v str = "Connections";
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

/// Hands a freshly added connection to a trailing block, or to the caller.
///
/// With a block the connection is scoped: it is disconnected when the block
/// returns, including when the block throws, so a failure partway through does
/// not leave a mapping behind. Without one the caller owns the connection and
/// decides when — or whether — to disconnect it.
async fn finish_connect<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    inner: connection::Connection,
    block: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let Some(block) = block else {
        make_connection(strand, global, inner, out);
        return Ok(());
    };
    strand
        .with_slots(async move |strand, [mut handle]| {
            make_connection(strand, global, inner, &mut handle);
            let result = call!(strand, block, out, &handle).await;
            // Masking interrupts keeps a cancellation from skipping the
            // disconnect, which would leave the mapping behind — the one thing
            // the scoped form exists to prevent.
            let cleanup = strand
                .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                    let this = global
                        .types
                        .connection
                        .cast(&handle)
                        .expect("connection handle");
                    this.enter(strand, async move |strand, this| {
                        Connection::teardown(this, strand, false, None).await
                    })
                    .await
                })
                .await;
            match (result, cleanup) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(cause), Err(error)) => Err(error.caused_by(strand, cause)),
            }
        })
        .await
}

pub(crate) fn configure_module<'v, 'a>(
    module: ModuleBuilder<'v, 'a>,
    global: State<'v, Global<'v>>,
) -> ModuleBuilder<'v, 'a> {
    module
        .value("Connection", global.types.connection)
        .value("ConnectionInfo", global.types.connection_info)
        .function("connection", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let vfs = dolang_ext_shell::vfs(strand);
            let connection = if let Some(info) = global.types.connection_info.cast(&value) {
                Ok(info.enter_sync(strand, |_, v| connection::from_info(&vfs, &v.annex().info)))
            } else if let Some(name) = value.as_str(strand) {
                let name = name.to_string();
                connection::by_name(&vfs, &name).await
            } else {
                return Err(Error::type_error(
                    strand,
                    "argument must be a connection name or ConnectionInfo",
                ));
            }
            .into_sys(strand)?;
            make_connection(strand, global, connection, out);
            Ok(())
        })
        .function("connections", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let connections = connection::enumerate(&dolang_ext_shell::vfs(strand));
            global.types.connections.create_with_annex(
                strand,
                Connections(connections),
                global,
                out,
            );
            Ok(())
        })
        .function("universal_name", async move |strand, args, out| {
            let ([value], []) = unpack!(strand, args, 1, 0)?;
            let path = as_windows_path(strand, &value)
                .ok_or_else(|| Error::type_error(strand, "path must be an fs.windows.Path"))?;
            let name = connection::universal_name(&dolang_ext_shell::vfs(strand), path.to_path())
                .await
                .into_sys(strand)?;
            Output::set(strand, out, name.as_str());
            Ok(())
        })
        .function("connect", async move |strand, args, out| {
            let local_sym = global.syms.local;
            let user_sym = global.syms.user;
            let password_sym = global.syms.password;
            let kind_sym = global.syms.kind;
            let persistent_sym = global.syms.persistent;
            let save_sym = global.syms.save_credentials;
            let ([remote], [block, local, user, password, kind_arg, persistent, save]) = unpack!(
                strand,
                args,
                1,
                1,
                local_sym = None,
                user_sym = None,
                password_sym = None,
                kind_sym = None,
                persistent_sym = None,
                save_sym = None
            )?;
            let remote = remote
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "remote must be a Str"))?
                .into();
            let mut create = connection::Create::new(remote);
            if let Some(v) = local {
                create = create.local(optional_str(strand, &v, "local")?);
            }
            if let Some(v) = user {
                create = create.user(optional_str(strand, &v, "user")?);
            }
            if let Some(v) = password {
                create = create.password(optional_str(strand, &v, "password")?);
            }
            if let Some(v) = kind_arg {
                create = create.kind(kind(strand, global, &v)?);
            }
            if let Some(v) = persistent {
                create = create.persistent(expect_bool(strand, &v, "persistent")?);
            }
            if let Some(v) = save {
                create = create.save_credentials(expect_bool(strand, &v, "save_credentials")?);
            }
            let connection = connection::add(&dolang_ext_shell::vfs(strand), create)
                .await
                .into_sys(strand)?;
            finish_connect(strand, global, connection, block, out).await
        })
}
