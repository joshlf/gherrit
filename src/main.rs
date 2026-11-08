//! Entry point for the normal binary. Choose production inputs here so test
//! environment variables cannot change the endpoint or ID generator.

use std::process::ExitCode;

mod process;

fn main() -> ExitCode {
    process::run(gherrit::Runtime::production())
}
