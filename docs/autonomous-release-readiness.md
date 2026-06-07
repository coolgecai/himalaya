# Autonomous Release Readiness

P8-A is a release-readiness audit for the autonomous runtime work completed through P7-D. It is a convergence checkpoint, not a new autonomous loop.

## Stable Capabilities

- durable task registry and scheduler queue
- worker lifecycle supervision and recovery diagnostics
- task memory and route feedback stores
- routing proposal, dry-run apply, governed apply, and rollback plumbing
- policy governance ledger, lifecycle replay, adapter review, and apply coordination
- autonomous evaluation, trace replay, integration report, and health view
- text, JSON, and stream-json output contracts for daemon report and evaluation

## Experimental Capabilities

- fully unattended long-horizon daemon execution
- automatic governed policy apply without human review
- cross-domain optimizer decisions beyond dry-run evidence
- large-scale concurrent worker pools

## Recommended Smoke Tests

Run these before treating a branch as release-ready:

```bash
cargo fmt -- --check
cargo check -p runtime
cargo check -p rusty-Himalaya-cli
cargo test -p runtime autonomous_integration --no-fail-fast
cargo test -p rusty-Himalaya-cli --test output_format_contract --no-fail-fast
cargo test -p rusty-Himalaya-cli --test stream_json_contract --no-fail-fast
```

Then run the read-only autonomous diagnostics in a real workspace:

```bash
Himalaya --output-format json tasks daemon report --limit 20 --max-ticks 3
Himalaya --output-format json tasks daemon evaluate --limit 20 --max-ticks 3
Himalaya --output-format json tasks daemon replay --limit 20 --max-ticks 3
```

## Release Gates

- no failed integration health blockers
- text output gives next action before detailed counters
- policy apply remains dry-run unless health is healthy
- stream-json schema remains backward compatible
- docs clearly separate stable and experimental autonomous behavior

`Himalaya maturity-matrix --output-format json` includes a `release_readiness` section with the same audit categories for tooling and CI consumers.
