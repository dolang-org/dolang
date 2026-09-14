use crate::{
    connection::{Connection, ConnectionInfo, Connections},
    domain::JoinStatus,
    group::{Group, GroupInfo, GroupMembers, Groups},
    machine::{MachineInfo, ServerType},
    policy::AccountPolicy,
    share::{Share, ShareInfo, Shares},
    user::{User, UserFlags, UserInfo, Users},
};
use dolang::runtime::{
    Sym, Type,
    vm::{Register, Stateful},
};

pub(crate) struct Types<'v> {
    // Users
    pub(crate) user: Type<'v, User>,
    pub(crate) info: Type<'v, UserInfo>,
    pub(crate) flags: Type<'v, UserFlags>,
    pub(crate) users: Type<'v, Users>,
    // Groups
    pub(crate) group: Type<'v, Group>,
    pub(crate) group_info: Type<'v, GroupInfo>,
    pub(crate) groups: Type<'v, Groups>,
    pub(crate) group_members: Type<'v, GroupMembers>,
    // Account policy
    pub(crate) account_policy: Type<'v, AccountPolicy>,
    // Shares this machine publishes
    pub(crate) share: Type<'v, Share>,
    pub(crate) share_info: Type<'v, ShareInfo>,
    pub(crate) shares: Type<'v, Shares>,
    // Connections to shares this machine uses
    pub(crate) connection: Type<'v, Connection>,
    pub(crate) connection_info: Type<'v, ConnectionInfo>,
    pub(crate) connections: Type<'v, Connections>,
    // Domain membership
    pub(crate) join_status: Type<'v, JoinStatus>,
    pub(crate) machine_info: Type<'v, MachineInfo>,
    pub(crate) server_type: Type<'v, ServerType>,
}

/// Keyword-argument and method names, plus the `:UPPER_CASE:` constants this
/// extension accepts and reports in place of dedicated enum types.
///
/// Interning is idempotent, so a name that serves more than one purpose — such
/// as `name`, shared by user, group and share keywords — is one `Sym`.
pub(crate) struct Syms<'v> {
    // Shared keywords
    pub(crate) name: Sym<'v, 'v>,
    pub(crate) password: Sym<'v, 'v>,
    pub(crate) comment: Sym<'v, 'v>,
    pub(crate) path: Sym<'v, 'v>,
    pub(crate) kind: Sym<'v, 'v>,
    // User keywords
    pub(crate) full_name: Sym<'v, 'v>,
    pub(crate) user_comment: Sym<'v, 'v>,
    pub(crate) home_dir: Sym<'v, 'v>,
    pub(crate) home_dir_drive: Sym<'v, 'v>,
    pub(crate) profile: Sym<'v, 'v>,
    pub(crate) script_path: Sym<'v, 'v>,
    pub(crate) account_expires: Sym<'v, 'v>,
    pub(crate) disabled: Sym<'v, 'v>,
    pub(crate) password_never_expires: Sym<'v, 'v>,
    pub(crate) password_cannot_change: Sym<'v, 'v>,
    // Account policy keywords
    pub(crate) min_password_length: Sym<'v, 'v>,
    pub(crate) max_password_age: Sym<'v, 'v>,
    pub(crate) min_password_age: Sym<'v, 'v>,
    pub(crate) force_logoff: Sym<'v, 'v>,
    pub(crate) password_history_length: Sym<'v, 'v>,
    pub(crate) lockout_duration: Sym<'v, 'v>,
    pub(crate) lockout_observation_window: Sym<'v, 'v>,
    pub(crate) lockout_threshold: Sym<'v, 'v>,
    // Share keywords
    pub(crate) max_uses: Sym<'v, 'v>,
    pub(crate) special: Sym<'v, 'v>,
    pub(crate) temporary: Sym<'v, 'v>,
    pub(crate) sec_desc: Sym<'v, 'v>,
    // Share kinds (discrete)
    pub(crate) disktree: Sym<'v, 'v>,
    pub(crate) printq: Sym<'v, 'v>,
    pub(crate) device: Sym<'v, 'v>,
    pub(crate) ipc: Sym<'v, 'v>,
    // Connection keywords
    pub(crate) local: Sym<'v, 'v>,
    pub(crate) user: Sym<'v, 'v>,
    pub(crate) persistent: Sym<'v, 'v>,
    pub(crate) save_credentials: Sym<'v, 'v>,
    pub(crate) force: Sym<'v, 'v>,
    pub(crate) forget_credentials: Sym<'v, 'v>,
    // Connection resource kinds (discrete)
    pub(crate) disk: Sym<'v, 'v>,
    pub(crate) print: Sym<'v, 'v>,
    pub(crate) any: Sym<'v, 'v>,
    // Connection states (discrete)
    pub(crate) connected: Sym<'v, 'v>,
    pub(crate) remembered: Sym<'v, 'v>,
    // Domain join keywords
    pub(crate) machine: Sym<'v, 'v>,
    pub(crate) ou: Sym<'v, 'v>,
    pub(crate) dc: Sym<'v, 'v>,
    pub(crate) account: Sym<'v, 'v>,
    pub(crate) machine_password: Sym<'v, 'v>,
    pub(crate) create_account: Sym<'v, 'v>,
    pub(crate) delete_account: Sym<'v, 'v>,
    pub(crate) join_if_joined: Sym<'v, 'v>,
    pub(crate) unsecure: Sym<'v, 'v>,
    pub(crate) defer_spn: Sym<'v, 'v>,
    pub(crate) force_spn: Sym<'v, 'v>,
    pub(crate) dc_account: Sym<'v, 'v>,
    pub(crate) with_new_name: Sym<'v, 'v>,
    pub(crate) readonly: Sym<'v, 'v>,
    pub(crate) ambiguous_dc: Sym<'v, 'v>,
    pub(crate) no_netlogon_cache: Sym<'v, 'v>,
    pub(crate) no_account_reuse: Sym<'v, 'v>,
    pub(crate) reuse: Sym<'v, 'v>,
    pub(crate) default_password: Sym<'v, 'v>,
    pub(crate) skip_account_search: Sym<'v, 'v>,
    pub(crate) root_ca_certs: Sym<'v, 'v>,
    pub(crate) downlevel_priv_support: Sym<'v, 'v>,
    pub(crate) windows_path: Sym<'v, 'v>,
    pub(crate) online: Sym<'v, 'v>,
    // Join kinds (discrete)
    pub(crate) kind_unknown: Sym<'v, 'v>,
    pub(crate) kind_unjoined: Sym<'v, 'v>,
    pub(crate) kind_workgroup: Sym<'v, 'v>,
    pub(crate) kind_domain: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
    pub(crate) syms: Syms<'v>,
}

pub struct Tag;
impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            types: Types {
                user: builder.register_type(),
                info: builder.register_type(),
                flags: builder.register_type(),
                users: builder.register_type(),
                group: builder.register_type(),
                group_info: builder.register_type(),
                groups: builder.register_type(),
                group_members: builder.register_type(),
                account_policy: builder.register_type(),
                share: builder.register_type(),
                share_info: builder.register_type(),
                shares: builder.register_type(),
                connection: builder.register_type(),
                connection_info: builder.register_type(),
                connections: builder.register_type(),
                join_status: builder.register_type(),
                machine_info: builder.register_type(),
                server_type: builder.register_type(),
            },
            syms: Syms {
                name: builder.sym("name"),
                password: builder.sym("password"),
                comment: builder.sym("comment"),
                path: builder.sym("path"),
                kind: builder.sym("kind"),
                full_name: builder.sym("full_name"),
                user_comment: builder.sym("user_comment"),
                home_dir: builder.sym("home_dir"),
                home_dir_drive: builder.sym("home_dir_drive"),
                profile: builder.sym("profile"),
                script_path: builder.sym("script_path"),
                account_expires: builder.sym("account_expires"),
                disabled: builder.sym("disabled"),
                password_never_expires: builder.sym("password_never_expires"),
                password_cannot_change: builder.sym("password_cannot_change"),
                min_password_length: builder.sym("min_password_length"),
                max_password_age: builder.sym("max_password_age"),
                min_password_age: builder.sym("min_password_age"),
                force_logoff: builder.sym("force_logoff"),
                password_history_length: builder.sym("password_history_length"),
                lockout_duration: builder.sym("lockout_duration"),
                lockout_observation_window: builder.sym("lockout_observation_window"),
                lockout_threshold: builder.sym("lockout_threshold"),
                max_uses: builder.sym("max_uses"),
                special: builder.sym("special"),
                temporary: builder.sym("temporary"),
                sec_desc: builder.sym("sec_desc"),
                disktree: builder.sym("DISKTREE"),
                printq: builder.sym("PRINTQ"),
                device: builder.sym("DEVICE"),
                ipc: builder.sym("IPC"),
                local: builder.sym("local"),
                user: builder.sym("user"),
                persistent: builder.sym("persistent"),
                save_credentials: builder.sym("save_credentials"),
                force: builder.sym("force"),
                forget_credentials: builder.sym("forget_credentials"),
                disk: builder.sym("DISK"),
                print: builder.sym("PRINT"),
                any: builder.sym("ANY"),
                connected: builder.sym("CONNECTED"),
                remembered: builder.sym("REMEMBERED"),
                machine: builder.sym("machine"),
                ou: builder.sym("ou"),
                dc: builder.sym("dc"),
                account: builder.sym("account"),
                machine_password: builder.sym("machine_password"),
                create_account: builder.sym("create_account"),
                delete_account: builder.sym("delete_account"),
                join_if_joined: builder.sym("join_if_joined"),
                unsecure: builder.sym("unsecure"),
                defer_spn: builder.sym("defer_spn"),
                force_spn: builder.sym("force_spn"),
                dc_account: builder.sym("dc_account"),
                with_new_name: builder.sym("with_new_name"),
                readonly: builder.sym("readonly"),
                ambiguous_dc: builder.sym("ambiguous_dc"),
                no_netlogon_cache: builder.sym("no_netlogon_cache"),
                no_account_reuse: builder.sym("no_account_reuse"),
                reuse: builder.sym("reuse"),
                default_password: builder.sym("default_password"),
                skip_account_search: builder.sym("skip_account_search"),
                root_ca_certs: builder.sym("root_ca_certs"),
                downlevel_priv_support: builder.sym("downlevel_priv_support"),
                windows_path: builder.sym("windows_path"),
                online: builder.sym("online"),
                kind_unknown: builder.sym("UNKNOWN"),
                kind_unjoined: builder.sym("UNJOINED"),
                kind_workgroup: builder.sym("WORKGROUP"),
                kind_domain: builder.sym("DOMAIN"),
            },
        }
    }
}
