pub static RESOURCES: &[dolang::resource::Resource] =
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));

dolang::resources!(RESOURCES);
