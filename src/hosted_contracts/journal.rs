//! Journal.

use super::*;

pub(super) fn redact_journal(value: &mut Value, credential: &str) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if [
                    "temp_clone_token",
                    "token",
                    "access_token",
                    "refresh_token",
                    "id_token",
                    "client_secret",
                    "authorization",
                    "password",
                ]
                .iter()
                .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                {
                    *value = json!("<redacted>");
                } else {
                    redact_journal(value, credential);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_journal(value, credential);
            }
        }
        Value::String(value) if !credential.is_empty() => {
            *value = value.replace(credential, "<redacted>")
        }
        _ => {}
    }
}

impl Api {
    pub(super) fn record(&mut self, mut event: Value) -> Result<()> {
        redact_journal(&mut event, &self.token);
        serde_json::to_writer(&mut self.journal, &event)?;
        self.journal.write_all(b"\n")?;
        self.journal.flush()?;
        self.journal.sync_all()?;
        Ok(())
    }
    pub(super) fn finish_run(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = &result {
            // Receipt validation occurs after a response has been persisted.
            // Its rejection must survive stderr loss, without implying undo.
            self.record(json!({"event":"failure","stage":"run","after_sequence":self.requests,
                "outcome":"stopped","reason":error.to_string(),"automatic_retry":false}))?;
        }
        result
    }
}
