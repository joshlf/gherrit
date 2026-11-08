//! Shipping entry point. Construct production dependencies here so test-only
//! environment variables cannot change the endpoint or ID generator.

use std::process::ExitCode;

mod process;

fn main() -> ExitCode {
    process::run(gherrit::Runtime::production())
}
