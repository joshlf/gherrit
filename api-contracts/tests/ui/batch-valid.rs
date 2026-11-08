#![allow(dead_code)]
#[path = "../../../src/pre_push/batching.rs"]
mod batching;

use std::num::NonZeroUsize;

use batching::BatchPlan;

fn main() {
    let mut plan = BatchPlan::new(3, NonZeroUsize::new(2).unwrap());
    let attempt = plan.current().unwrap();
    assert_eq!(attempt.range(), 0..2);
    attempt.reject().unwrap();
    plan.current().unwrap().accept();
    let dropped = plan.current().unwrap();
    assert_eq!(dropped.range(), 1..2);
    drop(dropped);
    plan.current().unwrap().accept();
}
