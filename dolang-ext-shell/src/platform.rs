use dolang::runtime::{State, vm::Register};

use crate::global::ErrorGlobal;

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, ErrorGlobal<'v>>) {
    builder
        .module("sys.unix")
        .value("Errno", global.types.errno)
        .commit();

    builder
        .module("sys.freebsd")
        .value("Errno", global.types.freebsd_errno)
        .commit();

    builder
        .module("sys.linux")
        .value("Errno", global.types.linux_errno)
        .commit();

    builder
        .module("sys.macos")
        .value("Errno", global.types.macos_errno)
        .commit();

    builder
        .module("sys.windows")
        .value("WinError", global.types.win_error)
        .commit();
}
