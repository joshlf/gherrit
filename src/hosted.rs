//! Factual GitHub reads. No value in this module permits a publication effect.
//! The publisher retains its consuming OPEN-only admissibility decoder.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Write as _,
    time::{Duration, Instant},
};

use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{identity::GherritPrId, json::UniqueJson, subprocess, util};

mod graphql;
use graphql::*;
mod native;
use native::*;
mod read_api;
use read_api::*;

const MAX_PAGES: usize = 32;
const MAX_ROWS: usize = 1000;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_OBSERVATION_ROWS: usize = 10_000;

/// Whole-observation bounds complement each response/connection's limit.
struct ReadBudget {
    remaining_bytes: usize,
    remaining_rows: usize,
}

impl Default for ReadBudget {
    fn default() -> Self {
        Self { remaining_bytes: MAX_OBSERVATION_BYTES, remaining_rows: MAX_OBSERVATION_ROWS }
    }
}

impl ReadBudget {
    fn response_limit(&self) -> Result<usize> {
        if self.remaining_bytes == 0 {
            bail!("Hosted observation exhausted its response byte budget");
        }
        Ok(self.remaining_bytes.min(MAX_RESPONSE_BYTES))
    }

    fn decode(&mut self, bytes: &[u8]) -> Result<Value> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes.len())
            .ok_or_else(|| eyre!("Hosted observation exceeds its response byte budget"))?;
        Ok(UniqueJson::decode(bytes).wrap_err("Malformed factual response")?.into_value())
    }

    fn retain_rows(&mut self, count: usize) -> Result<()> {
        self.remaining_rows = self
            .remaining_rows
            .checked_sub(count)
            .ok_or_else(|| eyre!("Hosted observation exceeds its aggregate row budget"))?;
        Ok(())
    }
}

pub(crate) fn observe(owner: &str, repository: &str, ids: &[GherritPrId]) -> Result<Value> {
    if ids.is_empty() {
        return Ok(
            json!({"state":"observed", "heads": [], "native_stacks": [], "publication_authority":false}),
        );
    }
    let mut api = ReadApi::new()?;
    observe_with(
        owner,
        repository,
        ids,
        &mut ReadBudget::default(),
        &mut |route, request, budget| api.call(route, request, budget),
    )
}

fn observe_with(
    owner: &str,
    repository: &str,
    ids: &[GherritPrId],
    budget: &mut ReadBudget,
    call: &mut impl FnMut(&str, Option<Value>, &mut ReadBudget) -> Result<Value>,
) -> Result<Value> {
    let mut heads = Vec::<HeadFacts>::new();
    let mut memberships = NativeStackIdentities::default();
    let mut identities = PullRequestIdentities::default();
    for id in ids {
        let head = read_head(
            owner,
            repository,
            id,
            &mut |request| call("graphql", Some(request), budget),
            &mut identities,
        )?;
        if heads.first().is_some_and(|first| first.repository != head.repository) {
            bail!("Repository identity changed between inspected heads");
        }
        budget.retain_rows(head.pull_requests.len())?;
        for pr in &head.pull_requests {
            if let Some(stack) = &pr.stack {
                memberships.register(stack)?;
            }
        }
        heads.push(head);
    }
    let mut stacks = Vec::new();
    for (&number, node_id) in &memberships.by_number {
        let stack = call(&format!("repos/{owner}/{repository}/stacks/{number}"), None, budget)?;
        validate_stack(&stack, number, node_id, &mut identities, &mut memberships.rest_ids)?;
        budget.retain_rows(
            stack["pull_requests"].as_array().expect("validated stack members").len(),
        )?;
        // Report the complete ordered native resource as provider facts.
        stacks.push(stack);
    }
    Ok(
        json!({"state":"observed", "heads":heads, "native_stacks":stacks, "publication_authority":false,
        "basis":"sequential read observations; not an atomic snapshot"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn validate_stack(value: &Value, number: u64, node: &str) -> Result<()> {
        super::validate_stack(
            value,
            number,
            node,
            &mut PullRequestIdentities::default(),
            &mut HashSet::new(),
        )
    }

    fn read_head(
        owner: &str,
        repo: &str,
        id: &GherritPrId,
        call: &mut impl FnMut(Value) -> Result<Value>,
    ) -> Result<HeadFacts> {
        super::read_head(owner, repo, id, call, &mut PullRequestIdentities::default())
    }
    #[test]
    fn identities_repeated_across_heads_make_observation_unavailable() {
        for duplicate_number in [false, true] {
            let mut identities = PullRequestIdentities::default();
            super::read_head(
                "owner",
                "repo",
                &id(),
                &mut |_| Ok(page(vec![row(7, "OPEN")], false, None)),
                &mut identities,
            )
            .unwrap();
            let other = GherritPrId::from_ref_component(b"Gtwo").unwrap();
            let mut renamed = row(if duplicate_number { 7 } else { 8 }, "OPEN");
            renamed["headRefName"] = json!("Gtwo");
            if !duplicate_number {
                renamed["id"] = json!("PR7");
            }
            assert!(
                super::read_head(
                    "owner",
                    "repo",
                    &other,
                    &mut |_| Ok(page(vec![renamed.clone()], false, None)),
                    &mut identities
                )
                .is_err()
            );
        }
    }
    fn id() -> GherritPrId {
        GherritPrId::from_ref_component(b"Gone").unwrap()
    }
    fn row(number: u64, state: &str) -> Value {
        json!({"number":number,"id":format!("PR{number}"),"state":state,"isDraft":false,
        "headRefName":"Gone","headRefOid":"1111111111111111111111111111111111111111","baseRefName":"main","baseRefOid":"2222222222222222222222222222222222222222",
        "headRepository":{"id":"R","nameWithOwner":"owner/repo"},"baseRepository":{"id":"R","nameWithOwner":"owner/repo"},"stack":null,"stackEntry":null})
    }
    fn page(rows: Vec<Value>, more: bool, cursor: Option<&str>) -> Value {
        json!({"data":{"repository":{"id":"R","nameWithOwner":"owner/repo",
        "pullRequests":{"nodes":rows,"pageInfo":{"hasNextPage":more,"endCursor":cursor}}}}})
    }
    #[test]
    fn terminal_facts_are_complete_only_after_exhaustion() {
        let mut requests = Vec::new();
        let mut responses = [
            page(vec![row(7, "CLOSED")], true, Some("opaque")),
            page(vec![row(8, "MERGED"), row(9, "OPEN")], false, None),
        ]
        .into_iter();
        let facts = read_head("owner", "repo", &id(), &mut |request| {
            requests.push(request);
            Ok(responses.next().unwrap())
        })
        .unwrap();
        assert_eq!(facts.pages, 2);
        assert_eq!(facts.pull_requests.iter().map(|pr| pr.number).collect::<Vec<_>>(), [7, 8, 9]);
        assert_eq!(requests[0]["variables"]["after"], Value::Null);
        assert_eq!(requests[1]["variables"]["after"], "opaque");
        assert!(!serde_json::to_string(&facts).unwrap().contains("authority"));
    }
    #[test]
    fn malformed_scope_pagination_and_unknown_membership_never_become_absence() {
        for field in ["number", "id", "headRefName", "baseRepository", "headRefOid", "stackEntry"] {
            let mut bad = row(7, "CLOSED");
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(
                    vec![bad.clone()],
                    false,
                    None
                )))
                .is_err(),
                "{field}"
            );
        }
        let mut missing = row(7, "OPEN");
        missing.as_object_mut().unwrap().remove("stack");
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(
                vec![missing.clone()],
                false,
                None
            )))
            .is_err()
        );
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![], true, Some("repeat"))))
                .is_err()
        );
        assert!(read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![], true, None))).is_err());
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(
                vec![row(7, "OPEN"), row(7, "OPEN")],
                false,
                None
            )))
            .is_err()
        );
        assert!(read_head("owner", "repo", &id(), &mut |_| Ok(json!({"data":null}))).is_err());
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(json!({"data":{},"errors":[{}]})))
                .is_err()
        );
        assert!(read_head("owner", "repo", &id(), &mut |_| Err(eyre!("unavailable"))).is_err());
    }

    #[test]
    fn valid_but_wrong_scope_or_lost_second_page_has_no_complete_result() {
        for wrong in ["head", "base"] {
            let mut value = row(7, "OPEN");
            if wrong == "head" {
                value["headRefName"] = json!("Gother");
            } else {
                value["baseRepository"]["id"] = json!("other repository");
            }
            assert!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(
                    vec![value.clone()],
                    false,
                    None
                )))
                .is_err()
            );
        }
        let mut count = 0;
        assert!(
            read_head("owner", "repo", &id(), &mut |_| {
                count += 1;
                if count == 1 {
                    Ok(page(vec![row(7, "CLOSED")], true, Some("next")))
                } else {
                    Err(eyre!("lost second page"))
                }
            })
            .is_err()
        );
        assert_eq!(count, 2);
    }
    #[test]
    fn explicit_empty_connection_and_deleted_or_fork_head_are_facts() {
        let facts =
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![], false, None))).unwrap();
        assert!(facts.pull_requests.is_empty());
        for head in [Value::Null, json!({"id":"fork","nameWithOwner":"other/repo"})] {
            let mut value = row(7, "CLOSED");
            value["headRepository"] = head;
            assert_eq!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(
                    vec![value.clone()],
                    false,
                    None
                )))
                .unwrap()
                .pull_requests
                .len(),
                1
            );
        }
    }
    fn native_member(number: u64) -> Value {
        let revision = |name: &str| {
            json!({"ref":name,"sha":"1111111111111111111111111111111111111111",
            "repo":{"id":1,"url":"https://api.github.com/repos/owner/repo","name":"repo"}})
        };
        json!({"id":number+100,"number":number,"node_id":format!("PR{number}"),"title":"Title",
            "state":"open","draft":false,"merged_at":null,"user":null,
            "url":format!("https://api.github.com/repos/owner/repo/pulls/{number}"),
            "html_url":format!("https://github.com/owner/repo/pull/{number}"),
            "head":revision(&format!("G{number}")),"base":revision("main")})
    }

    fn native_stack() -> Value {
        json!({"id":13,"number":3,"node_id":"S","url":"https://api.github.com/repos/owner/repo/stacks/3",
            "created_at":"2026-10-01T12:00:00Z","base":{"ref":"main"},"open":true,
            "pull_requests":[native_member(9),native_member(7)],"provider_extra":{"retained":true}})
    }

    #[test]
    fn native_order_and_resource_identity_are_independent_checks() {
        let mut stack = native_stack();
        validate_stack(&stack, 3, "S").unwrap();
        assert_eq!(stack["pull_requests"][0]["number"], 9);
        assert_eq!(stack["provider_extra"]["retained"], true);
        assert!(validate_stack(&stack, 4, "S").is_err());
        assert!(validate_stack(&stack, 3, "other").is_err());
        for key in ["id", "number", "node_id"] {
            let mut repeated = stack.clone();
            repeated["pull_requests"][1][key] = repeated["pull_requests"][0][key].clone();
            assert!(validate_stack(&repeated, 3, "S").is_err(), "accepted duplicate {key}");
        }
        stack["pull_requests"] = json!([native_member(7), native_member(7)]);
        assert!(validate_stack(&stack, 3, "S").is_err());
    }

    #[test]
    fn native_required_fields_are_not_complete_when_absent_or_malformed() {
        let valid = native_stack();
        for pointer in [
            "",
            "/base",
            "/pull_requests/0",
            "/pull_requests/0/head",
            "/pull_requests/0/base",
            "/pull_requests/0/head/repo",
            "/pull_requests/0/base/repo",
        ] {
            for key in valid.pointer(pointer).unwrap().as_object().unwrap().keys() {
                if key == "provider_extra" {
                    continue;
                }
                let mut incomplete = valid.clone();
                incomplete.pointer_mut(pointer).unwrap().as_object_mut().unwrap().remove(key);
                assert!(
                    validate_stack(&incomplete, 3, "S").is_err(),
                    "accepted absent {pointer}/{key}"
                );
            }
        }
        for (pointer, value) in [
            ("/created_at", json!("not a timestamp")),
            ("/url", json!("relative")),
            ("/pull_requests/0/url", Value::Null),
            ("/pull_requests/0/draft", json!("false")),
            ("/pull_requests/0/head/sha", json!("no oid")),
            ("/pull_requests/0/base/repo", Value::Null),
            ("/pull_requests/0/merged_at", json!("yesterday")),
            ("/pull_requests/0/user", json!({})),
        ] {
            let mut malformed = valid.clone();
            *malformed.pointer_mut(pointer).unwrap() = value;
            assert!(validate_stack(&malformed, 3, "S").is_err(), "accepted malformed {pointer}");
        }
    }

    #[test]
    fn native_nullable_user_requires_complete_fields_when_present() {
        let mut user = json!({"id":1,"login":"reviewer","node_id":"U1","type":"User",
            "site_admin":false,"gravatar_id":null,"name":null,"email":null});
        for key in [
            "avatar_url",
            "url",
            "html_url",
            "followers_url",
            "following_url",
            "gists_url",
            "starred_url",
            "subscriptions_url",
            "organizations_url",
            "repos_url",
            "events_url",
            "received_events_url",
        ] {
            user[key] = json!(format!("https://api.github.com/users/reviewer/{key}"));
        }
        let mut stack = native_stack();
        stack["pull_requests"][0]["user"] = user.clone();
        validate_stack(&stack, 3, "S").unwrap();
        for key in user.as_object().unwrap().keys() {
            if ["name", "email"].contains(&key.as_str()) {
                continue;
            }
            let mut incomplete = stack.clone();
            incomplete["pull_requests"][0]["user"].as_object_mut().unwrap().remove(key);
            assert!(validate_stack(&incomplete, 3, "S").is_err(), "accepted missing user {key}");
        }
    }
    fn factual_response(request: &Value, number: u64, stack: Option<(u64, &str)>) -> Value {
        let mut value = row(number, "OPEN");
        value["headRefName"] = request["variables"]["head"].clone();
        if let Some((number, node)) = stack {
            value["stack"] = json!({"id":node,"number":number,"size":2,"baseRefName":"main"});
            value["stackEntry"] = json!({"position":1,"stack":{"id":node}});
        }
        page(vec![value], false, None)
    }

    #[test]
    fn aggregate_rows_span_heads_and_allow_the_exact_limit() {
        let ids = [
            id(),
            GherritPrId::from_ref_component(b"Gtwo").unwrap(),
            GherritPrId::from_ref_component(b"Gthree").unwrap(),
        ];
        for head_count in [2, 3] {
            let mut budget = ReadBudget { remaining_rows: 2, ..ReadBudget::default() };
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &ids[..head_count],
                &mut budget,
                &mut |route, request, budget| {
                    assert_eq!(route, "graphql");
                    calls += 1;
                    budget.decode(
                        &serde_json::to_vec(&factual_response(&request.unwrap(), calls, None))
                            .unwrap(),
                    )
                },
            );
            assert_eq!(result.is_ok(), head_count == 2);
            assert_eq!(budget.remaining_rows, 0);
            assert_eq!(calls, head_count as u64);
        }
    }

    #[test]
    fn native_resources_share_the_row_and_raw_byte_budgets() {
        let head = factual_response(&json!({"variables":{"head":"Gone"}}), 7, Some((3, "S")));
        let total = serde_json::to_vec(&head).unwrap().len()
            + serde_json::to_vec(&native_stack()).unwrap().len();
        for (rows, bytes, success) in [(3, total, true), (2, total, false), (3, total - 1, false)] {
            let mut budget = ReadBudget { remaining_rows: rows, remaining_bytes: bytes };
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &[id()],
                &mut budget,
                &mut |route, request, budget| {
                    calls += 1;
                    let value = if route == "graphql" {
                        factual_response(&request.unwrap(), 7, Some((3, "S")))
                    } else {
                        assert_eq!(route, "repos/owner/repo/stacks/3");
                        native_stack()
                    };
                    budget.decode(&serde_json::to_vec(&value).unwrap())
                },
            );
            assert_eq!(result.is_ok(), success);
            assert_eq!(calls, 2);
            if !success {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(if rows == 2 { "row budget" } else { "byte budget" }));
            }
        }
    }

    #[test]
    fn raw_response_budget_is_checked_before_decode_and_caps_the_child() {
        let mut budget = ReadBudget { remaining_bytes: 2, ..ReadBudget::default() };
        assert_eq!(budget.response_limit().unwrap(), 2);
        assert_eq!(budget.decode(b"{}").unwrap(), json!({}));
        assert!(budget.response_limit().is_err());
        assert!(budget.decode(b" ").unwrap_err().to_string().contains("byte budget"));
        let mut budget = ReadBudget::default();
        assert_eq!(budget.response_limit().unwrap(), MAX_RESPONSE_BYTES);
        assert!(budget.decode(b"{}").is_ok());
        assert_eq!(budget.remaining_bytes, MAX_OBSERVATION_BYTES - 2);
    }

    #[test]
    fn native_stack_identities_are_one_to_one_across_heads() {
        let ids = [id(), GherritPrId::from_ref_component(b"Gtwo").unwrap()];
        for memberships in [[(3, "S"), (4, "S")], [(3, "S"), (3, "T")]] {
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &ids,
                &mut ReadBudget::default(),
                &mut |route, request, budget| {
                    assert_eq!(route, "graphql", "contradictory identity must stop before REST");
                    let membership = memberships[calls];
                    calls += 1;
                    budget.decode(
                        &serde_json::to_vec(&factual_response(
                            &request.unwrap(),
                            calls as u64,
                            Some(membership),
                        ))
                        .unwrap(),
                    )
                },
            );
            assert!(result.is_err());
            assert_eq!(calls, 2);
        }
        let mut identities = NativeStackIdentities::default();
        let stack = NativeSummary {
            id: "S".to_owned(),
            number: 3,
            size: 2,
            base_ref_name: "main".to_owned(),
        };
        identities.register(&stack).unwrap();
        identities.register(&stack).unwrap();
        assert_eq!(identities.by_number.len(), 1);
    }
    #[test]
    fn member_identity_is_unique_across_native_resources_and_agrees_with_graphql() {
        let first = native_stack();
        for key in ["number", "node_id", "id"] {
            let mut identities = PullRequestIdentities::default();
            super::validate_stack(&first, 3, "S", &mut identities, &mut HashSet::new()).unwrap();
            let mut second = first.clone();
            second["number"] = json!(4);
            second["node_id"] = json!("S4");
            second["pull_requests"] = json!([native_member(42)]);
            second["pull_requests"][0][key] = first["pull_requests"][0][key].clone();
            assert!(
                super::validate_stack(&second, 4, "S4", &mut identities, &mut HashSet::new())
                    .is_err(),
                "{key}"
            );
        }
        let member = &first["pull_requests"][0];
        let mut identities = PullRequestIdentities::default();
        identities.observe_head(9, member["node_id"].as_str().unwrap()).unwrap();
        super::validate_stack(&first, 3, "S", &mut identities, &mut HashSet::new()).unwrap();
        for (number, node) in [(9, "different"), (42, member["node_id"].as_str().unwrap())] {
            let mut identities = PullRequestIdentities::default();
            identities.observe_head(number, node).unwrap();
            assert!(
                super::validate_stack(&first, 3, "S", &mut identities, &mut HashSet::new())
                    .is_err()
            );
        }
    }
    #[test]
    fn a_native_membership_summary_needs_a_nonempty_base() {
        let mut pr = row(7, "OPEN");
        pr["stack"] = json!({"id":"S","number":3,"size":1,"baseRefName":""});
        pr["stackEntry"] = json!({"position":1,"stack":{"id":"S"}});
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![pr.clone()], false, None)))
                .is_err()
        );
        pr["stack"]["baseRefName"] = json!("main");
        read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![pr.clone()], false, None)))
            .unwrap();
    }
    #[test]
    fn empty_pull_request_bases_are_rejected() {
        let mut pr = row(7, "OPEN");
        pr["baseRefName"] = json!("");
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![pr.clone()], false, None)))
                .is_err()
        );
        pr["baseRefName"] = json!("main");
        read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![pr.clone()], false, None)))
            .unwrap();
    }

    #[test]
    fn stack_rest_identity_is_unique_across_resources_in_its_own_namespace() {
        let first = native_stack();
        let mut second = first.clone();
        second["number"] = json!(4);
        second["node_id"] = json!("S4");
        second["pull_requests"] = json!([native_member(42)]);
        for (second_id, accepted) in [(13, false), (14, true), (109, true)] {
            let mut members = PullRequestIdentities::default();
            let mut stacks = HashSet::new();
            super::validate_stack(&first, 3, "S", &mut members, &mut stacks).unwrap();
            second["id"] = json!(second_id);
            assert_eq!(
                super::validate_stack(&second, 4, "S4", &mut members, &mut stacks).is_ok(),
                accepted
            );
        }
    }
    #[test]
    fn a_present_head_repository_requires_both_identity_fields() {
        let mut pr = row(7, "OPEN");
        for repository in [
            json!({"id":"", "nameWithOwner":"fork/repo"}),
            json!({"id":"FORK", "nameWithOwner":""}),
        ] {
            pr["headRepository"] = repository;
            assert!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![pr.clone()], false, None)))
                    .is_err()
            );
        }
        for repository in [Value::Null, json!({"id":"FORK", "nameWithOwner":"fork/repo"})] {
            pr["headRepository"] = repository;
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![pr.clone()], false, None)))
                .unwrap();
        }
    }

    #[test]
    fn repository_identity_is_shared_across_all_inspected_heads() {
        let ids = [id(), GherritPrId::from_ref_component(b"Gtwo").unwrap()];
        for changed in [false, true] {
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &ids,
                &mut ReadBudget::default(),
                &mut |route, _, _| {
                    assert_eq!(route, "graphql");
                    calls += 1;
                    let mut value = page(Vec::new(), false, None);
                    if changed && calls == 2 {
                        value["data"]["repository"]["id"] = json!("REPLACEMENT");
                    }
                    Ok(value)
                },
            );
            assert_eq!(calls, 2);
            assert_eq!(result.is_ok(), !changed);
            if changed {
                assert!(result.unwrap_err().to_string().contains("Repository identity changed"));
            }
        }
    }
    #[test]
    fn factual_object_ids_must_name_a_nonnull_object() {
        for width in [40, 64] {
            assert!(!oid(&"0".repeat(width)));
            assert!(oid(&format!("{}1", "0".repeat(width - 1))));
        }
    }
}
