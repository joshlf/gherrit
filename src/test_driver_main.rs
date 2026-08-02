use std::process::ExitCode;

mod process;
mod test_git;

fn main() -> ExitCode {
    if test_git::is_invocation() {
        return test_git::run();
    }

    let github_api_url = std::env::var("GHERRIT_GITHUB_API_URL").ok();
    process::run(gherrit::Runtime::test(github_api_url))
}
