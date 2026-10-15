//! Api.

use super::*;

pub(super) struct Api {
    pub(super) token: String,
    pub(super) journal: File,
    pub(super) deadline: Instant,
    pub(super) requests: usize,
    #[cfg(test)]
    pub(super) test_now: std::cell::RefCell<std::collections::VecDeque<Instant>>,
}
impl Api {
    pub(super) fn remaining_request_time(&self) -> Result<Duration> {
        #[cfg(not(test))]
        let now = Instant::now();
        #[cfg(test)]
        let now = self.test_now.borrow_mut().pop_front().unwrap_or_else(Instant::now);
        self.deadline
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(Duration::from_secs(30)))
            .ok_or_else(|| eyre!("Hosted probe exhausted its 180-second deadline"))
    }

    pub(super) fn next_poll_delay(&self, polls_left: usize) -> Result<Option<Duration>> {
        if polls_left == 0 {
            return Ok(None);
        }
        Ok(Some(self.remaining_request_time()?.min(Duration::from_secs(1))))
    }

    pub(super) fn call(
        &mut self,
        stage: &str,
        method: &str,
        route: &str,
        request: Option<Value>,
    ) -> Result<Value> {
        if self.requests == MAX_REQUESTS {
            bail!("Hosted probe exhausted its 40-request budget");
        }
        self.remaining_request_time()?;
        self.requests += 1;
        let sequence = self.requests;
        // Persist intent before any effect. A missing receipt never proves no
        // write; failed mutation calls stop the run and are never replayed.
        self.record(json!({"event":"request","sequence":sequence,"stage":stage,"method":method,"route":route,"body":request}))?;
        let mut command = util::cmd(
            "gh",
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                method,
                route,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2026-03-10",
            ],
        );
        command.env("GH_TOKEN", &self.token).env("GH_PROMPT_DISABLED", "1");
        let input = if let Some(request) = request {
            command.args(["--input", "-"]);
            let mut input = subprocess::RegularFileStdinBuilder::new()?;
            input.write_all(&serde_json::to_vec(&request)?)?;
            Some(input.finish()?)
        } else {
            None
        };
        let remaining = self.remaining_request_time()?;
        let result = subprocess::local_output(&mut command, input, remaining, 4 * 1024 * 1024);
        let response = match result {
            Ok(output) if output.status.success() => {
                UniqueJson::decode(&output.stdout).ok().map(UniqueJson::into_value)
            }
            _ => None,
        };
        match response {
            Some(response) => {
                self.record(
                    json!({"event":"response","sequence":sequence,"stage":stage,"body":response}),
                )?;
                Ok(response)
            }
            None => {
                let outcome = if method == "GET" { "unavailable" } else { "indeterminate" };
                self.record(json!({"event":"failure","sequence":sequence,"stage":stage,"outcome":outcome,"automatic_retry":false}))?;
                bail!(
                    "Hosted {stage} outcome is {outcome}; inspect the journal and remote state before another run"
                );
            }
        }
    }
    pub(super) fn graphql(&mut self, stage: &str, query: &str, id: &str) -> Result<Value> {
        let response = self.call(
            stage,
            "POST",
            "graphql",
            Some(json!({"query":query,"variables":{"id":id}})),
        )?;
        if response
            .get("errors")
            .is_some_and(|errors| errors.as_array().is_none_or(|errors| !errors.is_empty()))
            || !response["data"].is_object()
        {
            bail!("Hosted lifecycle acknowledgement is indeterminate");
        }
        Ok(response)
    }
}
