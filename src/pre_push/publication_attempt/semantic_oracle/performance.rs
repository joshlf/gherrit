use super::*;

#[tokio::test]
async fn performance_contract_fresh_stack_effects_and_quiescent_retry() {
    for depth in [1, 8, 32, 64] {
        let intent = many_bounded_ids_intent(depth, 8);
        let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
        let report = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
        assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
        assert!(report.trace.draft_conversions.is_empty());
        let tuples = flatten(&report.trace.initial_refs);
        let creates = flatten(&report.trace.creates);
        let markers = flatten(&report.trace.markers);
        let projections = flatten(&report.trace.projections);
        assert_eq!(tuples.len(), depth);
        assert!(tuples.iter().all(|effect| matches!(effect, InitialRefEffect::Tuple(_))));
        assert_eq!(creates.len(), depth);
        assert_eq!(markers.len(), depth);
        assert_eq!(projections.len(), depth);
        let body_bytes: Vec<_> = intent
            .iter()
            .map(|change| {
                world
                    .selected_open_pull_request(&change.id)
                    .unwrap()
                    .open_fields()
                    .unwrap()
                    .body
                    .len()
            })
            .collect();
        assert!(body_bytes.iter().all(|bytes| *bytes <= 131_072));
        let metrics = serde_json::json!({
            "workload": "fresh_stack", "depth": depth, "tuple_refs": depth * 3,
            "markers": markers.len(), "create_aliases": creates.len(),
            "projection_aliases": projections.len(),
            "initial_git_batches": report.trace.initial_refs.len(),
            "marker_git_batches": report.trace.markers.len(),
            "create_requests": report.trace.creates.len(),
            "projection_requests": report.trace.projections.len(),
            "body_bytes": body_bytes.iter().sum::<usize>(),
            "maximum_body_bytes": body_bytes.iter().max().unwrap(),
        });
        println!("GHERRIT_PERFORMANCE {metrics}");
        let retry = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
        assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
        assert!(retry.trace.is_empty(), "converged depth {depth} gained another effect");
    }
}
