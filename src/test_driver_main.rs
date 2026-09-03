use std::process::ExitCode;

mod process;
mod test_git;

fn main() -> ExitCode {
    if test_git::is_invocation() {
        return test_git::run();
    }

    if let Some(path) = std::env::var_os("GHERRIT_TEST_INTERCEPT_PATH") {
        // SAFETY: this is the single-threaded test-driver entrypoint, before
        // logging or the async runtime can start another thread.
        unsafe { std::env::set_var("PATH", path) };
    }

    let github_api_url = std::env::var("GHERRIT_GITHUB_API_URL").ok();
    process::run(gherrit::Runtime::test(github_api_url))
}
