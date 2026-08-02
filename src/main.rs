use std::process::ExitCode;

#[cfg(gherrit_test)]
mod test_git;

mod process;

fn main() -> ExitCode {
    #[cfg(gherrit_test)]
    if test_git::is_invocation() {
        return test_git::run();
    }

    #[cfg(not(gherrit_test))]
    let runtime = gherrit::Runtime::production();
    #[cfg(gherrit_test)]
    let runtime = gherrit::Runtime::test();

    process::run(runtime)
}
