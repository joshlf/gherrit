//! Shared bounded HTTP-body and safe diagnostic primitives.
use std::fmt;

use http_body_util::{BodyExt, LengthLimitError, Limited};
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum GraphqlBodyError {
    ExceededLimit,
    Read(String),
}

impl fmt::Display for GraphqlBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceededLimit => write!(formatter, "GraphQL response exceeded its body limit"),
            Self::Read(error) => write!(formatter, "Failed to read GraphQL response body: {error}"),
        }
    }
}

impl std::error::Error for GraphqlBodyError {}

pub(crate) async fn collect_body_with_limit<B>(
    body: B,
    limit: usize,
) -> std::result::Result<http_body_util::Collected<B::Data>, GraphqlBodyError>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    Limited::new(body, limit).collect().await.map_err(|error| {
        if error.is::<LengthLimitError>() {
            GraphqlBodyError::ExceededLimit
        } else {
            GraphqlBodyError::Read(diagnostic_detail(&error.to_string()))
        }
    })
}

pub(crate) fn diagnostic_detail(value: &str) -> String {
    const MAX_BYTES: usize = 160;

    let mut rendered = String::new();
    for character in value.chars() {
        let escaped = character.escape_default().to_string();
        if rendered.len() + escaped.len() > MAX_BYTES {
            rendered.push('…');
            break;
        }
        rendered.push_str(&escaped);
    }
    rendered
}
