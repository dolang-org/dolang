#![deny(warnings)]

use dolang::resource::Resource;

/// Stock module bytecode and typelibs, as the enabled features select them.
pub static RESOURCES: &[Resource] = include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);

/// The guest script `libvirt` provisions Windows VMs with, bundled with it.
#[cfg(provision)]
static PROVISION: &[Resource] = &[Resource {
    namespace: "dolang-shell-modules",
    name: "provision.dol",
    payload: dolang::resource::Payload::Text(include_str!("../resource/provision.dol")),
}];

#[cfg(provision)]
dolang::resources!(PROVISION);
