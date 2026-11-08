//! Finds a GitHub token for the HTTP client.
//!
//! A nonempty `GITHUB_TOKEN` takes priority over the account selected by `gh`.
//! The caller chooses the endpoint and must keep the returned token out of
//! diagnostics and requests to unintended destinations.

use eyre::{Result, WrapErr, eyre};

use crate::cmd;
pub fn get_github_token() -> Result<String> {
    // Priority 1: GITHUB_TOKEN env var
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }

    // Priority 2: gh auth token
    let output = cmd!("gh auth token").output().wrap_err("Failed to run `gh auth token`")?;

    if output.status.success() {
        let token = String::from_utf8(output.stdout)?.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    Err(eyre!(
        "Could not find GitHub token. Please set GITHUB_TOKEN environment variable or login via `gh auth login`."
    ))
}
