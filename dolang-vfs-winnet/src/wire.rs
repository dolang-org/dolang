use dolang_vfs::{
    error::Error,
    extension::{ExtContext, VfsExtension},
    path,
};
use dolang_winterop::security::{SecDesc, Sid};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use crate::backend;

/// Narrows a wire path to a Windows path.
///
/// The wire carries a path syntax tag, so a peer could in principle send a Unix
/// path where a Windows one belongs; every accessor here reports that as an
/// absent value rather than handing back a path the caller cannot use.
fn windows_path(path: &path::PathBuf) -> Option<path::Path<'_>> {
    (path.kind() == path::Kind::Windows).then(|| path.to_path())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareKind {
    #[default]
    DiskTree,
    PrintQueue,
    Device,
    Ipc,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub(crate) name: String,
    pub(crate) kind: ShareKind,
    pub(crate) special: bool,
    pub(crate) temporary: bool,
    pub(crate) comment: Option<String>,
    pub(crate) max_uses: Option<u32>,
    pub(crate) current_uses: u32,
    pub(crate) path: path::PathBuf,
    pub(crate) sec_desc: Option<SecDesc>,
}
impl ShareInfo {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> ShareKind {
        self.kind
    }
    pub fn special(&self) -> bool {
        self.special
    }
    pub fn temporary(&self) -> bool {
        self.temporary
    }
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
    pub fn max_uses(&self) -> Option<u32> {
        self.max_uses
    }
    pub fn current_uses(&self) -> u32 {
        self.current_uses
    }
    pub fn path(&self) -> Option<path::Path<'_>> {
        windows_path(&self.path)
    }
    /// The share's security descriptor, or `None` when the share uses the
    /// server service's default security.
    pub fn sec_desc(&self) -> Option<&SecDesc> {
        self.sec_desc.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCreate {
    pub(crate) name: String,
    pub(crate) path: path::PathBuf,
    pub(crate) kind: ShareKind,
    pub(crate) comment: Option<String>,
    pub(crate) max_uses: Option<u32>,
    pub(crate) special: bool,
    pub(crate) temporary: bool,
    pub(crate) sec_desc: Option<SecDesc>,
}
impl ShareCreate {
    pub fn new(name: String, path: path::PathBuf) -> Self {
        Self {
            name,
            path,
            kind: ShareKind::default(),
            comment: None,
            max_uses: None,
            special: false,
            temporary: false,
            sec_desc: None,
        }
    }
    pub fn kind(mut self, value: ShareKind) -> Self {
        self.kind = value;
        self
    }
    pub fn comment(mut self, value: Option<String>) -> Self {
        self.comment = value;
        self
    }
    pub fn max_uses(mut self, value: Option<u32>) -> Self {
        self.max_uses = value;
        self
    }
    pub fn special(mut self, value: bool) -> Self {
        self.special = value;
        self
    }
    pub fn temporary(mut self, value: bool) -> Self {
        self.temporary = value;
        self
    }
    pub fn sec_desc(mut self, value: SecDesc) -> Self {
        self.sec_desc = Some(value);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareUpdate {
    pub(crate) comment: Option<Option<String>>,
    pub(crate) max_uses: Option<Option<u32>>,
    pub(crate) sec_desc: Option<SecDesc>,
}
impl ShareUpdate {
    pub fn comment(mut self, value: Option<String>) -> Self {
        self.comment = Some(value);
        self
    }
    pub fn max_uses(mut self, value: Option<u32>) -> Self {
        self.max_uses = Some(value);
        self
    }
    pub fn sec_desc(mut self, value: SecDesc) -> Self {
        self.sec_desc = Some(value);
        self
    }
}

/// The resource type of a client connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionKind {
    #[default]
    Disk,
    Print,
    Any,
}

/// Whether a connection is live or only recorded in the user's profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    /// Currently connected.
    #[default]
    Connected,
    /// Saved in the profile but not connected, which is what `net use` shows as
    /// `Unavailable`.
    Remembered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub(crate) local: Option<String>,
    pub(crate) remote: String,
    pub(crate) provider: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) kind: ConnectionKind,
    pub(crate) state: ConnectionState,
    pub(crate) persistent: bool,
}
impl ConnectionInfo {
    /// The redirected local device, or `None` for a deviceless connection.
    pub fn local(&self) -> Option<&str> {
        self.local.as_deref()
    }
    /// The remote resource in UNC form.
    pub fn remote(&self) -> &str {
        &self.remote
    }
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }
    /// The account the connection authenticated as, when the provider reports
    /// one.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
    pub fn kind(&self) -> ConnectionKind {
        self.kind
    }
    pub fn state(&self) -> ConnectionState {
        self.state
    }
    /// Whether the connection is recorded in the profile and restored at logon.
    ///
    /// Independent of [`state`](Self::state): a restored mapping whose server is
    /// currently reachable is both [`Connected`](ConnectionState::Connected) and
    /// persistent.
    pub fn persistent(&self) -> bool {
        self.persistent
    }
    /// The name this connection is addressed by, preferring the local device.
    pub fn name(&self) -> &str {
        self.local.as_deref().unwrap_or(&self.remote)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionCreate {
    pub(crate) remote: String,
    pub(crate) local: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) kind: ConnectionKind,
    pub(crate) persistent: bool,
    pub(crate) save_credentials: Option<bool>,
}
impl ConnectionCreate {
    pub fn new(remote: String) -> Self {
        Self {
            remote,
            local: None,
            user: None,
            password: None,
            kind: ConnectionKind::default(),
            persistent: false,
            save_credentials: None,
        }
    }
    /// Redirects a local device, such as `Z:`.
    ///
    /// Required for a [`persistent`](Self::persistent) connection: Windows only
    /// remembers connections that redirect a device.
    pub fn local(mut self, value: Option<String>) -> Self {
        self.local = value;
        self
    }
    pub fn user(mut self, value: Option<String>) -> Self {
        self.user = value;
        self
    }
    pub fn password(mut self, value: Option<String>) -> Self {
        self.password = value;
        self
    }
    pub fn kind(mut self, value: ConnectionKind) -> Self {
        self.kind = value;
        self
    }
    /// Records the connection in the profile so it is restored at logon.
    pub fn persistent(mut self, value: bool) -> Self {
        self.persistent = value;
        self
    }
    /// Overrides whether supplied credentials are saved for the target server.
    ///
    /// Unset means saving them for a persistent connection and not otherwise: a
    /// persistent mapping has nothing to authenticate with at logon unless its
    /// credentials were stored.
    pub fn save_credentials(mut self, value: bool) -> Self {
        self.save_credentials = Some(value);
        self
    }
    /// Whether credentials will be stored, resolving the unset default.
    pub fn stores_credentials(&self) -> bool {
        self.save_credentials.unwrap_or(self.persistent) && self.password.is_some()
    }
}

bitflags::bitflags! {
    /// Native `USER_INFO_4::usri4_flags`, including unknown bits.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct UserFlags: u32 {
        const SCRIPT = 0x0001;
        const ACCOUNT_DISABLED = 0x0002;
        const HOME_DIR_REQUIRED = 0x0008;
        const LOCKOUT = 0x0010;
        const PASSWORD_NOT_REQUIRED = 0x0020;
        const PASSWORD_CANNOT_CHANGE = 0x0040;
        const ENCRYPTED_TEXT_PASSWORD_ALLOWED = 0x0080;
        const TEMP_DUPLICATE_ACCOUNT = 0x0100;
        const NORMAL_ACCOUNT = 0x0200;
        const INTERDOMAIN_TRUST_ACCOUNT = 0x0800;
        const WORKSTATION_TRUST_ACCOUNT = 0x1000;
        const SERVER_TRUST_ACCOUNT = 0x2000;
        const PASSWORD_NEVER_EXPIRES = 0x10000;
        const MNS_LOGON_ACCOUNT = 0x20000;
        const SMARTCARD_REQUIRED = 0x40000;
        const TRUSTED_FOR_DELEGATION = 0x80000;
        const NOT_DELEGATED = 0x100000;
        const USE_DES_KEY_ONLY = 0x200000;
        const DONT_REQUIRE_PREAUTH = 0x400000;
        const PASSWORD_EXPIRED = 0x800000;
        const TRUSTED_TO_AUTHENTICATE_FOR_DELEGATION = 0x1000000;
        const NO_AUTH_DATA_REQUIRED = 0x2000000;
        const PARTIAL_SECRETS_ACCOUNT = 0x4000000;
        const USE_AES_KEYS = 0x8000000;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub(crate) sid: Sid,
    pub(crate) name: String,
    pub(crate) full_name: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) user_comment: Option<String>,
    pub(crate) home_dir: Option<path::PathBuf>,
    pub(crate) home_dir_drive: Option<String>,
    pub(crate) profile: Option<path::PathBuf>,
    pub(crate) script_path: Option<path::PathBuf>,
    pub(crate) flags: UserFlags,
    pub(crate) password_age: u64,
    pub(crate) password_expired: bool,
    pub(crate) last_logon: Option<u64>,
    pub(crate) account_expires: Option<u64>,
    pub(crate) bad_password_count: u32,
    pub(crate) logon_count: u32,
}
impl UserInfo {
    pub fn sid(&self) -> &Sid {
        &self.sid
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn full_name(&self) -> Option<&str> {
        self.full_name.as_deref()
    }
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
    pub fn user_comment(&self) -> Option<&str> {
        self.user_comment.as_deref()
    }
    pub fn home_dir(&self) -> Option<path::Path<'_>> {
        self.home_dir.as_ref().and_then(windows_path)
    }
    pub fn home_dir_drive(&self) -> Option<&str> {
        self.home_dir_drive.as_deref()
    }
    pub fn profile(&self) -> Option<path::Path<'_>> {
        self.profile.as_ref().and_then(windows_path)
    }
    pub fn script_path(&self) -> Option<path::Path<'_>> {
        self.script_path.as_ref().and_then(windows_path)
    }
    pub fn flags(&self) -> UserFlags {
        self.flags
    }
    pub fn password_age(&self) -> u64 {
        self.password_age
    }
    pub fn password_expired(&self) -> bool {
        self.password_expired
    }
    pub fn last_logon(&self) -> Option<u64> {
        self.last_logon
    }
    pub fn account_expires(&self) -> Option<u64> {
        self.account_expires
    }
    pub fn bad_password_count(&self) -> u32 {
        self.bad_password_count
    }
    pub fn logon_count(&self) -> u32 {
        self.logon_count
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserUpdate {
    pub(crate) name: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) full_name: Option<Option<String>>,
    pub(crate) comment: Option<Option<String>>,
    pub(crate) user_comment: Option<Option<String>>,
    pub(crate) home_dir: Option<Option<path::PathBuf>>,
    pub(crate) home_dir_drive: Option<Option<String>>,
    pub(crate) profile: Option<Option<path::PathBuf>>,
    pub(crate) script_path: Option<Option<path::PathBuf>>,
    pub(crate) account_expires: Option<Option<u64>>,
    pub(crate) set_flags: UserFlags,
    pub(crate) clear_flags: UserFlags,
}

impl UserUpdate {
    pub fn name(mut self, value: String) -> Self {
        self.name = Some(value);
        self
    }
    fn flag(mut self, flag: UserFlags, value: bool) -> Self {
        if value {
            self.set_flags.insert(flag);
            self.clear_flags.remove(flag);
        } else {
            self.clear_flags.insert(flag);
            self.set_flags.remove(flag);
        }
        self
    }
    pub fn disabled(self, value: bool) -> Self {
        self.flag(UserFlags::ACCOUNT_DISABLED, value)
    }
    pub fn password_never_expires(self, value: bool) -> Self {
        self.flag(UserFlags::PASSWORD_NEVER_EXPIRES, value)
    }
    pub fn password_cannot_change(self, value: bool) -> Self {
        self.flag(UserFlags::PASSWORD_CANNOT_CHANGE, value)
    }
    pub fn password(mut self, value: String) -> Self {
        self.password = Some(value);
        self
    }
    pub fn full_name(mut self, value: Option<String>) -> Self {
        self.full_name = Some(value);
        self
    }
    pub fn comment(mut self, value: Option<String>) -> Self {
        self.comment = Some(value);
        self
    }
    pub fn user_comment(mut self, value: Option<String>) -> Self {
        self.user_comment = Some(value);
        self
    }
    pub fn home_dir(mut self, value: Option<path::PathBuf>) -> Self {
        self.home_dir = Some(value);
        self
    }
    pub fn home_dir_drive(mut self, value: Option<String>) -> Self {
        self.home_dir_drive = Some(value);
        self
    }
    pub fn profile(mut self, value: Option<path::PathBuf>) -> Self {
        self.profile = Some(value);
        self
    }
    pub fn script_path(mut self, value: Option<path::PathBuf>) -> Self {
        self.script_path = Some(value);
        self
    }
    pub fn account_expires(mut self, value: Option<u64>) -> Self {
        self.account_expires = Some(value);
        self
    }
    pub fn set_flags(&self) -> UserFlags {
        self.set_flags
    }
    pub fn clear_flags(&self) -> UserFlags {
        self.clear_flags
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCreate {
    pub(crate) name: String,
    pub(crate) password: String,
    pub(crate) update: UserUpdate,
}
impl UserCreate {
    pub fn new(name: String, password: String) -> Self {
        Self {
            name,
            password,
            update: UserUpdate::default(),
        }
    }
    pub fn update(mut self, update: UserUpdate) -> Self {
        self.update = update;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub(crate) sid: Sid,
    pub(crate) name: String,
    pub(crate) comment: Option<String>,
}
impl GroupInfo {
    pub fn sid(&self) -> &Sid {
        &self.sid
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupUpdate {
    pub(crate) name: Option<String>,
    pub(crate) comment: Option<Option<String>>,
}
impl GroupUpdate {
    pub fn name(mut self, value: String) -> Self {
        self.name = Some(value);
        self
    }
    pub fn comment(mut self, value: Option<String>) -> Self {
        self.comment = Some(value);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCreate {
    pub(crate) name: String,
    pub(crate) comment: Option<String>,
}
impl GroupCreate {
    pub fn new(name: String) -> Self {
        Self {
            name,
            comment: None,
        }
    }
    pub fn comment(mut self, value: Option<String>) -> Self {
        self.comment = value;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPolicy {
    pub(crate) min_password_length: u32,
    pub(crate) max_password_age: Option<u64>,
    pub(crate) min_password_age: u64,
    pub(crate) force_logoff: Option<u64>,
    pub(crate) password_history_length: u32,
    pub(crate) lockout_duration: u64,
    pub(crate) lockout_observation_window: u64,
    pub(crate) lockout_threshold: u32,
}
impl AccountPolicy {
    pub fn min_password_length(&self) -> u32 {
        self.min_password_length
    }
    pub fn max_password_age(&self) -> Option<u64> {
        self.max_password_age
    }
    pub fn min_password_age(&self) -> u64 {
        self.min_password_age
    }
    pub fn force_logoff(&self) -> Option<u64> {
        self.force_logoff
    }
    pub fn password_history_length(&self) -> u32 {
        self.password_history_length
    }
    pub fn lockout_duration(&self) -> u64 {
        self.lockout_duration
    }
    pub fn lockout_observation_window(&self) -> u64 {
        self.lockout_observation_window
    }
    pub fn lockout_threshold(&self) -> u32 {
        self.lockout_threshold
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPolicyUpdate {
    pub(crate) min_password_length: Option<u32>,
    pub(crate) max_password_age: Option<Option<u64>>,
    pub(crate) min_password_age: Option<u64>,
    pub(crate) force_logoff: Option<Option<u64>>,
    pub(crate) password_history_length: Option<u32>,
    pub(crate) lockout_duration: Option<u64>,
    pub(crate) lockout_observation_window: Option<u64>,
    pub(crate) lockout_threshold: Option<u32>,
}
impl AccountPolicyUpdate {
    pub fn min_password_length(mut self, value: u32) -> Self {
        self.min_password_length = Some(value);
        self
    }
    pub fn max_password_age(mut self, value: Option<u64>) -> Self {
        self.max_password_age = Some(value);
        self
    }
    pub fn min_password_age(mut self, value: u64) -> Self {
        self.min_password_age = Some(value);
        self
    }
    pub fn force_logoff(mut self, value: Option<u64>) -> Self {
        self.force_logoff = Some(value);
        self
    }
    pub fn password_history_length(mut self, value: u32) -> Self {
        self.password_history_length = Some(value);
        self
    }
    pub fn lockout_duration(mut self, value: u64) -> Self {
        self.lockout_duration = Some(value);
        self
    }
    pub fn lockout_observation_window(mut self, value: u64) -> Self {
        self.lockout_observation_window = Some(value);
        self
    }
    pub fn lockout_threshold(mut self, value: u32) -> Self {
        self.lockout_threshold = Some(value);
        self
    }
}

/// How the machine is currently named on the network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinKind {
    /// The join state could not be determined.
    #[default]
    Unknown,
    /// The machine belongs to neither a workgroup nor a domain.
    Unjoined,
    /// The name is a workgroup.
    Workgroup,
    /// The name is a domain.
    Domain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinInfo {
    pub(crate) kind: JoinKind,
    pub(crate) name: Option<String>,
}
impl JoinInfo {
    pub fn kind(&self) -> JoinKind {
        self.kind
    }
    /// The workgroup or domain name, absent when unjoined or unknown.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

bitflags::bitflags! {
    /// Native `SERVER_INFO_101::sv101_type`, including unknown bits.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct ServerType: u32 {
        const WORKSTATION = 0x0000_0001;
        const SERVER = 0x0000_0002;
        const SQLSERVER = 0x0000_0004;
        const DOMAIN_CTRL = 0x0000_0008;
        const DOMAIN_BAKCTRL = 0x0000_0010;
        const TIME_SOURCE = 0x0000_0020;
        const AFP = 0x0000_0040;
        const NOVELL = 0x0000_0080;
        const DOMAIN_MEMBER = 0x0000_0100;
        const PRINTQ_SERVER = 0x0000_0200;
        const DIALIN_SERVER = 0x0000_0400;
        const SERVER_UNIX = 0x0000_0800;
        const NT = 0x0000_1000;
        const WFW = 0x0000_2000;
        const SERVER_MFPN = 0x0000_4000;
        const SERVER_NT = 0x0000_8000;
        const POTENTIAL_BROWSER = 0x0001_0000;
        const BACKUP_BROWSER = 0x0002_0000;
        const MASTER_BROWSER = 0x0004_0000;
        const DOMAIN_MASTER = 0x0008_0000;
        const SERVER_OSF = 0x0010_0000;
        const SERVER_VMS = 0x0020_0000;
        const WINDOWS = 0x0040_0000;
        const DFS = 0x0080_0000;
        const CLUSTER_NT = 0x0100_0000;
        const TERMINALSERVER = 0x0200_0000;
        const CLUSTER_VS_NT = 0x0400_0000;
        const DCE = 0x1000_0000;
        const ALTERNATE_XPORT = 0x2000_0000;
        const LOCAL_LIST_ONLY = 0x4000_0000;
        const DOMAIN_ENUM = 0x8000_0000;
    }
}

/// Machine identity from `NetWkstaGetInfo` and `NetServerGetInfo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineInfo {
    pub(crate) name: String,
    pub(crate) domain: String,
    pub(crate) version_major: u32,
    pub(crate) version_minor: u32,
    pub(crate) comment: Option<String>,
    pub(crate) server_type: ServerType,
    pub(crate) server_started: bool,
}
impl MachineInfo {
    /// The NetBIOS computer name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The domain or workgroup the machine belongs to.
    pub fn domain(&self) -> &str {
        &self.domain
    }
    pub fn version_major(&self) -> u32 {
        self.version_major
    }
    pub fn version_minor(&self) -> u32 {
        self.version_minor
    }
    /// The server comment, absent when unset or when the server service is
    /// not running.
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
    /// The advertised server role mask, empty when the server service is not
    /// running.
    pub fn server_type(&self) -> ServerType {
        self.server_type
    }
    /// Whether the server service supplied the role mask and comment.
    ///
    /// Machine identity comes from the workstation service, so a stopped
    /// server service leaves only the server-side fields unpopulated rather
    /// than failing the whole query.
    pub fn server_started(&self) -> bool {
        self.server_started
    }
}

bitflags::bitflags! {
    /// Native `NetJoinDomain` option flags.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct JoinOptions: u32 {
        const JOIN_DOMAIN = 0x0000_0001;
        const ACCT_CREATE = 0x0000_0002;
        const DOMAIN_JOIN_IF_JOINED = 0x0000_0020;
        const JOIN_UNSECURE = 0x0000_0040;
        const MACHINE_PWD_PASSED = 0x0000_0080;
        const DEFER_SPN_SET = 0x0000_0100;
        const JOIN_DC_ACCOUNT = 0x0000_0200;
        const JOIN_WITH_NEW_NAME = 0x0000_0400;
        const JOIN_READONLY = 0x0000_0800;
        const AMBIGUOUS_DC = 0x0000_1000;
        const NO_NETLOGON_CACHE = 0x0000_2000;
        const FORCE_SPN_SET = 0x0001_0000;
        const NO_ACCT_REUSE = 0x0002_0000;
    }
}

/// A request to join a domain.
///
/// The join does not take effect until the machine is restarted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub(crate) domain: String,
    pub(crate) ou: Option<String>,
    pub(crate) account: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) options: JoinOptions,
}
impl JoinRequest {
    pub fn new(domain: String) -> Self {
        Self {
            domain,
            ou: None,
            account: None,
            password: None,
            options: JoinOptions::JOIN_DOMAIN,
        }
    }
    /// The organizational unit to create the computer account in.
    pub fn ou(mut self, value: Option<String>) -> Self {
        self.ou = value;
        self
    }
    /// Domain credentials authorized to perform the join.
    pub fn credentials(mut self, account: String, password: String) -> Self {
        self.account = Some(account);
        self.password = Some(password);
        self.options.remove(JoinOptions::MACHINE_PWD_PASSED);
        self
    }
    /// Joins with a pre-created computer account password instead of user
    /// credentials.
    pub fn machine_password(mut self, value: String) -> Self {
        self.account = None;
        self.password = Some(value);
        self.options.insert(JoinOptions::MACHINE_PWD_PASSED);
        self
    }
    fn flag(mut self, flag: JoinOptions, value: bool) -> Self {
        self.options.set(flag, value);
        self
    }
    pub fn create_account(self, value: bool) -> Self {
        self.flag(JoinOptions::ACCT_CREATE, value)
    }
    pub fn join_if_joined(self, value: bool) -> Self {
        self.flag(JoinOptions::DOMAIN_JOIN_IF_JOINED, value)
    }
    pub fn unsecure(self, value: bool) -> Self {
        self.flag(JoinOptions::JOIN_UNSECURE, value)
    }
    pub fn defer_spn(self, value: bool) -> Self {
        self.flag(JoinOptions::DEFER_SPN_SET, value)
    }
    pub fn force_spn(self, value: bool) -> Self {
        self.flag(JoinOptions::FORCE_SPN_SET, value)
    }
    pub fn dc_account(self, value: bool) -> Self {
        self.flag(JoinOptions::JOIN_DC_ACCOUNT, value)
    }
    pub fn with_new_name(self, value: bool) -> Self {
        self.flag(JoinOptions::JOIN_WITH_NEW_NAME, value)
    }
    pub fn readonly(self, value: bool) -> Self {
        self.flag(JoinOptions::JOIN_READONLY, value)
    }
    pub fn ambiguous_dc(self, value: bool) -> Self {
        self.flag(JoinOptions::AMBIGUOUS_DC, value)
    }
    pub fn no_netlogon_cache(self, value: bool) -> Self {
        self.flag(JoinOptions::NO_NETLOGON_CACHE, value)
    }
    pub fn no_account_reuse(self, value: bool) -> Self {
        self.flag(JoinOptions::NO_ACCT_REUSE, value)
    }
    pub fn options(&self) -> JoinOptions {
        self.options
    }
}

/// A request to leave the current domain.
///
/// The change does not take effect until the machine is restarted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnjoinRequest {
    pub(crate) account: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) delete_account: bool,
}
impl UnjoinRequest {
    /// Domain credentials authorized to disable the computer account.
    pub fn credentials(mut self, account: String, password: String) -> Self {
        self.account = Some(account);
        self.password = Some(password);
        self
    }
    /// Disables the computer account in the domain as part of the unjoin.
    pub fn delete_account(mut self, value: bool) -> Self {
        self.delete_account = value;
        self
    }
}

/// A request to rename the machine within its domain.
///
/// The rename does not take effect until the machine is restarted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRequest {
    pub(crate) name: String,
    pub(crate) account: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) create_account: bool,
}
impl RenameRequest {
    pub fn new(name: String) -> Self {
        Self {
            name,
            account: None,
            password: None,
            create_account: false,
        }
    }
    /// Domain credentials authorized to rename the computer account.
    pub fn credentials(mut self, account: String, password: String) -> Self {
        self.account = Some(account);
        self.password = Some(password);
        self
    }
    /// Creates the renamed computer account if it does not already exist.
    pub fn create_account(mut self, value: bool) -> Self {
        self.create_account = value;
        self
    }
}

bitflags::bitflags! {
    /// Native `NetProvisionComputerAccount` option flags.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct ProvisionOptions: u32 {
        const DOWNLEVEL_PRIV_SUPPORT = 0x0000_0001;
        const REUSE_ACCOUNT = 0x0000_0002;
        const USE_DEFAULT_PASSWORD = 0x0000_0004;
        const SKIP_ACCOUNT_SEARCH = 0x0000_0008;
        const ROOT_CA_CERTS = 0x0000_0010;
    }
}

/// A request to mint an offline domain join blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    pub(crate) domain: String,
    pub(crate) machine: String,
    pub(crate) ou: Option<String>,
    pub(crate) dc: Option<String>,
    pub(crate) options: ProvisionOptions,
}
impl ProvisionRequest {
    pub fn new(domain: String, machine: String) -> Self {
        Self {
            domain,
            machine,
            ou: None,
            dc: None,
            options: ProvisionOptions::empty(),
        }
    }
    /// The organizational unit to create the computer account in.
    pub fn ou(mut self, value: Option<String>) -> Self {
        self.ou = value;
        self
    }
    /// The domain controller to provision against.
    pub fn dc(mut self, value: Option<String>) -> Self {
        self.dc = value;
        self
    }
    fn flag(mut self, flag: ProvisionOptions, value: bool) -> Self {
        self.options.set(flag, value);
        self
    }
    pub fn reuse(self, value: bool) -> Self {
        self.flag(ProvisionOptions::REUSE_ACCOUNT, value)
    }
    pub fn default_password(self, value: bool) -> Self {
        self.flag(ProvisionOptions::USE_DEFAULT_PASSWORD, value)
    }
    pub fn skip_account_search(self, value: bool) -> Self {
        self.flag(ProvisionOptions::SKIP_ACCOUNT_SEARCH, value)
    }
    pub fn root_ca_certs(self, value: bool) -> Self {
        self.flag(ProvisionOptions::ROOT_CA_CERTS, value)
    }
    pub fn downlevel_priv_support(self, value: bool) -> Self {
        self.flag(ProvisionOptions::DOWNLEVEL_PRIV_SUPPORT, value)
    }
    pub fn options(&self) -> ProvisionOptions {
        self.options
    }
}

/// A request to apply an offline domain join blob to a Windows installation.
///
/// The join does not take effect until the target installation is started or
/// restarted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineJoinRequest {
    pub(crate) blob: Vec<u8>,
    pub(crate) windows_path: path::PathBuf,
    pub(crate) online: bool,
}
impl OfflineJoinRequest {
    /// Applies `blob` to the Windows directory at `windows_path`.
    pub fn new(blob: Vec<u8>, windows_path: path::PathBuf) -> Self {
        Self {
            blob,
            windows_path,
            online: false,
        }
    }
    /// Marks the target installation as the running system.
    pub fn online(mut self, value: bool) -> Self {
        self.online = value;
        self
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WinNetRequest {
    UserByName {
        name: String,
    },
    UserBySid {
        sid: Sid,
    },
    UsersPage {
        resume: u64,
    },
    CreateUser(UserCreate),
    Info {
        name: String,
        sid: Sid,
    },
    Update {
        name: String,
        sid: Sid,
        update: UserUpdate,
    },
    Delete {
        name: String,
        sid: Sid,
    },
    GroupByName {
        name: String,
    },
    GroupsPage {
        resume: u64,
    },
    CreateGroup(GroupCreate),
    GroupInfo {
        name: String,
        sid: Sid,
    },
    GroupUpdate {
        name: String,
        sid: Sid,
        update: GroupUpdate,
    },
    GroupMembersPage {
        name: String,
        sid: Sid,
        resume: u64,
    },
    GroupAddMember {
        name: String,
        sid: Sid,
        member: Sid,
    },
    GroupRemoveMember {
        name: String,
        sid: Sid,
        member: Sid,
    },
    GroupDelete {
        name: String,
        sid: Sid,
    },
    AccountRights {
        sid: Sid,
    },
    GrantAccountRight {
        sid: Sid,
        right: String,
    },
    RevokeAccountRight {
        sid: Sid,
        right: String,
    },
    AccountPolicy,
    UpdateAccountPolicy(AccountPolicyUpdate),
    ShareInfo {
        name: String,
    },
    SharesPage {
        resume: u64,
    },
    CreateShare(ShareCreate),
    UpdateShare {
        name: String,
        update: ShareUpdate,
    },
    DeleteShare {
        name: String,
    },
    JoinStatus,
    MachineInfo,
    JoinDomain(Box<JoinRequest>),
    UnjoinDomain(UnjoinRequest),
    RenameMachine(RenameRequest),
    ProvisionComputer(Box<ProvisionRequest>),
    ApplyOfflineJoin(Box<OfflineJoinRequest>),
    ConnectionInfo {
        name: String,
    },
    ConnectionsPage {
        resume: u64,
    },
    AddConnection(Box<ConnectionCreate>),
    CancelConnection {
        name: String,
        force: bool,
        forget_credentials: Option<bool>,
    },
    UniversalName {
        path: path::PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WinNetResponse {
    User {
        name: String,
        sid: Sid,
    },
    UsersPage {
        users: Vec<UserInfo>,
        resume: u64,
        done: bool,
    },
    Info(Box<UserInfo>),
    Deleted,
    Group {
        name: String,
        sid: Sid,
    },
    GroupsPage {
        groups: Vec<GroupInfo>,
        resume: u64,
        done: bool,
    },
    GroupInfo(GroupInfo),
    GroupMembersPage {
        members: Vec<Sid>,
        resume: u64,
        done: bool,
    },
    Unit,
    AccountRights(Vec<String>),
    AccountPolicy(AccountPolicy),
    ShareInfo(Box<ShareInfo>),
    SharesPage {
        shares: Vec<ShareInfo>,
        resume: u64,
        done: bool,
    },
    JoinInfo(JoinInfo),
    MachineInfo(Box<MachineInfo>),
    Blob(Vec<u8>),
    ConnectionInfo(Box<ConnectionInfo>),
    ConnectionsPage {
        connections: Vec<ConnectionInfo>,
        resume: u64,
        done: bool,
    },
    UniversalName(String),
}

pub(crate) struct WinNetExt;
impl VfsExtension for WinNetExt {
    type Request = WinNetRequest;
    type Response = Result<WinNetResponse, Error>;
    const NAME: &'static str = "dolang-vfs-winnet";
    const VERSION: u16 = 6;
    const AVAILABLE: bool = cfg!(windows);
    async fn handle(&self, ctx: &mut ExtContext<'_>, request: WinNetRequest) -> Self::Response {
        #[cfg(windows)]
        return backend::handle(ctx, request).await;
        #[cfg(not(windows))]
        {
            let _ = (ctx, request);
            unreachable!("unavailable VFS extension was dispatched")
        }
    }
}
dolang_vfs::extension::vfs_extension!(WinNetExt);
