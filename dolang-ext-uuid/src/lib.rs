#![deny(warnings)]

mod extension;
mod global;
mod guid;
mod uuid;

pub use self::guid::{create_guid, downcast_guid, value_to_guid};
pub use self::uuid::{cast_uuid, create_uuid, value_to_uuid};

pub use extension::UuidExt;

/// Resources bundled by this crate.
pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
