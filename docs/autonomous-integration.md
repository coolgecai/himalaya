# Autonomous Integration Diagnostics

P7-A adds a read-only integration report for the autonomous agent loop. The report is intentionally a diagnostic surface, not another state-mutating loop.

For release-readiness criteria and final smoke tests, see [Autonomous Release Readiness](autonomous-release-readiness.md).

## Inputs

`AutonomousIntegrationInput` is assembled from existing snapshots:

- task registry tasks, progress ledger, and event log
- scheduler daemon state, daemon events, and durable scheduler queue
- worker registry snapshot
- task memory and route feedback stores
- routing policy proposals
- policy governance ledger and lifecycle replay
- autonomous run history
- autonomous evaluation report

The runtime constructor is pure: it does not read files, write files, start workers, tick the scheduler, or apply policy.

## Output

`AutonomousIntegrationReport` contains:

- `summary`: cross-domain counts for tasks, scheduler, workers, memory, routing, policy, runs, and evaluation score
- `components`: coarse health for each subsystem
- `invariants`: cross-ledger consistency checks with `passed`, `warning`, or `failed`
- `replay`: golden replay stage coverage for the end-to-end autonomous path
- `recommendations`: operator-oriented next actions

P7-B also derives an `AutonomousHealthView` from the integration report. It is the user-facing convergence layer for daemon report/evaluate output:

- `headline`: short human-readable status summary
- `next_action`: the single most useful next operator action
- `safe_to_iterate`: whether another bounded autonomous run is reasonable
- `safe_to_apply_policy`: whether governed policy apply is currently supported by healthy evidence
- `blockers`, `warnings`, and `highlights`: compact operational context

The health view is still read-only. It never starts workers, ticks the scheduler, writes ledgers, applies policy, or rolls anything back.

## Invariants

The current hard checks verify that scheduler queues/events, task memory entries, route feedback, and plan worker ids reference known tasks/workers. Evaluation counters are also checked against the diagnostic snapshot.

Warnings are used for incomplete but recoverable evidence, such as missing worker lifecycle events, policy replay anomalies, or missing optional golden replay stages.

## Golden Replay Stages

The report tracks the expected autonomous path:

1. task lifecycle
2. scheduler queue
3. worker lifecycle
4. memory feedback
5. routing feedback
6. routing policy replay
7. autonomous evaluation

This makes dry-run policy simulations visible without confusing them with persistent routing apply replay.

## CLI Surface

`Himalaya tasks daemon report` and `Himalaya tasks daemon evaluate` include the integration report in JSON and stream-json output. Text output appends a compact integration summary after the existing daemon or evaluation section.

Those same commands now include `health` in JSON and stream-json output. Text output prints the health status, headline, next action, and safe-to-iterate/apply-policy flags before lower-level counters.

`Himalaya tasks daemon status` and `Himalaya tasks daemon logs` expose the same `health` view as a lightweight checkpoint. They do not include the full integration report, but their text output still shows the next operator action and whether iteration or governed policy apply is safe.

## Recommended Workflow

Use the read-only diagnostics before running or applying anything:

1. `Himalaya tasks daemon report --limit 20 --max-ticks 3`
2. `Himalaya tasks daemon evaluate --limit 20 --max-ticks 3`
3. `Himalaya tasks daemon replay --limit 20 --max-ticks 3`

If health is `blocked`, fix the listed blocker first. If health is `degraded`, prefer one bounded run such as `Himalaya tasks daemon start --max-ticks 1` and then re-run the report. Keep policy changes in dry-run mode until health is `healthy`.
