use std::process::ExitCode;

mod process;

fn main() -> ExitCode {
    process::run(gherrit::Runtime::production())
}
