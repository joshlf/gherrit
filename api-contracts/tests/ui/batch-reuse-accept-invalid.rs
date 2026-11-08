#![allow(dead_code)]
#[path = "../../../src/pre_push/batching.rs"]
mod batching;

use std::num::NonZeroUsize;

use batching::BatchPlan;

fn main() {
    let mut plan = BatchPlan::new(2, NonZeroUsize::new(2).unwrap());
    let attempt = plan.current().unwrap();
    attempt.accept();
    attempt.reject().unwrap();
}
