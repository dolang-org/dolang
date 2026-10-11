#![deny(warnings)]

#[path = "common/typeck.rs"]
mod harness;

use harness::run;

include!(concat!(env!("OUT_DIR"), "/generated_typeck_tests.rs"));
