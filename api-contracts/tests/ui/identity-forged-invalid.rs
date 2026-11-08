#![allow(dead_code)]
#[path = "../../../src/pre_push/identity.rs"]
mod identity;

use identity::ChangeId;

fn main() {
    let _ = ChangeId("invalid/id".to_owned());
}
