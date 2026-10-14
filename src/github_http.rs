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

/// Retry-After permits seconds or HTTP dates; reset uses UTC epoch seconds.
/// Keep parsing independent of the HTTP implementation used by each reader.
pub(crate) fn query_retry_delay(
    status: u16,
    retry_after: Option<&[u8]>,
    remaining: Option<&[u8]>,
    reset: Option<&[u8]>,
    now: std::time::SystemTime,
) -> eyre::Result<Option<std::time::Duration>> {
    use std::time::{Duration, UNIX_EPOCH};
    let seconds = |value: &[u8]| -> eyre::Result<u64> { Ok(std::str::from_utf8(value)?.parse()?) };
    if let Some(value) = retry_after {
        let value = std::str::from_utf8(value)?.trim_matches([' ', '\t']);
        let delay = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            Duration::from_secs(value.parse()?)
        } else {
            httpdate::parse_http_date(value)?.duration_since(now).unwrap_or(Duration::ZERO)
        };
        return Ok(Some(delay));
    }
    if remaining == Some(b"0") {
        let reset = reset.ok_or_else(|| eyre::eyre!("GitHub rate limit omitted its reset time"))?;
        let until = UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds(reset)?))
            .ok_or_else(|| eyre::eyre!("GitHub rate-limit reset is out of range"))?;
        return Ok(Some(until.duration_since(now).unwrap_or(Duration::ZERO)));
    }
    Ok((status == 429).then_some(Duration::from_secs(60)))
}

#[cfg(test)]
mod retry_tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    #[test]
    fn rate_limit_headers_bound_the_next_query() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(
            query_retry_delay(429, Some(b"2"), Some(b"0"), Some(b"999"), now).unwrap(),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            query_retry_delay(403, None, Some(b"0"), Some(b"103"), now).unwrap(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            query_retry_delay(429, None, None, None, now).unwrap(),
            Some(Duration::from_secs(60))
        );
        assert_eq!(query_retry_delay(503, None, None, None, now).unwrap(), None);
        assert!(query_retry_delay(429, Some(b"invalid"), None, None, now).is_err());
    }
    #[test]
    fn retry_after_http_dates_use_their_absolute_deadline() {
        let now = UNIX_EPOCH + Duration::from_secs(784111775);
        for date in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(
                query_retry_delay(503, Some(date.as_bytes()), None, None, now).unwrap(),
                Some(Duration::from_secs(2))
            );
        }
        let past = UNIX_EPOCH + Duration::from_secs(784111780);
        assert_eq!(
            query_retry_delay(503, Some(b"Sun, 06 Nov 1994 08:49:37 GMT"), None, None, past)
                .unwrap(),
            Some(Duration::ZERO)
        );
    }
}
