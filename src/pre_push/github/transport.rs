//! Execute adaptive GraphQL requests without deciding stack policy.
//!
//! Only complete, decoded batches advance. Resource responses can reduce the
//! next attempt's size; transport failures and unusable acknowledgements stop
//! execution without claiming that a mutation had no external effect.

use color_eyre::eyre::{Context as _, Result, bail};
use octocrab::Octocrab;

use super::{BatchedOperation, batch_document, decode_batch_response};
use crate::pre_push::batching::{
    BatchPlan, INITIAL_GRAPHQL_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};

/// Executes batched GraphQL operations (queries or mutations).
///
/// Builds a combined query for each adaptive batch and decodes each operation
/// in a successful response.
pub(in crate::pre_push) async fn run_batched_graphql<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
) -> Result<Vec<O::Output>>
where
    O: BatchedOperation,
{
    let operations: Vec<O> = operations.into_iter().collect();
    if operations.is_empty() {
        return Ok(Vec::new());
    }

    let mut outputs = Vec::with_capacity(operations.len());

    // GraphQL resource use depends on both item count and document size. Start
    // with a fixed batch cap, then reduce the unaccepted range on the resource
    // responses recognized by classify_response. This changes batch size,
    // not timing; transport failures do not enter this retry path.
    //
    // [1] https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api#node-limit
    // [2] https://github.blog/changelog/2025-09-01-graphql-api-resource-limits/
    let mut batches = BatchPlan::new(operations.len(), INITIAL_GRAPHQL_BATCH_LEN);
    while let Some(attempt) = batches.current() {
        let chunk = &operations[attempt.range()];
        let query = batch_document(chunk);

        // Attempt to perform the query. Returns:
        // - Ok(Some(response)): Success
        // - Ok(None): Heuristic or API limit hit (needs backoff)
        // - Err(e): Fatal error (bail)
        let response = async {
            // HEURISTIC: Check query size before sending. GitHub's WAF/load
            // balancer/some other middleware seems to silently drop or truncate
            // requests larger than ~600KB, leading to confusing "missing query
            // attribute" errors. We preemptively backoff if we exceed a
            // conservative limit (256KB).
            if query_exceeds_limit(&query) {
                log::warn!(
                    "GraphQL query size ({} bytes) exceeds heuristic limit ({} bytes).",
                    query.len(),
                    MAX_GRAPHQL_QUERY_BYTES
                );
                return Ok(None);
            }

            log::trace!("Sending GraphQL Query (Length: {}): {}", query.len(), query);
            let request_payload = serde_json::json!({ "query": query });
            let response: serde_json::Value = octocrab
                .graphql(&request_payload)
                .await
                .wrap_err("GraphQL batched operation failed")?;

            match classify_response(&response) {
                ResponseDisposition::Success => {}
                ResponseDisposition::RetryLimit => {
                    log::warn!(
                        "Hit GitHub resource limit with GraphQL batch of size {}",
                        chunk.len()
                    );
                    return Ok(None);
                }
                ResponseDisposition::Fatal => {
                    let errors = response.get("errors").expect("fatal response has errors");
                    log::error!("GraphQL errors: {errors}");
                    bail!("GraphQL errors: {errors:?}");
                }
            }

            Ok(Some(response))
        }
        .await?;

        let Some(response) = response else {
            match attempt.reject() {
                Ok(backoff) => log::warn!(
                    "Backing off GraphQL batch size from {} to {}.",
                    backoff.attempted,
                    backoff.retry
                ),
                Err(item) => bail!(
                    "GraphQL operation at item {} exceeds GitHub resource limits. Cannot sync.",
                    item.index
                ),
            }
            continue;
        };

        // Advance only after every alias decodes. A malformed acknowledgement
        // still may follow an applied mutation; returning an error here does
        // not establish that GitHub left the remote state unchanged.
        outputs.extend(decode_batch_response(chunk, response)?);

        attempt.accept();
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::{super::UpdatePullRequest, *};

    #[test]
    fn batched_mutation_http_contracts_use_literal_requests_and_raw_responses() {
        use testutil::scripted_http::{Exchange, HttpRequest, ScriptedHttp};
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _entered = runtime.enter();
        for (status, response, success) in [
            (200, br#"{"data":{"op0":{"clientMutationId":null}}}"#.as_slice(), true),
            (200, b"not JSON".as_slice(), false),
            (200, br#"{"data":{}}"#.as_slice(), false),
            (
                400,
                br#"{"message":"Bad request","documentation_url":"https://example.invalid/docs"}"#
                    .as_slice(),
                false,
            ),
        ] {
            let server = ScriptedHttp::start([Exchange {
                request: HttpRequest { method: "POST".to_string(), path: "/graphql".to_string(),
                    body: br#"{"query":"mutation { op0: updatePullRequest(input: { pullRequestId: \"PR_1\" }) { clientMutationId } }"}"#.to_vec() },
                status, response: response.to_vec(),
            }]);
            let client = Octocrab::builder()
                .personal_token("fixture-token".to_string())
                .base_uri(&server.url)
                .unwrap()
                .build()
                .unwrap();
            let result = runtime.block_on(run_batched_graphql(
                &client,
                [UpdatePullRequest::new("PR_1".to_string(), None, None, None)],
            ));
            assert_eq!(result.is_ok(), success, "status={status}, result={result:?}");
            assert_eq!(
                server.arrivals().len(),
                1,
                "a malformed acknowledgement must not be replayed"
            );
        }
    }
}
