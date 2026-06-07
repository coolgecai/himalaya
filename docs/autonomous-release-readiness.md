# Autonomous Release Readiness

P8-A is a release-readiness audit for the autonomous runtime work completed through P7-D. It is a convergence checkpoint, not a new autonomous loop.

P9 extends that checkpoint into a final integration convergence pass: align the stream-json protocol with the VS Code extension, split autonomous CLI rendering from command execution, cover the end-to-end diagnostics user path, and normalize the operator-facing text layout. It still does not add a new autonomous loop.

## Stable Capabilities

- durable task registry and scheduler queue
- worker lifecycle supervision and recovery diagnostics
- task memory and route feedback stores
- routing proposal, dry-run apply, governed apply, and rollback plumbing
- policy governance ledger, lifecycle replay, adapter review, and apply coordination
- autonomous evaluation, trace replay, integration report, and health view
- daemon status/logs health checkpoints
- preflight gates for mutating autonomous operations
- text, JSON, and stream-json output contracts for daemon status, logs, report, and evaluation
- VS Code stream-json protocol validation for autonomous, policy, routing, and benchmark events
- operator-facing autonomous diagnostics with consistent `Next action`, `Safety`, `Guidance`, and `Policy` labels

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
cargo test -p rusty-Himalaya-cli --bin Himalaya --no-fail-fast
cargo test -p rusty-Himalaya-cli --test output_format_contract --no-fail-fast
cargo test -p rusty-Himalaya-cli --test stream_json_contract --no-fail-fast
```

Then verify the VS Code companion protocol and packaging checks:

```bash
cd vscode-extension
npm test
```

Then run the read-only autonomous diagnostics in a real workspace:

```bash
Himalaya --output-format json tasks daemon status
Himalaya --output-format json tasks daemon logs --limit 1
Himalaya --output-format json tasks daemon report --limit 20 --max-ticks 3
Himalaya --output-format json tasks daemon evaluate --limit 20 --max-ticks 3
Himalaya --output-format json tasks daemon replay --limit 20 --max-ticks 3
```

Use `Himalaya policy apply --dry-run` before any persistent governed policy apply. If a command returns `autonomous_preflight_blocked`, follow its `next_action` and re-run the read-only diagnostics before trying another mutating command.

## Release Gates

- no failed integration health blockers
- daemon status/logs/report/evaluate text output gives next action before detailed counters
- tasks daemon start is preflight-blocked unless health allows iteration
- policy apply remains dry-run or preflight-blocked unless health is healthy
- stream-json schema remains backward compatible
- VS Code `streamProtocol.ts` accepts every Rust-emitted autonomous event type
- text diagnostics keep the same field labels across status, logs, report, evaluate, and replay
- docs clearly separate stable and experimental autonomous behavior

`Himalaya maturity-matrix --output-format json` includes a `release_readiness` section with the same audit categories for tooling and CI consumers.

## P9 Release Checklist

- `tasks daemon status/logs/report/evaluate/replay` must all expose a clear next action in text output.
- JSON and stream-json outputs must keep health, policy recommendation, and runs path fields machine-readable.
- Mutating autonomous commands must still pass through the health preflight gate.
- The VS Code extension must reject malformed stream events while accepting all known event types from Rust tests.
- Generated or local-only artifacts should not be staged as part of release readiness unless they are intentional release outputs.
