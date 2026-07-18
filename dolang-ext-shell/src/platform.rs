use dolang::runtime::{State, vm::Builder};

use crate::global::Global;

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("sys.unix")
        .value("Errno", global.types.errno)
        .commit();

    builder
        .module("sys.linux")
        .value("LinuxErrno", global.types.linux_errno)
        .commit();

    builder
        .module("sys.macos")
        .value("MacosErrno", global.types.macos_errno)
        .commit();

    builder
        .module("sys.windows")
        .value("WinError", global.types.win_error)
        .value("Guid", global.types.guid)
        .commit();
}
