//! Failures.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOperation {
    Var,
    InterpretTrailers,
    LsRemote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryableHttpStatus {
    TooManyRequests,
    ServiceUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectStatus {
    Temporary,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedJson {
    DuplicateObjectMember,
    TrailingValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationPushStage {
    Initial,
    Marker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    GraphQl,
    QueryTransport,
    QueryHttp(RetryableHttpStatus),
    CreatePr,
    CreatePrApplyThenDisconnect,
    CreatePrMismatchedContent,
    CreatePrMalformedJson(MalformedJson),
    CreatePrHttp(RetryableHttpStatus),
    CreatePrRedirect(RedirectStatus),
    ClosePr,
    ClosePrApplyThenDisconnect,
    UpdatePr,
    UpdatePrApplyThenDisconnect,
    UpdatePrMalformedJson(MalformedJson),
    UpdatePrConcurrentClose,
    LosePublicationPushReceipt(PublicationPushStage),
    Git(GitOperation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQlOperation {
    Query,
    CreatePr,
    DraftPr,
    ClosePr,
    UpdatePr,
}
