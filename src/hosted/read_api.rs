//! Read api.

use super::*;

/// Bounded read-only gh adapter; auth is explicit and mutations are not an API.
pub(super) struct ReadApi {
    pub(super) token: String,
    pub(super) deadline: Instant,
}
impl ReadApi {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            token: util::get_github_token()?,
            deadline: Instant::now() + Duration::from_secs(120),
        })
    }
    pub(super) fn call(
        &mut self,
        route: &str,
        request: Option<Value>,
        budget: &mut ReadBudget,
    ) -> Result<Value> {
        let response_limit = budget.response_limit()?;
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| eyre!("GitHub inspection deadline exceeded"))?;
        let mut command = util::cmd(
            "gh",
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                if request.is_some() { "POST" } else { "GET" },
                route,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2026-03-10",
            ],
        );
        command.env("GH_TOKEN", &self.token).env("GH_PROMPT_DISABLED", "1");
        let input = if let Some(request) = request {
            // The only POST route is the read-only GraphQL document above.
            if route != "graphql" || request["query"] != QUERY {
                bail!("Read-only inspection rejects this request");
            }
            command.args(["--input", "-"]);
            let mut input = subprocess::RegularFileStdinBuilder::new()?;
            input.write_all(&serde_json::to_vec(&request)?)?;
            Some(input.finish()?)
        } else {
            None
        };
        let output = subprocess::local_output(
            &mut command,
            input,
            remaining.min(Duration::from_secs(60)),
            response_limit,
        )?;
        if !output.status.success() {
            bail!(
                "GitHub inspection request failed; check credentials, permissions, and service availability"
            );
        }
        budget.decode(&output.stdout)
    }
}
