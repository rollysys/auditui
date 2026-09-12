//! Conservative, evidence-linked candidates, not a waste or savings classifier.
//!
//! Tool counts and response bytes never stand in for money. Related costs are
//! entire explicitly linked requests, and may overlap both other work and other
//! candidates. Detection uses source order, not the order of a parsed vector.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::{
    fingerprint, observation_in_window, Candidate, ContextEvent, CostTotals, Ledger, LlmRequest,
    Query, Session, SourceRef, ToolExecution, UsageObservation,
};
use crate::providers::Agent;

const EVIDENCE_LIMIT: usize = 32;
const ANALYSIS_VERSION: &str = "ledger-candidates-v2";

/// Find observed recovery, repeated script generation, cache discontinuities,
/// context growth and workflows. Related spend is never claimed avoidable.
pub fn candidates(ledger: &Ledger, query: &Query) -> Vec<Candidate> {
    let index = Index::new(ledger, query);
    let mut found = Vec::new();
    let mut sequences = BTreeMap::<(&str, [Operation<'_>; 2]), Vec<Occurrence<'_>>>::new();

    for session in ledger.sessions.iter().filter(|s| selected(s, query)) {
        let batches = serial_batches(&index, session);
        recovery(&index, session, &batches, &mut found);
        context_growth(&index, session, &mut found);
        for pair in batches.windows(2) {
            let (Some(first), Some(second)) = (pair[0], pair[1]) else {
                continue;
            };
            if !result_before_call(first, second) {
                continue;
            }
            let (Some(a), Some(b)) = (operation(first), operation(second)) else {
                continue;
            };
            // Repeating one identical call is not a multi-operation workflow.
            if a == b {
                continue;
            }
            sequences
                .entry((&session.project, [a, b]))
                .or_default()
                .push(Occurrence {
                    session,
                    first,
                    second,
                });
        }
    }
    repeated_sequences(&index, sequences, &mut found);
    found.extend(super::script_generation::detect(ledger, query));
    found.extend(super::cache_discontinuity::detect(ledger, query));
    // Put specific actionable evidence before ordinary context growth. Within
    // each tier, mixed reported/estimated spend is a display ordering only,
    // not severity, an invoice total, or a ranking of savings.
    found.sort_by(|a, b| {
        let a_cost = a.related_cost.reported_usd + a.related_cost.estimated_usd;
        let b_cost = b.related_cost.reported_usd + b.related_cost.estimated_usd;
        (a.kind == "context_growth")
            .cmp(&(b.kind == "context_growth"))
            .then_with(|| b_cost.total_cmp(&a_cost))
            .then_with(|| a.id.cmp(&b.id))
    });
    found
}

pub(super) struct Index<'a> {
    ledger: &'a Ledger,
    query: &'a Query,
    sessions: HashMap<&'a str, &'a Session>,
    requests: HashMap<(&'a str, &'a str), &'a LlmRequest>,
    observations: HashMap<(&'a str, &'a str), Vec<&'a UsageObservation>>,
    session_observations: HashMap<&'a str, Vec<&'a UsageObservation>>,
    contexts: HashMap<&'a str, Vec<&'a ContextEvent>>,
    pub(super) tools: HashMap<&'a str, Vec<&'a ToolExecution>>,
}

impl<'a> Index<'a> {
    pub(super) fn new(ledger: &'a Ledger, query: &'a Query) -> Self {
        let mut observations = HashMap::<_, Vec<_>>::new();
        let mut session_observations = HashMap::<_, Vec<_>>::new();
        for observation in &ledger.observations {
            session_observations
                .entry(observation.session_id.as_str())
                .or_default()
                .push(observation);
            if let Some(request) = observation.request_id.as_deref() {
                observations
                    .entry((observation.session_id.as_str(), request))
                    .or_default()
                    .push(observation);
            }
        }
        let mut tools = HashMap::<_, Vec<_>>::new();
        for execution in &ledger.tools {
            tools
                .entry(execution.session_id.as_str())
                .or_default()
                .push(execution);
        }
        let mut contexts = HashMap::<_, Vec<_>>::new();
        for event in &ledger.contexts {
            contexts
                .entry(event.session_id.as_str())
                .or_default()
                .push(event);
        }
        Self {
            ledger,
            query,
            sessions: ledger.sessions.iter().map(|s| (s.id.as_str(), s)).collect(),
            requests: ledger
                .requests
                .iter()
                .map(|r| ((r.session_id.as_str(), r.id.as_str()), r))
                .collect(),
            observations,
            session_observations,
            contexts,
            tools,
        }
    }

    pub(super) fn request(&self, tool: &ToolExecution) -> Option<&'a LlmRequest> {
        self.requests
            .get(&(tool.session_id.as_str(), tool.request_id.as_deref()?))
            .copied()
    }

    pub(super) fn tool_in_window(&self, tool: &ToolExecution) -> bool {
        in_window(
            tool.start_ms
                .or_else(|| self.request(tool).and_then(|r| r.ts_ms)),
            self.query,
        )
    }

    fn result_in_window(&self, tool: &ToolExecution) -> bool {
        tool.result.is_some() && in_window(tool.end_ms, self.query)
    }

    pub(super) fn linked<'b>(
        &self,
        tools: &[&'b ToolExecution],
        direct: &[&'b UsageObservation],
        evidence: &mut Vec<&'b SourceRef>,
        limitations: &mut Vec<String>,
    ) -> CostTotals
    where
        'a: 'b,
    {
        let mut keys = BTreeSet::new();
        let mut unlinked_tools = 0;
        for tool in tools {
            if let Some(request) = self.request(tool) {
                keys.insert((request.session_id.as_str(), request.id.as_str()));
            } else {
                unlinked_tools += 1;
            }
        }
        for observation in direct {
            if let Some(request_id) = observation.request_id.as_deref() {
                let key = (observation.session_id.as_str(), request_id);
                if self.requests.contains_key(&key) {
                    keys.insert(key);
                }
            }
        }
        let mut totals = CostTotals::default();
        let mut seen_observations = HashSet::new();
        let mut missing_usage = 0;
        for key in keys {
            let request = self.requests[&key];
            evidence.push(&request.source);
            if in_window(request.ts_ms, self.query) {
                totals.requests += 1;
            }
            let mut has_usage = false;
            if let Some(observations) = self.observations.get(&key) {
                for observation in observations {
                    if observation.basis == "unattributed"
                        || !observation_in_window(observation, self.query)
                    {
                        continue;
                    }
                    has_usage = true;
                    if seen_observations
                        .insert((observation.session_id.as_str(), observation.id.as_str()))
                    {
                        totals.add_observation(observation);
                        evidence.push(&observation.source);
                    }
                }
            }
            if !has_usage {
                missing_usage += 1;
            }
        }
        if unlinked_tools > 0 {
            limitations.push(format!(
                "{unlinked_tools} tool executions lack an explicit matching request; no session-wide usage is assigned to them."
            ));
        }
        if missing_usage > 0 {
            limitations.push(format!(
                "{missing_usage} linked requests have no attributable usage observation in this window; their missing cost is unknown, not zero."
            ));
        }
        if totals.unknown_observations > 0 {
            limitations.push(format!(
                "{} linked observations have unknown monetary cost; recorded tokens do not supply a price.",
                totals.unknown_observations
            ));
        }
        if totals.observations == 0 {
            limitations.push("No linked in-window cost observation is available; zero displayed monetary buckets do not establish free work.".into());
        }
        limitations.push("Related cost covers whole explicitly linked requests, including other work in those requests; it is not marginal tool cost, avoidable cost, or savings.".into());
        limitations.push("Requests can overlap other candidates and candidate kinds. Do not sum candidate costs; use the ledger cost report for deduplicated totals. Unattributed session deltas are not allocated to tools.".into());
        totals
    }
}

pub(super) fn selected(session: &Session, query: &Query) -> bool {
    matches!(session.provider, Agent::Claude | Agent::Codex | Agent::Omp)
        && query.project.as_ref().is_none_or(|p| p == &session.project)
        && (query.agents.is_empty() || query.agents.contains(&session.provider))
}

pub(super) fn in_window(ts: Option<i64>, query: &Query) -> bool {
    match ts {
        Some(ts) => {
            query.since_ms.is_none_or(|since| ts >= since)
                && query.until_ms.is_none_or(|until| ts < until)
        }
        None => query.since_ms.is_none() && query.until_ms.is_none(),
    }
}

pub(super) fn source_key(source: &SourceRef) -> (&str, &str, u64, Option<&str>) {
    (
        &source.source_id,
        &source.version,
        source.line,
        source.record_id.as_deref(),
    )
}

pub(super) fn before(a: &SourceRef, a_ms: Option<i64>, b: &SourceRef, b_ms: Option<i64>) -> bool {
    if a.source_id == b.source_id && a.version == b.version {
        a.line < b.line
    } else {
        matches!((a_ms, b_ms), (Some(a), Some(b)) if a < b)
    }
}

pub(super) fn result_before_call(first: &ToolExecution, second: &ToolExecution) -> bool {
    first.batch_id != second.batch_id
        && first
            .result
            .as_ref()
            .is_some_and(|result| before(result, first.end_ms, &second.call, second.start_ms))
}

/// No invented sequence within a parallel batch. Mixed substantive batches are
/// barriers; control-plane-only batches are ignored rather than workflow steps.
fn serial_batches<'a>(index: &Index<'a>, session: &Session) -> Vec<Option<&'a ToolExecution>> {
    let Some(tools) = index.tools.get(session.id.as_str()) else {
        return Vec::new();
    };
    let mut ordered: Vec<_> = tools.iter().copied().collect();
    ordered.sort_by(|a, b| {
        source_key(&a.call)
            .cmp(&source_key(&b.call))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut batches = Vec::<Vec<&ToolExecution>>::new();
    let mut positions = HashMap::<&str, usize>::new();
    for tool in ordered {
        if tool.batch_id.is_empty() {
            batches.push(vec![tool]);
            continue;
        }
        if let Some(position) = positions.get(tool.batch_id.as_str()) {
            batches[*position].push(tool);
        } else {
            positions.insert(tool.batch_id.as_str(), batches.len());
            batches.push(vec![tool]);
        }
    }
    batches
        .into_iter()
        .filter_map(|batch| {
            let substantive: Vec<_> = batch
                .into_iter()
                .filter(|tool| !control_plane(tool))
                .collect();
            if substantive.is_empty() {
                None
            } else if substantive.len() == 1
                && !substantive[0].batch_id.is_empty()
                && index.tool_in_window(substantive[0])
            {
                Some(Some(substantive[0]))
            } else {
                Some(None)
            }
        })
        .collect()
}

fn control_plane(tool: &ToolExecution) -> bool {
    matches!(
        tool.operation.as_str(),
        "hub:wait"
            | "hub:jobs"
            | "hub:inbox"
            | "hub:list"
            | "hub:ps"
            | "yield"
            | "TodoWrite"
            | "todowrite"
            | "update_plan"
    )
}

fn same_target(a: &ToolExecution, b: &ToolExecution) -> bool {
    a.session_id == b.session_id
        && !a.operation.is_empty()
        && a.operation == b.operation
        && a.object.as_deref().is_some_and(|object| !object.is_empty())
        && a.object == b.object
}

fn recovery(
    index: &Index<'_>,
    session: &Session,
    batches: &[Option<&ToolExecution>],
    found: &mut Vec<Candidate>,
) {
    let mut position = 0;
    while position < batches.len() {
        let Some(first) = batches[position] else {
            position += 1;
            continue;
        };
        if first.status != "failure" || first.result.is_none() {
            position += 1;
            continue;
        }
        let mut attempts = vec![first];
        let mut end = position;
        while let Some(Some(next)) = batches.get(end + 1) {
            let previous = attempts[attempts.len() - 1];
            if previous.status != "failure"
                || !same_target(previous, next)
                || !result_before_call(previous, next)
            {
                break;
            }
            attempts.push(*next);
            end += 1;
        }
        position = end + 1;
        if attempts.len() < 2 {
            continue;
        }
        let terminal = attempts[attempts.len() - 1];
        let state = match terminal.status.as_str() {
            "success" if index.result_in_window(terminal) => "success with an observed result",
            "failure" if index.result_in_window(terminal) => "failure with an observed result",
            "success" | "failure" => "unknown at query boundary; the terminal result is outside or not timed within this window",
            "pending" => "pending; no terminal result observed",
            _ => "unknown; success is not established",
        };
        let mut evidence = vec![&session.source];
        for (attempt, tool) in attempts.iter().enumerate() {
            evidence.push(&tool.call);
            if attempt + 1 < attempts.len() || index.result_in_window(tool) {
                evidence.extend(tool.result.as_ref());
            }
        }
        let mut limitations = vec![
            "This narrow detector requires consecutive related substantive batches and an observed failure before the next call. Intervening edit/build work and parallel batches are not classified as blind retries.".into(),
            "Equal operation and resolved object support relatedness, not identical user intent. Recovery can be necessary; no failed-attempt spend is declared waste.".into(),
            "Only attempts within the query window are selected; missing-time attempts are excluded from bounded queries. Recovery outside this window may change the terminal interpretation.".into(),
        ];
        let related_cost = index.linked(&attempts, &[], &mut evidence, &mut limitations);
        let observed = vec![
            format!("{} related attempts in distinct assistant batches; each retry follows its predecessor's observed failed result.", attempts.len()),
            format!("Terminal state: {state}."),
        ];
        found.push(finish(index.query, Candidate {
            kind: "recovery".into(),
            project: session.project.clone(),
            summary: format!("Retry-recovery candidate: {} attempts; {state}.", attempts.len()),
            observed,
            hypothesis: "If this is a recurring operational failure, a reusable recovery rule may reduce repeated handling. Inspect the required remediation and task outcome before intervening.".into(),
            related_cost,
            limitations,
            ..Candidate::default()
        }, evidence));
    }
}

fn context_input(observation: &UsageObservation) -> u64 {
    let usage = &observation.usage;
    usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_creation_total())
}

fn context_growth(index: &Index<'_>, session: &Session, found: &mut Vec<Candidate>) {
    let Some(session_observations) = index.session_observations.get(session.id.as_str()) else {
        return;
    };
    let mut observations: Vec<_> = session_observations
        .iter()
        .copied()
        .filter(|observation| {
            if observation.basis != "request"
                || !observation_in_window(observation, index.query)
                || context_input(observation) == 0
                || observation.model.is_empty()
            {
                return false;
            }
            let Some(request_id) = observation.request_id.as_deref() else {
                return false;
            };
            let key = (observation.session_id.as_str(), request_id);
            // Ambiguous multiple observations of a request are not a context series.
            index.requests.contains_key(&key)
                && index
                    .observations
                    .get(&key)
                    .is_some_and(|values| values.len() == 1)
        })
        .collect();
    observations.sort_by(|a, b| {
        source_key(&a.source)
            .cmp(&source_key(&b.source))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut related = BTreeMap::new();
    let mut increases = 0;
    let mut largest: Option<(&UsageObservation, &UsageObservation)> = None;
    for pair in observations.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.model != b.model
            || !before(&a.source, a.ts_ms, &b.source, b.ts_ms)
            || context_input(b) <= context_input(a)
        {
            continue;
        }
        increases += 1;
        related.insert(a.id.as_str(), a);
        related.insert(b.id.as_str(), b);
        let growth = context_input(b) - context_input(a);
        if largest.is_none_or(|(old_a, old_b)| growth > context_input(old_b) - context_input(old_a))
        {
            largest = Some((a, b));
        }
    }
    let Some((first, last)) = largest else {
        return;
    };
    let linked_observations: Vec<_> = related.values().copied().collect();
    let mut evidence = vec![&session.source];
    evidence.extend(
        linked_observations
            .iter()
            .map(|observation| &observation.source),
    );
    let mut annotations = BTreeMap::<&str, usize>::new();
    for event in index
        .contexts
        .get(session.id.as_str())
        .into_iter()
        .flatten()
    {
        if before(&first.source, first.ts_ms, &event.source, event.ts_ms)
            && before(&event.source, event.ts_ms, &last.source, last.ts_ms)
        {
            // Unknown kinds might contain source text; never echo them.
            let kind = match event.kind.as_str() {
                "compaction" => "compaction",
                "model_change" => "model change",
                _ => "other recorded context event",
            };
            *annotations.entry(kind).or_default() += 1;
            evidence.push(&event.source);
        }
    }
    let mut observed = vec![
        format!("{increases} observed same-model request-to-request input/cache increases across {} linked requests.", related.len()),
        format!("Largest adjacent observed increase: {} to {} input/cache tokens; these are request measurements, not tokens attributed to a tool result.", context_input(first), context_input(last)),
        format!("Before: input={}, cache-read={}, cache-write={}; after: input={}, cache-read={}, cache-write={}.",
            first.usage.input_tokens, first.usage.cache_read_tokens, first.usage.cache_creation_total(),
            last.usage.input_tokens, last.usage.cache_read_tokens, last.usage.cache_creation_total()),
    ];
    if annotations.is_empty() {
        observed.push("No recorded context event lies between the displayed pair; absence of an event does not prove unchanged context.".into());
    } else {
        observed.extend(annotations.into_iter().map(|(kind, count)| {
            format!("Context annotation between the displayed pair: {count} {kind} event(s).")
        }));
    }
    let mut limitations = vec![
        "Input/cache totals use recorded request input plus cache-read and cache-write tokens, not output length or tool-result bytes. They are not an exact context-window occupancy measurement.".into(),
        "Only positive, unambiguous, request-linked usage is compared, within the same model. Cumulative deltas, unknown usage and model transitions are not interpreted as context growth.".into(),
        "Growth can be necessary for the task and can reflect instructions, user input, retrieval, or context management. No causal token contribution or monetary saving is assigned to any tool or context event.".into(),
        "Costs cover the union of request endpoints for observed increases, not a hypothetical constant-context baseline. The displayed largest increase is a descriptive example, not a severity threshold.".into(),
    ];
    let related_cost = index.linked(&[], &linked_observations, &mut evidence, &mut limitations);
    found.push(finish(index.query, Candidate {
        kind: "context_growth".into(),
        project: session.project.clone(),
        summary: format!("Observed input/cache growth in {increases} same-model request transitions."),
        observed,
        hypothesis: "Review whether repeated context is still needed and whether context management preserves task quality. Measurements alone cannot identify a removable cause.".into(),
        related_cost,
        limitations,
        ..Candidate::default()
    }, evidence));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Operation<'a> {
    operation: &'a str,
    object: &'a str,
    fingerprint: &'a str,
}

fn operation(tool: &ToolExecution) -> Option<Operation<'_>> {
    if tool.operation.is_empty() || tool.args_fingerprint.is_empty() || control_plane(tool) {
        return None;
    }
    // A project label cannot establish the cwd of a child session. Do not
    // merge unresolved relative paths or scripts merely because args match.
    let object = tool.object.as_deref().filter(|object| !object.is_empty())?;
    Some(Operation {
        operation: &tool.operation,
        object,
        fingerprint: &tool.args_fingerprint,
    })
}

struct Occurrence<'a> {
    session: &'a Session,
    first: &'a ToolExecution,
    second: &'a ToolExecution,
}

/// Explicit parent/task relationships prevent a parent and its subagents (or
/// two attempts at one known task) from becoming independent replication.
fn family<'a>(
    session: &'a Session,
    sessions: &HashMap<&str, &'a Session>,
) -> (&'a str, BTreeSet<&'a str>) {
    let mut current = session;
    let mut visited = BTreeSet::new();
    let mut tasks = BTreeSet::new();
    loop {
        if !visited.insert(current.id.as_str()) {
            return (
                visited.first().copied().unwrap_or(session.id.as_str()),
                tasks,
            );
        }
        if let Some(task) = current.task_id.as_deref().filter(|task| !task.is_empty()) {
            tasks.insert(task);
        }
        let Some(parent_id) = current.parent_id.as_deref() else {
            return (current.id.as_str(), tasks);
        };
        let Some(parent) = sessions.get(parent_id) else {
            return (parent_id, tasks);
        };
        current = parent;
    }
}

fn repeated_sequences<'a>(
    index: &Index<'a>,
    sequences: BTreeMap<(&str, [Operation<'_>; 2]), Vec<Occurrence<'a>>>,
    found: &mut Vec<Candidate>,
) {
    let families: HashMap<_, _> = index
        .ledger
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), family(session, &index.sessions)))
        .collect();
    for ((project, _), occurrences) in sequences {
        let mut by_session = BTreeMap::<&str, Vec<&Occurrence<'_>>>::new();
        for occurrence in &occurrences {
            by_session
                .entry(&occurrence.session.id)
                .or_default()
                .push(occurrence);
        }
        let mut selected = Vec::new();
        let mut roots = HashSet::new();
        let mut tasks = BTreeSet::new();
        for (session_id, values) in by_session {
            let (root, known_tasks) = &families[session_id];
            if roots.contains(root) || !tasks.is_disjoint(known_tasks) {
                continue;
            }
            roots.insert(*root);
            tasks.extend(known_tasks.iter().copied());
            selected.push(values);
        }
        if selected.len() < 2 {
            continue;
        }
        let mut tools = Vec::new();
        let mut evidence = Vec::new();
        let mut occurrences_count = 0;
        let mut successful = 0;
        for values in &selected {
            evidence.push(&values[0].session.source);
            for occurrence in values {
                occurrences_count += 1;
                if occurrence.first.status == "success"
                    && occurrence.second.status == "success"
                    && index.result_in_window(occurrence.second)
                {
                    successful += 1;
                }
                for (position, tool) in [occurrence.first, occurrence.second]
                    .into_iter()
                    .enumerate()
                {
                    tools.push(tool);
                    evidence.push(&tool.call);
                    if position == 0 || index.result_in_window(tool) {
                        evidence.extend(tool.result.as_ref());
                    }
                }
            }
        }
        let mut limitations = vec![
            "This is an exact adjacent two-operation fragment, not proof of a whole reusable task. Successful repetitions, including edit/build workflows, can be necessary and are not labeled waste.".into(),
            "Matching preserves semantic argument fingerprints, exact script identity, resolved object/cwd identity and project identity. No broad Python, shell, path, or string normalization is used.".into(),
            "Distinct sessions exclude known common parent families and task IDs. Missing task metadata means real-world task independence cannot be proven; sessions are not inferred from user turns.".into(),
            "Parallel or unresolved preceding operations do not establish a sequence. Exact matching intentionally misses semantically equivalent scripts with different bytes.".into(),
        ];
        let related_cost = index.linked(&tools, &[], &mut evidence, &mut limitations);
        found.push(finish(index.query, Candidate {
            kind: "repeated_sequence".into(),
            project: project.to_string(),
            summary: format!("A two-operation workflow recurs in {} independent session families.", selected.len()),
            observed: vec![
                format!("{occurrences_count} exact ordered occurrences across {} selected distinct sessions in one project.", selected.len()),
                format!("{successful} occurrences have observed successful results for both operations; unsuccessful or pending outcomes are retained, not inferred successful."),
            ],
            hypothesis: "A recurring workflow fragment may justify a reusable skill or command if its preconditions and quality checks are stable. Verify that packaging reduces effort without removing necessary edits or validation.".into(),
            related_cost,
            limitations,
            ..Candidate::default()
        }, evidence));
    }
}

pub(super) fn finish(
    query: &Query,
    mut candidate: Candidate,
    mut evidence: Vec<&SourceRef>,
) -> Candidate {
    evidence.sort_by_key(|source| source_key(source));
    evidence.dedup_by(|a, b| source_key(a) == source_key(b));
    let mut agents = if query.agents.is_empty() {
        vec![Agent::Claude, Agent::Codex, Agent::Omp]
    } else {
        query.agents.clone()
    };
    agents.sort();
    agents.dedup();
    let identity = serde_json::json!({
        "analysis_version": ANALYSIS_VERSION,
        "kind": candidate.kind,
        "project": candidate.project,
        "query": {
            "root": query.root,
            "since_ms": query.since_ms,
            "until_ms": query.until_ms,
            "project": query.project,
            "agents": agents,
        },
        "evidence": evidence.iter().map(|source| source_key(source)).collect::<Vec<_>>(),
    });
    candidate.id = format!("candidate-{}", fingerprint(identity.to_string().as_bytes()));
    if evidence.len() > EVIDENCE_LIMIT {
        candidate.limitations.push(format!(
            "Evidence display is bounded to {EVIDENCE_LIMIT} of {} references (first and last examples); identity and related costs use all matched evidence.", evidence.len()
        ));
        let half = EVIDENCE_LIMIT / 2;
        candidate.evidence = evidence[..half]
            .iter()
            .chain(evidence[evidence.len() - half..].iter())
            .map(|source| (*source).clone())
            .collect();
    } else {
        candidate.evidence = evidence.into_iter().cloned().collect();
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::Usage;
    use crate::ledger::ContextEvent;
    use std::path::PathBuf;

    fn source(session: &str, line: u64) -> SourceRef {
        SourceRef {
            source_id: session.into(),
            version: "version-a".into(),
            path: PathBuf::from(format!("/private/transcripts/{session}.jsonl")),
            line,
            record_id: None,
        }
    }

    fn session(ledger: &mut Ledger, id: &str, project: &str) {
        ledger.sessions.push(Session {
            id: id.into(),
            provider: Agent::Omp,
            project: project.into(),
            parent_id: None,
            task_id: None,
            work_type: None,
            source: source(id, 1),
        });
    }

    fn execution(
        ledger: &mut Ledger,
        session: &str,
        line: u64,
        operation: &str,
        object: &str,
        args: &str,
        status: &str,
    ) {
        let request_id = format!("{session}:request:{line}");
        ledger.requests.push(LlmRequest {
            id: request_id.clone(),
            session_id: session.into(),
            ts_ms: Some(line as i64 * 100),
            model: "known-model".into(),
            status: "complete".into(),
            source: source(session, line),
        });
        ledger.observations.push(UsageObservation {
            id: format!("{request_id}:usage"),
            session_id: session.into(),
            request_id: Some(request_id.clone()),
            ts_ms: Some(line as i64 * 100),
            model: "known-model".into(),
            basis: "request".into(),
            usage: Usage {
                input_tokens: 100,
                ..Usage::default()
            },
            reported_usd: Some(1.0),
            source: source(session, line),
            ..UsageObservation::default()
        });
        ledger.tools.push(ToolExecution {
            id: format!("{session}:tool:{line}"),
            session_id: session.into(),
            request_id: Some(request_id.clone()),
            batch_id: request_id,
            name: operation.split(':').next().unwrap().into(),
            operation: operation.into(),
            object: Some(object.into()),
            args_fingerprint: fingerprint(args.as_bytes()),
            start_ms: Some(line as i64 * 100),
            end_ms: (status != "pending").then_some((line as i64 + 1) * 100),
            status: status.into(),
            call: source(session, line),
            result: (status != "pending").then(|| source(session, line + 1)),
            ..ToolExecution::default()
        });
    }

    fn retry_ledger() -> Ledger {
        let mut ledger = Ledger::default();
        session(&mut ledger, "one", "/project");
        execution(
            &mut ledger,
            "one",
            2,
            "read",
            "/project/data.csv",
            "range:1",
            "failure",
        );
        execution(
            &mut ledger,
            "one",
            4,
            "read",
            "/project/data.csv",
            "range:2",
            "success",
        );
        ledger
    }

    fn workflow(ledger: &mut Ledger, id: &str, project: &str, cwd: &str, script: &str) {
        session(ledger, id, project);
        execution(
            ledger,
            id,
            2,
            "read",
            &format!("{cwd}/input.csv"),
            "input.csv",
            "success",
        );
        execution(
            ledger,
            id,
            4,
            "bash",
            &format!("cwd:{cwd}:command:{}", fingerprint(script.as_bytes())),
            script,
            "success",
        );
    }

    fn only_kind(ledger: &Ledger, query: &Query, kind: &str) -> Vec<Candidate> {
        candidates(ledger, query)
            .into_iter()
            .filter(|candidate| candidate.kind == kind)
            .collect()
    }

    #[test]
    fn recovery_joins_only_observed_related_requests_without_allocating_session_cost() {
        let mut ledger = retry_ledger();
        ledger.observations[0].estimated_usd = Some(50.0);
        ledger.observations[1].reported_usd = None;
        ledger.observations[1].estimated_usd = Some(2.0);
        let mut unattributed = ledger.observations[0].clone();
        unattributed.id = "unattributed".into();
        unattributed.request_id = None;
        unattributed.basis = "cumulative_delta".into();
        unattributed.reported_usd = Some(1_000.0);
        ledger.observations.push(unattributed);
        execution(
            &mut ledger,
            "one",
            8,
            "read",
            "/project/unrelated.csv",
            "unrelated",
            "success",
        );
        ledger.observations.last_mut().unwrap().reported_usd = Some(100.0);

        let found = only_kind(&ledger, &Query::default(), "recovery");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].related_cost.reported_usd, 1.0);
        assert_eq!(found[0].related_cost.estimated_usd, 2.0);
        assert_eq!(found[0].related_cost.observations, 2);
        assert_eq!(found[0].related_cost.requests, 2);
        assert!(found[0]
            .observed
            .iter()
            .any(|item| item.contains("success")));
        assert!(found[0].evidence.iter().any(|item| item.line == 5));
    }

    #[test]
    fn parallel_failure_is_not_a_retry_even_if_one_result_arrives_first() {
        let mut ledger = retry_ledger();
        ledger.tools[1].batch_id = ledger.tools[0].batch_id.clone();
        assert!(only_kind(&ledger, &Query::default(), "recovery").is_empty());
    }

    #[test]
    fn failure_arriving_after_next_call_does_not_establish_recovery() {
        let mut ledger = retry_ledger();
        ledger.tools[0].result = Some(source("one", 6));
        ledger.tools[0].end_ms = Some(600);
        assert!(only_kind(&ledger, &Query::default(), "recovery").is_empty());
        ledger.tools[0].result = None;
        assert!(only_kind(&ledger, &Query::default(), "recovery").is_empty());
    }

    #[test]
    fn pending_attempt_does_not_become_terminal_success() {
        let mut ledger = retry_ledger();
        ledger.tools[1].status = "pending".into();
        ledger.tools[1].result = None;
        ledger.tools[1].end_ms = None;
        let found = only_kind(&ledger, &Query::default(), "recovery");
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .observed
            .iter()
            .any(|item| item.contains("pending")));
        assert!(!found[0]
            .observed
            .iter()
            .any(|item| item.contains("success")));
    }

    #[test]
    fn result_at_exclusive_query_end_does_not_establish_in_window_terminal_success() {
        let ledger = retry_ledger();
        let query = Query {
            until_ms: Some(500),
            ..Query::default()
        };
        let found = only_kind(&ledger, &query, "recovery");
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .observed
            .iter()
            .any(|item| item.contains("unknown at query boundary")));
        assert!(!found[0]
            .evidence
            .iter()
            .any(|reference| reference.line == 5));
    }

    #[test]
    fn necessary_edit_build_cycle_is_not_a_blind_retry_or_cross_session_workflow() {
        let mut ledger = Ledger::default();
        session(&mut ledger, "one", "/project");
        execution(
            &mut ledger,
            "one",
            2,
            "bash",
            "cwd:/project:command:build",
            "cargo build",
            "failure",
        );
        execution(
            &mut ledger,
            "one",
            4,
            "edit",
            "/project/lib.rs",
            "fix:one",
            "success",
        );
        execution(
            &mut ledger,
            "one",
            6,
            "bash",
            "cwd:/project:command:build",
            "cargo build",
            "failure",
        );
        execution(
            &mut ledger,
            "one",
            8,
            "edit",
            "/project/lib.rs",
            "fix:two",
            "success",
        );
        execution(
            &mut ledger,
            "one",
            10,
            "bash",
            "cwd:/project:command:build",
            "cargo build",
            "success",
        );
        assert!(only_kind(&ledger, &Query::default(), "recovery").is_empty());
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn successful_exact_workflows_recur_without_a_failure_requirement() {
        let mut ledger = Ledger::default();
        workflow(
            &mut ledger,
            "one",
            "/project",
            "/project",
            "python3 analyze.py",
        );
        workflow(
            &mut ledger,
            "two",
            "/project",
            "/project",
            "python3 analyze.py",
        );
        ledger.tools[2].name = "Read".into();
        ledger.tools[3].name = "functions.bash".into();
        let found = only_kind(&ledger, &Query::default(), "repeated_sequence");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].related_cost.reported_usd, 4.0);
        assert_eq!(found[0].related_cost.requests, 4);
    }

    #[test]
    fn unrelated_python_scripts_are_not_generalized_into_one_workflow() {
        let mut ledger = Ledger::default();
        workflow(
            &mut ledger,
            "one",
            "/project",
            "/project",
            "python3 -c 'print(1)'",
        );
        workflow(
            &mut ledger,
            "two",
            "/project",
            "/project",
            "python3 -c 'delete_data()'",
        );
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
        // Even a coarse upstream object must not defeat exact script identity.
        ledger.tools[1].object = Some("python".into());
        ledger.tools[3].object = Some("python".into());
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn cwd_and_project_identity_are_not_normalized_away() {
        let mut ledger = Ledger::default();
        workflow(
            &mut ledger,
            "one",
            "/project",
            "/project/a",
            "python3 analyze.py",
        );
        workflow(
            &mut ledger,
            "two",
            "/project",
            "/project/b",
            "python3 analyze.py",
        );
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
        ledger.tools[2].object = ledger.tools[0].object.clone();
        ledger.tools[3].object = ledger.tools[1].object.clone();
        ledger.sessions[1].project = "/other-project".into();
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn inherited_project_does_not_supply_missing_execution_cwd() {
        let mut ledger = Ledger::default();
        workflow(
            &mut ledger,
            "one",
            "/project",
            "/project",
            "python3 analyze.py",
        );
        workflow(
            &mut ledger,
            "two",
            "/project",
            "/project",
            "python3 analyze.py",
        );
        for tool in &mut ledger.tools {
            tool.object = None;
        }
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn subagents_and_same_known_task_attempts_are_not_independent_sessions() {
        let mut ledger = Ledger::default();
        workflow(
            &mut ledger,
            "parent",
            "/project",
            "/project",
            "python3 analyze.py",
        );
        workflow(
            &mut ledger,
            "child",
            "/project",
            "/project",
            "python3 analyze.py",
        );
        ledger.sessions[1].parent_id = Some("parent".into());
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
        ledger.sessions[1].parent_id = None;
        ledger.sessions[0].task_id = Some("same-task".into());
        ledger.sessions[1].task_id = Some("same-task".into());
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn one_repeated_operation_is_not_a_substantive_two_operation_sequence() {
        let mut ledger = Ledger::default();
        for id in ["one", "two"] {
            session(&mut ledger, id, "/project");
            execution(
                &mut ledger,
                id,
                2,
                "read",
                "/project/data.csv",
                "same",
                "success",
            );
            execution(
                &mut ledger,
                id,
                4,
                "read",
                "/project/data.csv",
                "same",
                "success",
            );
        }
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn parallel_batches_never_supply_an_invented_operation_order() {
        let mut ledger = Ledger::default();
        for id in ["one", "two"] {
            workflow(
                &mut ledger,
                id,
                "/project",
                "/project",
                "python3 analyze.py",
            );
        }
        ledger.tools[1].batch_id = ledger.tools[0].batch_id.clone();
        ledger.tools[3].batch_id = ledger.tools[2].batch_id.clone();
        assert!(only_kind(&ledger, &Query::default(), "repeated_sequence").is_empty());
    }

    #[test]
    fn context_growth_uses_measured_input_cache_not_tool_payload_size() {
        let mut ledger = retry_ledger();
        ledger.tools[0].result_bytes = 10_000_000;
        assert!(only_kind(&ledger, &Query::default(), "context_growth").is_empty());
        ledger.observations[1].usage.cache_read_tokens = 200;
        ledger.contexts.push(ContextEvent {
            session_id: "one".into(),
            kind: "compaction".into(),
            ts_ms: Some(300),
            source: source("one", 3),
        });
        let found = only_kind(&ledger, &Query::default(), "context_growth");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].related_cost.input_tokens, 200);
        assert_eq!(found[0].related_cost.cache_read_tokens, 200);
        assert_eq!(found[0].related_cost.reported_usd, 2.0);
        assert!(found[0]
            .observed
            .iter()
            .any(|item| item.contains("100 to 300")));
        assert!(found[0]
            .observed
            .iter()
            .any(|item| item.contains("compaction")));
    }

    #[test]
    fn cumulative_deltas_model_changes_and_ambiguous_observations_do_not_define_context_growth() {
        let mut ledger = retry_ledger();
        ledger.observations[1].usage.input_tokens = 1_000;
        ledger.observations[1].basis = "cumulative_delta".into();
        assert!(only_kind(&ledger, &Query::default(), "context_growth").is_empty());
        ledger.observations[1].basis = "request".into();
        ledger.observations[1].model = "different-model".into();
        assert!(only_kind(&ledger, &Query::default(), "context_growth").is_empty());
        ledger.observations[1].model = "known-model".into();
        let duplicate = ledger.observations[1].clone();
        ledger.observations.push(duplicate);
        assert!(only_kind(&ledger, &Query::default(), "context_growth").is_empty());
    }

    #[test]
    fn query_window_keeps_unknown_cost_unknown_and_has_exclusive_end() {
        let mut ledger = retry_ledger();
        ledger.observations[0].ts_ms = None;
        ledger.observations[1].reported_usd = None;
        let query = Query {
            since_ms: Some(0),
            until_ms: Some(500),
            ..Query::default()
        };
        let found = only_kind(&ledger, &query, "recovery");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].related_cost.observations, 1);
        assert_eq!(found[0].related_cost.unknown_observations, 1);
        assert_eq!(found[0].related_cost.requests, 2);
        let query = Query {
            until_ms: Some(400),
            ..query
        };
        assert!(only_kind(&ledger, &query, "recovery").is_empty());
        let query = Query {
            project: Some("/other".into()),
            ..Query::default()
        };
        assert!(candidates(&ledger, &query).is_empty());
        let query = Query {
            agents: vec![Agent::Claude],
            ..Query::default()
        };
        assert!(candidates(&ledger, &query).is_empty());
    }

    #[test]
    fn candidate_identity_is_order_stable_and_evidence_version_query_scoped() {
        let mut ledger = retry_ledger();
        let original = only_kind(&ledger, &Query::default(), "recovery")
            .remove(0)
            .id;
        ledger.tools.reverse();
        ledger.requests.reverse();
        ledger.observations.reverse();
        assert_eq!(
            only_kind(&ledger, &Query::default(), "recovery")[0].id,
            original
        );
        let query = Query {
            since_ms: Some(0),
            ..Query::default()
        };
        assert_ne!(only_kind(&ledger, &query, "recovery")[0].id, original);
        let query = Query {
            agents: vec![Agent::Omp, Agent::Claude, Agent::Codex, Agent::Omp],
            ..Query::default()
        };
        assert_eq!(only_kind(&ledger, &query, "recovery")[0].id, original);
        ledger.sessions[0].source.version = "version-b".into();
        assert_ne!(
            only_kind(&ledger, &Query::default(), "recovery")[0].id,
            original
        );
    }

    #[test]
    fn candidate_prose_does_not_export_semantic_arguments_or_object_contents() {
        let mut ledger = retry_ledger();
        for tool in &mut ledger.tools {
            tool.object = Some("/private/customer-account-123/token-secret".into());
            tool.operation = "read:embedded-secret".into();
        }
        let candidate = only_kind(&ledger, &Query::default(), "recovery").remove(0);
        let json = serde_json::to_string(&candidate).unwrap();
        assert!(!json.contains("customer-account-123"));
        assert!(!json.contains("token-secret"));
        assert!(!json.contains("embedded-secret"));
    }
}
