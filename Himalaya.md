# Himalaya.md

This file provides guidance to Himalaya Code (Himalaya.ai/code) when working with code in this repository.

## Detected stack
- Languages: Rust.
- Frameworks: none detected from the supported starter markers.

## Verification
- Run Rust verification from `rust/`: `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- `src/` and `tests/` are both present; update both surfaces together when behavior changes.

## Repository shape
- `rust/` contains the Rust workspace and active CLI/runtime implementation.
- `src/` contains source files that should stay consistent with generated guidance and tests.
- `tests/` contains validation surfaces that should be reviewed alongside code changes.

## New Features: AI Reasoning Process Visualization

### Overview
Himalaya Code now supports real-time visualization of AI reasoning processes, similar to Claude's thinking display. This feature provides transparency into how the AI analyzes problems, makes decisions, and generates responses.

### Implementation Details
- **Stream Protocol Extension**: Added `reasoning_step` event type to the stream protocol
- **Reasoning Step Types**:
  - `analysis`: Problem analysis with confidence scores
  - `planning`: Solution planning with step breakdowns
  - `reflection`: Self-critique and adjustments
  - `decision`: Final choices with reasoning
- **Frontend Visualization**: JavaScript-based UI components for displaying reasoning steps
- **Rust Backend**: Event handling and message building for reasoning steps

### Demo
Run `reasoning-demo.html` in a browser to see the reasoning visualization in action.

### Files Modified
- `vscode-extension/src/streamProtocol.ts`: Extended stream protocol
- `vscode-extension/src/chatPanel.ts`: Added reasoning step display logic
- `rust/crates/runtime/src/conversation.rs`: Added reasoning step event handling
- `reasoning-demo.html`: Standalone demonstration

### Testing
- All existing tests pass
- New tests added for reasoning step processing
- Compilation successful for both TypeScript and Rust components
