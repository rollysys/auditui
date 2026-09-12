//! Cache reuse changes are observations, not proof of a cache failure or savings.

use std::collections::{BTreeMap, HashMap};

use super::analysis::{finish, in_window, selected, source_key};
use super::{Candidate, CostTotals, Ledger, LlmRequest, Query, SourceRef, UsageObservation};

const MIN_RATE_DROP: f64 = 0.30;
const MIN_AFFECTED_TOKENS: u64 = 4096;

#[derive(Clone, Copy)]
struct InputCounters {
    total: u64,
    read: u64,
    write: u64,
    non_reused: u64,
}

impl InputCounters {
    fn observed(observation: &UsageObservation) -> Option<Self> {
        if !observation.cache_counters_complete || observation.basis != "request" {
            return None;
        }
        let usage = &observation.usage;
        // Follow the ledger's disjoint write-bucket convention, not legacy plus split.
        let write = if usage.cache_creation_5m_tokens == 0 && usage.cache_creation_1h_tokens == 0 {
            usage.cache_creation_tokens
        } else {
            usage
                .cache_creation_5m_tokens
                .checked_add(usage.cache_creation_1h_tokens)?
        };
        let non_reused = usage.input_tokens.checked_add(write)?;
        let total = non_reused.checked_add(usage.cache_read_tokens)?;
        (total > 0).then_some(Self {
            total,
            read: usage.cache_read_tokens,
            write,
            non_reused,
        })
    }

    fn hit_rate(self) -> f64 {
        self.read as f64 / self.total as f64
    }
}

fn same_snapshot(a: &SourceRef, b: &SourceRef) -> bool {
    a.source_id == b.source_id && a.version == b.version
}

fn comparable_cost(a: &UsageObservation, b: &UsageObservation) -> Option<(&'static str, f64, f64)> {
    match (
        super::valid_amount(a.reported_usd),
        super::valid_amount(b.reported_usd),
    ) {
        (Some(first), Some(second)) => Some(("reported", first, second)),
        (None, None) if a.pricing_version.is_some() && a.pricing_version == b.pricing_version => {
            Some((
                "estimated",
                super::valid_amount(a.estimated_usd)?,
                super::valid_amount(b.estimated_usd)?,
            ))
        }
        _ => None,
    }
}

/// Compare adjacent requests, retaining missing/ambiguous usage as sequence barriers.
pub(super) fn detect(ledger: &Ledger, query: &Query) -> Vec<Candidate> {
    let mut streams = BTreeMap::<&str, Vec<(&SourceRef, Option<&LlmRequest>)>>::new();
    let mut request_counts = HashMap::<(&str, &str), usize>::new();
    for request in &ledger.requests {
        *request_counts
            .entry((&request.session_id, &request.id))
            .or_default() += 1;
        streams
            .entry(&request.session_id)
            .or_default()
            .push((&request.source, Some(request)));
    }
    let mut observations = HashMap::<(&str, &str), Option<&UsageObservation>>::new();
    for observation in &ledger.observations {
        if let Some(request_id) = observation
            .request_id
            .as_deref()
            .filter(|id| !id.is_empty())
        {
            let key = (observation.session_id.as_str(), request_id);
            if request_counts.contains_key(&key) {
                observations
                    .entry(key)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(observation));
                continue;
            }
        }
        // Unattributed/cumulative observations may conceal an intervening request.
        streams
            .entry(&observation.session_id)
            .or_default()
            .push((&observation.source, None));
    }
    let mut contexts = HashMap::<&str, Vec<_>>::new();
    for event in &ledger.contexts {
        contexts.entry(&event.session_id).or_default().push(event);
    }
    let mut session_counts = HashMap::<&str, usize>::new();
    for session in &ledger.sessions {
        *session_counts.entry(&session.id).or_default() += 1;
    }
    let mut found = Vec::new();
    for session in ledger
        .sessions
        .iter()
        .filter(|session| selected(session, query))
    {
        // An ID shared by different session/provider records cannot establish identity.
        if session_counts[session.id.as_str()] != 1 {
            continue;
        }
        let Some(stream) = streams.get_mut(session.id.as_str()) else {
            continue;
        };
        stream.sort_by_key(|(source, _)| source_key(source));
        for (index, pair) in stream.windows(2).enumerate() {
            // Multiple entries on one source line have no serial request order.
            if (index > 0
                && same_snapshot(stream[index - 1].0, pair[0].0)
                && stream[index - 1].0.line == pair[0].0.line)
                || stream.get(index + 2).is_some_and(|entry| {
                    same_snapshot(entry.0, pair[1].0) && entry.0.line == pair[1].0.line
                })
            {
                continue;
            }
            let (Some(first_request), Some(last_request)) = (pair[0].1, pair[1].1) else {
                continue;
            };
            if first_request.id.is_empty()
                || last_request.id.is_empty()
                || first_request.model.is_empty()
                || first_request.model != last_request.model
                || !same_snapshot(&first_request.source, &last_request.source)
                || first_request.source.line >= last_request.source.line
                || !in_window(first_request.ts_ms, query)
                || !in_window(last_request.ts_ms, query)
            {
                continue;
            }
            let first_key = (session.id.as_str(), first_request.id.as_str());
            let last_key = (session.id.as_str(), last_request.id.as_str());
            if request_counts[&first_key] != 1 || request_counts[&last_key] != 1 {
                continue;
            }
            let (Some(Some(first)), Some(Some(last))) =
                (observations.get(&first_key), observations.get(&last_key))
            else {
                continue;
            };
            if first.model != first_request.model
                || last.model != last_request.model
                || !same_snapshot(&first.source, &first_request.source)
                || !same_snapshot(&last.source, &last_request.source)
                || first.source.line >= last.source.line
                || !in_window(first.ts_ms, query)
                || !in_window(last.ts_ms, query)
            {
                continue;
            }
            let (Some(a), Some(b)) = (
                InputCounters::observed(first),
                InputCounters::observed(last),
            ) else {
                continue;
            };
            if a.hit_rate() - b.hit_rate() + f64::EPSILON < MIN_RATE_DROP
                || a.read.saturating_sub(b.read) < MIN_AFFECTED_TOKENS
                || b.non_reused.saturating_sub(a.non_reused) < MIN_AFFECTED_TOKENS
            {
                continue;
            }
            let money = comparable_cost(first, last);
            // Large shrinkage is a different context regime. Even smaller shrinkage
            // with falling comparable spend is not an adverse reuse candidate.
            if u128::from(b.total) * 100 < u128::from(a.total) * 80
                || (b.total < a.total && money.is_some_and(|(_, before, after)| after < before))
            {
                continue;
            }
            let between: Vec<_> = contexts
                .get(session.id.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|event| {
                    same_snapshot(&event.source, &first_request.source)
                        && event.source.line > first_request.source.line
                        && event.source.line < last_request.source.line
                })
                .collect();
            // Explicit switches remain a barrier even if the endpoints return to one model.
            if between.iter().any(|event| event.kind == "model_change") {
                continue;
            }
            let stable_size = u128::from(b.total) * 100 <= u128::from(a.total) * 120;
            let rebuild = b.write.saturating_sub(a.write) >= MIN_AFFECTED_TOKENS;
            let pattern = match (stable_size, rebuild) {
                (true, true) => {
                    "Stable-size cache-reuse loss with increased cache-write accounting"
                }
                (true, false) => {
                    "Stable-size cache-reuse loss with increased uncached-input accounting"
                }
                (false, true) => {
                    "Growing-context cache-reuse drop with increased cache-write accounting"
                }
                (false, false) => {
                    "Growing-context cache-reuse drop with increased uncached-input accounting"
                }
            };
            let mut observed = vec![
                format!("Adjacent same-session/provider/model request cache-read hit rate: {:.2}% to {:.2}% ({:.2} percentage-point drop). Denominator is uncached input + cache-read + cache-write.", a.hit_rate() * 100.0, b.hit_rate() * 100.0, (a.hit_rate() - b.hit_rate()) * 100.0),
                format!("Input/cache tokens: {} to {}; cache-read: {} to {}; uncached input: {} to {}; cache-write: {} to {}. Absolute cache-read decrease: {} tokens; non-reused input increase: {} tokens.", a.total, b.total, a.read, b.read, first.usage.input_tokens, last.usage.input_tokens, a.write, b.write, a.read - b.read, b.non_reused - a.non_reused),
                format!("{pattern}; stable size means within 20% of the earlier input/cache total."),
            ];
            match money {
                Some((basis, before, after)) => observed.push(format!("Comparable {basis} whole-request cost: ${before:.8} to ${after:.8}; change ${:+.8}. This includes output and other billed work, not cache-attributable cost.", after - before)),
                None => observed.push("Request cost change is unknown or not comparable: amounts are missing, monetary provenance differs, or estimated pricing versions are not the same known version.".into()),
            }
            observed.push(format!("Output tokens: {} to {}; output variation can change whole-request cost independently of cache reuse.", first.usage.output_tokens, last.usage.output_tokens));
            match first_request.ts_ms.zip(last_request.ts_ms).and_then(|(a, b)| b.checked_sub(a)).filter(|gap| *gap >= 0) {
                Some(gap) => observed.push(format!("Recorded request timestamp gap: {gap} ms; this is not request latency or evidence of cache expiry.")),
                None => observed.push("Recorded request timestamp gap is unavailable or non-monotonic; no latency is inferred.".into()),
            }
            let mut evidence = vec![
                &session.source,
                &first_request.source,
                &last_request.source,
                &first.source,
                &last.source,
            ];
            let mut annotations = BTreeMap::<&str, usize>::new();
            for event in between {
                let kind = match event.kind.as_str() {
                    "compaction" => "compaction",
                    _ => "other recorded context event",
                };
                *annotations.entry(kind).or_default() += 1;
                evidence.push(&event.source);
            }
            if annotations.is_empty() {
                observed.push("No recorded context event lies between the pair; this does not establish an unchanged prefix or identify a cause.".into());
            } else {
                observed.extend(annotations.into_iter().map(|(kind, count)| format!("Between-request context annotation: {count} {kind} event(s); temporal association does not establish a cause.")));
            }
            let mut related_cost = CostTotals {
                requests: 2,
                ..CostTotals::default()
            };
            related_cost.add_observation(first);
            related_cost.add_observation(last);
            let mut limitations = vec![
                "Thresholds: at least 30 percentage points of hit-rate decline, 4096 fewer cache-read tokens, and 4096 more uncached-input/cache-write tokens. These are review thresholds, not a provider cache guarantee.".into(),
                "Only adjacent unambiguous request-linked measurements with explicit valid cache counters are compared. Unknown zeros, cumulative usage, model transitions and substantial context shrinkage are excluded. Input/cache totals are not exact context-window occupancy.".into(),
                "Stable-size changes are consistent with prefix loss or rebuild, but content identity, cache keys, provider retention rules and request execution latency are not observed. No cause or required remediation is established.".into(),
                "Related cost covers both entire explicitly linked requests, including output and other work. It is not marginal cache cost, avoidable cost, or savings; requests can overlap other candidates, so candidate costs must not be summed.".into(),
            ];
            if related_cost.unknown_observations > 0 {
                limitations.push(format!("{} linked observations have unknown monetary cost; zero displayed monetary buckets do not establish free work.", related_cost.unknown_observations));
            }
            found.push(finish(query, Candidate {
                kind: "cache_reuse_discontinuity".into(),
                project: session.project.clone(),
                summary: format!("{pattern} across two adjacent requests."),
                observed,
                hypothesis: "Review request-prefix construction and recorded context events against provider cache behavior. A reuse discontinuity alone cannot prove cache expiry, an invalidated prefix, or removable spend.".into(),
                related_cost,
                limitations,
                ..Candidate::default()
            }, evidence));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::Usage;
    use crate::ledger::{ContextEvent, Session};
    use crate::providers::Agent;

    fn source(line: u64) -> SourceRef {
        SourceRef {
            source_id: "source".into(),
            version: "snapshot-a".into(),
            line,
            ..SourceRef::default()
        }
    }

    fn ledger() -> Ledger {
        let mut ledger = Ledger::default();
        ledger.sessions.push(Session {
            id: "session".into(),
            provider: Agent::Omp,
            project: "/project".into(),
            parent_id: None,
            task_id: None,
            work_type: None,
            source: source(1),
        });
        request(&mut ledger, 10, 2000, 18000, 0, Some(0.02));
        request(&mut ledger, 20, 2000, 2000, 16000, Some(0.12));
        ledger
    }

    fn request(
        ledger: &mut Ledger,
        line: u64,
        input: u64,
        read: u64,
        write: u64,
        cost: Option<f64>,
    ) {
        let id = format!("request-{line}");
        ledger.requests.push(LlmRequest {
            id: id.clone(),
            session_id: "session".into(),
            model: "model".into(),
            ts_ms: Some(line as i64 * 100),
            status: "complete".into(),
            source: source(line),
        });
        ledger.observations.push(UsageObservation {
            id: format!("usage-{line}"),
            session_id: "session".into(),
            request_id: Some(id),
            model: "model".into(),
            basis: "request".into(),
            cache_counters_complete: true,
            ts_ms: Some(line as i64 * 100),
            reported_usd: cost,
            source: source(line),
            usage: Usage {
                input_tokens: input,
                cache_read_tokens: read,
                cache_creation_tokens: write,
                output_tokens: 100,
                ..Usage::default()
            },
            ..UsageObservation::default()
        });
    }

    #[test]
    fn stable_prefix_loss_links_measured_counters_money_and_context_without_cause() {
        let mut ledger = ledger();
        ledger.contexts.push(ContextEvent {
            session_id: "session".into(),
            kind: "compaction".into(),
            ts_ms: Some(1500),
            source: source(15),
        });
        let found = detect(&ledger, &Query::default());
        assert_eq!(found.len(), 1);
        let candidate = &found[0];
        assert_eq!(candidate.kind, "cache_reuse_discontinuity");
        assert_eq!(candidate.related_cost.requests, 2);
        assert_eq!(candidate.related_cost.observations, 2);
        assert!((candidate.related_cost.reported_usd - 0.14).abs() < 1e-12);
        assert_eq!(
            candidate
                .evidence
                .iter()
                .map(|s| s.line)
                .collect::<Vec<_>>(),
            vec![1, 10, 15, 20]
        );
        let observed = candidate.observed.join("\n");
        assert!(observed.contains("90.00% to 10.00%"));
        assert!(observed.contains("16000 tokens"));
        assert!(observed.contains("$+0.10000000"));
        assert!(observed.contains("1000 ms"));
        ledger.requests.reverse();
        ledger.observations.reverse();
        assert_eq!(candidate.id, detect(&ledger, &Query::default())[0].id);
        ledger.contexts[0].source.version = "snapshot-b".into();
        let without_matching_event = detect(&ledger, &Query::default());
        assert_ne!(candidate.id, without_matching_event[0].id);
        assert!(!without_matching_event[0]
            .evidence
            .iter()
            .any(|s| s.line == 15));
    }

    #[test]
    fn shrinking_context_with_falling_cost_is_not_adverse_even_with_large_hit_drop() {
        let mut ledger = ledger();
        ledger.observations[1].usage = Usage {
            input_tokens: 17000,
            cache_read_tokens: 1000,
            ..Usage::default()
        };
        ledger.observations[1].reported_usd = Some(0.01);
        assert!(detect(&ledger, &Query::default()).is_empty());
    }

    #[test]
    fn missing_counters_and_intervening_unknown_requests_are_sequence_barriers() {
        let mut missing = ledger();
        missing.observations[1].cache_counters_complete = false;
        assert!(detect(&missing, &Query::default()).is_empty());
        let mut intervening = ledger();
        request(&mut intervening, 15, 0, 0, 0, None);
        intervening.observations.pop();
        assert!(detect(&intervening, &Query::default()).is_empty());
        intervening.requests.pop();
        intervening.observations.push(UsageObservation {
            session_id: "session".into(),
            basis: "unattributed".into(),
            source: source(15),
            ..UsageObservation::default()
        });
        assert!(detect(&intervening, &Query::default()).is_empty());
    }

    #[test]
    fn model_changes_in_requests_or_context_events_block_comparison() {
        let mut switched = ledger();
        switched.requests[1].model = "other".into();
        switched.observations[1].model = "other".into();
        assert!(detect(&switched, &Query::default()).is_empty());
        let mut returned = ledger();
        request(&mut returned, 15, 2000, 18000, 0, Some(0.02));
        returned.requests[2].model = "other".into();
        returned.observations[2].model = "other".into();
        assert!(detect(&returned, &Query::default()).is_empty());
        let mut event = ledger();
        event.contexts.push(ContextEvent {
            session_id: "session".into(),
            kind: "model_change".into(),
            source: source(15),
            ..ContextEvent::default()
        });
        assert!(detect(&event, &Query::default()).is_empty());
    }

    #[test]
    fn duplicate_ids_cumulative_usage_and_unknown_models_are_excluded() {
        let original = ledger();
        let mut duplicate_request = original.clone();
        duplicate_request
            .requests
            .push(duplicate_request.requests[0].clone());
        assert!(detect(&duplicate_request, &Query::default()).is_empty());
        let mut duplicate_usage = original.clone();
        duplicate_usage
            .observations
            .push(duplicate_usage.observations[0].clone());
        assert!(detect(&duplicate_usage, &Query::default()).is_empty());
        let mut cumulative = original.clone();
        cumulative.observations[1].basis = "cumulative_delta".into();
        assert!(detect(&cumulative, &Query::default()).is_empty());
        let mut unknown_model = original;
        for request in &mut unknown_model.requests {
            request.model.clear();
        }
        for observation in &mut unknown_model.observations {
            observation.model.clear();
        }
        assert!(detect(&unknown_model, &Query::default()).is_empty());
    }

    #[test]
    fn large_ratio_changes_need_absolute_affected_tokens_and_non_reused_increase() {
        let mut small = ledger();
        for observation in &mut small.observations {
            observation.usage.input_tokens /= 100;
            observation.usage.cache_read_tokens /= 100;
            observation.usage.cache_creation_tokens /= 100;
        }
        assert!(detect(&small, &Query::default()).is_empty());
        let mut removed_context = ledger();
        removed_context.observations[1].usage.cache_creation_tokens = 0;
        assert!(detect(&removed_context, &Query::default()).is_empty());
    }

    #[test]
    fn unknown_and_mixed_money_do_not_discard_measured_discontinuity() {
        let mut ledger = ledger();
        ledger.observations[1].reported_usd = None;
        let unknown = detect(&ledger, &Query::default());
        assert_eq!(unknown[0].related_cost.unknown_observations, 1);
        assert_eq!(unknown[0].related_cost.estimated_usd, 0.0);
        assert!(unknown[0]
            .observed
            .iter()
            .any(|s| s.contains("cost change is unknown or not comparable")));
        ledger.observations[1].estimated_usd = Some(0.12);
        ledger.observations[1].pricing_version = Some("price-a".into());
        let mixed = detect(&ledger, &Query::default());
        assert_eq!(mixed[0].related_cost.estimated_usd, 0.12);
        assert!(mixed[0]
            .observed
            .iter()
            .any(|s| s.contains("cost change is unknown or not comparable")));
        ledger.observations[0].reported_usd = None;
        ledger.observations[0].estimated_usd = Some(0.02);
        ledger.observations[0].pricing_version = Some("price-a".into());
        assert!(detect(&ledger, &Query::default())[0]
            .observed
            .iter()
            .any(|s| s.contains("Comparable estimated")));
        ledger.observations[0].pricing_version = Some("price-b".into());
        assert!(detect(&ledger, &Query::default())[0]
            .observed
            .iter()
            .any(|s| s.contains("cost change is unknown or not comparable")));
    }

    #[test]
    fn query_bounds_never_link_across_an_excluded_endpoint() {
        let ledger = ledger();
        assert!(detect(
            &ledger,
            &Query {
                since_ms: Some(1500),
                ..Query::default()
            }
        )
        .is_empty());
        assert!(detect(
            &ledger,
            &Query {
                until_ms: Some(2000),
                ..Query::default()
            }
        )
        .is_empty());
        assert!(detect(
            &ledger,
            &Query {
                agents: vec![Agent::Claude],
                ..Query::default()
            }
        )
        .is_empty());
    }

    #[test]
    fn arbitrary_context_kind_is_not_exported() {
        let mut ledger = ledger();
        ledger.contexts.push(ContextEvent {
            session_id: "session".into(),
            kind: "secret-script-body".into(),
            source: source(15),
            ..ContextEvent::default()
        });
        let candidate = detect(&ledger, &Query::default()).pop().unwrap();
        assert!(!serde_json::to_string(&candidate)
            .unwrap()
            .contains("secret-script-body"));
        assert!(candidate.evidence.iter().any(|s| s.line == 15));
    }

    #[test]
    fn requests_on_one_record_do_not_invent_an_adjacent_order() {
        let mut ledger = ledger();
        request(&mut ledger, 10, 2000, 18000, 0, Some(0.02));
        ledger.requests[2].id = "parallel-request".into();
        ledger.observations[2].request_id = Some("parallel-request".into());
        assert!(detect(&ledger, &Query::default()).is_empty());
    }

    #[test]
    fn hit_rate_and_absolute_thresholds_include_the_boundary_not_the_neighbor() {
        let mut ledger = ledger();
        ledger.observations[0].usage = Usage {
            input_tokens: 6000,
            cache_read_tokens: 14000,
            ..Usage::default()
        };
        ledger.observations[1].usage = Usage {
            input_tokens: 12000,
            cache_read_tokens: 8000,
            ..Usage::default()
        };
        assert_eq!(detect(&ledger, &Query::default()).len(), 1);
        ledger.observations[1].usage.input_tokens = 11999;
        ledger.observations[1].usage.cache_read_tokens = 8001;
        assert!(detect(&ledger, &Query::default()).is_empty());
        ledger.observations[0].usage = Usage {
            cache_read_tokens: 10000,
            ..Usage::default()
        };
        ledger.observations[1].usage = Usage {
            input_tokens: 4096,
            cache_read_tokens: 5904,
            ..Usage::default()
        };
        assert_eq!(detect(&ledger, &Query::default()).len(), 1);
        ledger.observations[1].usage.input_tokens = 4095;
        ledger.observations[1].usage.cache_read_tokens = 5905;
        assert!(detect(&ledger, &Query::default()).is_empty());
    }
}
