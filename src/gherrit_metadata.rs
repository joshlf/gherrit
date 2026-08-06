use eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};

use crate::gherrit_id;

const PREFIX: &str = "<!-- gherrit-meta: ";
const SUFFIX: &str = " -->";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GherritMetadata {
    pub id: String,
    pub parent: Option<String>,
    pub child: Option<String>,
}

impl GherritMetadata {
    pub fn validate(&self) -> Result<()> {
        gherrit_id::validate(&self.id)?;
        if let Some(parent) = &self.parent {
            gherrit_id::validate(parent)?;
        }
        if let Some(child) = &self.child {
            gherrit_id::validate(child)?;
        }
        if self.parent.as_deref() == Some(self.id.as_str()) {
            bail!("GHerrit metadata names its own ID as its parent");
        }
        if self.child.as_deref() == Some(self.id.as_str()) {
            bail!("GHerrit metadata names its own ID as its child");
        }
        Ok(())
    }
}

pub fn render(id: &str, parent: Option<&str>, child: Option<&str>) -> String {
    let metadata = GherritMetadata {
        id: id.to_string(),
        parent: parent.map(ToString::to_string),
        child: child.map(ToString::to_string),
    };
    metadata.validate().expect("generated GHerrit metadata is valid");
    let json = serde_json::to_string(&metadata).expect("serializing GHerrit metadata cannot fail");
    format!("{PREFIX}{json}{SUFFIX}")
}

/// Parses the authoritative metadata comment at the end of a GHerrit PR body.
///
/// Earlier lookalike comments are deliberately ignored: GHerrit has always
/// treated the terminal generated block as authoritative. A historical body
/// generator briefly emitted one extra quote after the JSON object, so that
/// exact legacy spelling remains readable for migration purposes.
pub fn parse_terminal(body: &str) -> Result<Option<GherritMetadata>> {
    let body = body.trim_end();
    let Some(start) = body.rfind(PREFIX) else {
        return Ok(None);
    };
    let candidate = &body[start..];
    if !candidate.ends_with(SUFFIX) {
        bail!("GHerrit metadata marker is not a complete terminal HTML comment");
    }
    if start + candidate.len() != body.len() {
        bail!("GHerrit metadata comment is not terminal");
    }

    let mut json = &candidate[PREFIX.len()..candidate.len() - SUFFIX.len()];
    let parsed = serde_json::from_str::<GherritMetadata>(json).or_else(|primary| {
        // Explicit compatibility with the old `...}\" -->` spelling.
        let Some(stripped) = json.strip_suffix('"') else {
            return Err(primary);
        };
        json = stripped;
        serde_json::from_str::<GherritMetadata>(json)
    });
    let metadata = parsed.map_err(|error| eyre!("Invalid terminal GHerrit metadata: {error}"))?;
    metadata.validate()?;
    Ok(Some(metadata))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "Gabcdefghijklmnopqrstuvwxyz234567";
    const PARENT: &str = "G234567abcdefghijklmnopqrstuvwxyz";

    #[test]
    fn round_trips_generated_metadata() {
        let rendered = render(ID, Some(PARENT), None);
        assert_eq!(
            parse_terminal(&rendered).unwrap(),
            Some(GherritMetadata {
                id: ID.to_string(),
                parent: Some(PARENT.to_string()),
                child: None,
            })
        );
    }

    #[test]
    fn uses_only_the_terminal_comment() {
        let body = format!("{PREFIX}not-json{SUFFIX}\ntext\n{}", render(ID, None, Some(PARENT)));
        assert_eq!(parse_terminal(&body).unwrap().unwrap().child.as_deref(), Some(PARENT));
    }

    #[test]
    fn accepts_the_explicit_legacy_extra_quote() {
        let legacy =
            format!("{PREFIX}{{\"id\":\"{ID}\",\"parent\":null,\"child\":null}}\"{SUFFIX}");
        assert_eq!(parse_terminal(&legacy).unwrap().unwrap().id, ID);
    }

    #[test]
    fn rejects_nonterminal_or_invalid_metadata() {
        assert!(parse_terminal(&format!("{}\ntext", render(ID, None, None))).is_err());
        assert!(parse_terminal("<!-- gherrit-meta: not-json -->").is_err());
        assert!(
            parse_terminal(
                "<!-- gherrit-meta: {\"id\":\"main\",\"parent\":null,\"child\":null} -->"
            )
            .is_err()
        );
    }
}
