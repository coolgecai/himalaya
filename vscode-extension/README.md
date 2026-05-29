# Himalaya Code VS Code extension

This extension wraps the Himalaya Code CLI with a chat panel, a sessions tree, local chat history, and command palette actions.

## Features

- Open a chat panel and send prompts to the CLI.
- Register a real VS Code chat participant, so `@himalaya` appears in the Chat / Agent list.
- Review local chat history, rename it, pin it, delete it, or resume from it.
- Inspect CLI session files from the workspace or your user profile, then open or resume them.
- Run `status`, `doctor`, `login`, and `logout` from VS Code.
- Point the extension at a specific binary or let it auto-detect the local build.

- Reasoning visualization: shows structured model reasoning steps (sent as `reasoning_step` stream events) inline in the chat panel. Disabled by default — toggle the feature with the magnifier button (🔎) in the chat panel top bar. The preference is persisted per workspace (workspaceState key: `himalayaCode.showReasoning.v1`). When disabled, the extension will not forward `reasoning_step` events to the webview. To enable by default when opening the panel programmatically, pass the `showReasoning` option in the chat launch options.

## Binary resolution

The extension looks for the binary in this order:

1. `himalayaCode.binaryPath`
2. `rust/target/release/Himalaya`
3. `rust/target/debug/Himalaya`
4. `Himalaya` on `PATH`

## Session recovery

The sidebar separates local chat history from CLI session files.

- Local history is stored in VS Code global state and survives editor restarts.
- CLI sessions are discovered from `.Himalaya/sessions/` in workspace roots and from `~/.Himalaya/sessions/`.
- `Resume` on a session entry focuses the panel on that session and uses its id as the resume target.

## Output channel

`himalayaCode.showOutputChannelByDefault` controls whether the output channel is revealed automatically when a command starts.

## Workspace trust

Prompt execution is blocked in untrusted workspaces unless `himalayaCode.allowUntrustedRuns` is enabled.

## Development

```bash
cd vscode-extension
npm install
npm run compile
```
