#![allow(dead_code)]
#[path = "../../../src/pre_push/batching.rs"]
mod batching;

use std::num::NonZeroUsize;

use batching::{BatchAttempt, BatchPlan};

fn main() {
    let mut plan = BatchPlan::new(2, NonZeroUsize::new(2).unwrap());
    let _ = BatchAttempt { plan: &mut plan, range: 0..0 };
}
