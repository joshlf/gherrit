#![allow(dead_code)]
#[path = "../../../src/pre_push/identity.rs"]
mod identity;

use identity::ChangeId;

fn main() {
    let id = ChangeId::parse(b"legacy09").unwrap();
    assert_eq!(id.as_str(), "legacy09");
    assert_eq!(id.clone().to_string(), "legacy09");
}
