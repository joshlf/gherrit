//! Starts the normal binary with production runtime inputs.
//!
//! Test environment variables cannot change its endpoint or ID generator.

use std::process::ExitCode;

mod process;

fn main() -> ExitCode {
    process::run(gherrit::Runtime::production())
}
