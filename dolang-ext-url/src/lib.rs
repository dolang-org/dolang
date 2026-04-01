#![deny(warnings)]

mod extension;
mod global;
mod url;

pub use self::url::{create_url, value_to_url};
