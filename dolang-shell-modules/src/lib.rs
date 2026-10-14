#![deny(warnings)]

/// Stock module bytecode, typelibs, and guest source scripts.
pub static RESOURCES: &[dolang::resource::Resource] = &{
    const BUNDLED: &[dolang::resource::Resource] =
        include!(concat!(env!("OUT_DIR"), "/resources.rs"));
    const PROVISION: dolang::resource::Resource = dolang::resource::Resource {
        namespace: "dolang-shell-modules",
        name: "provision.dol",
        payload: dolang::resource::Payload::Text(include_str!("../resource/provision.dol")),
    };
    let mut resources = [PROVISION; BUNDLED.len() + 1];
    let mut i = 0;
    while i < BUNDLED.len() {
        resources[i] = BUNDLED[i];
        i += 1;
    }
    resources
};

dolang::resources!(RESOURCES);
