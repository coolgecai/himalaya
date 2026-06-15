const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const Module = require('node:module');

const root = path.resolve(__dirname, '..');
const chatPanelPath = path.join(root, 'src', 'chatPanel.ts');
const chatParticipantPath = path.join(root, 'src', 'chatParticipant.ts');
const extensionPath = path.join(root, 'src', 'extension.ts');
const packageJsonPath = path.join(root, 'package.json');
const streamJsonSchemaPath = path.resolve(root, '..', 'protocol', 'stream-json-v1.schema.json');
const goldenStreamTranscriptPath = path.resolve(root, '..', 'protocol', 'stream-json-v1.golden.ndjson');
const streamJsonSchema = JSON.parse(fs.readFileSync(streamJsonSchemaPath, 'utf8'));
const historyPath = path.join(root, 'src', 'history.ts');
const vscodeignorePath = path.join(root, '.vscodeignore');
const vscodeignoreSource = fs.readFileSync(vscodeignorePath, 'utf8');
const permissionPolicyPath = path.join(root, 'out', 'permissionPolicy.js');
const streamProtocolPath = path.join(root, 'out', 'streamProtocol.js');
const streamProtocolSource = fs.readFileSync(path.join(root, 'src', 'streamProtocol.ts'), 'utf8');
const executionGatePath = path.join(root, 'out', 'executionGate.js');
const attachmentPathsPath = path.join(root, 'out', 'attachmentPaths.js');
const prepackageCheckPath = path.join(root, 'scripts', 'prepackage-check.cjs');
const prepackageCheckSource = fs.readFileSync(prepackageCheckPath, 'utf8');
const chatPanelSource = fs.readFileSync(chatPanelPath, 'utf8');
const chatParticipantSource = fs.readFileSync(chatParticipantPath, 'utf8');
const extensionSource = fs.readFileSync(extensionPath, 'utf8');
const historySource = fs.readFileSync(historyPath, 'utf8');
const cliSource = fs.readFileSync(path.join(root, 'src', 'cli.ts'), 'utf8');
const permissionPolicySource = fs.readFileSync(path.join(root, 'src', 'permissionPolicy.ts'), 'utf8');
const packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));

test('default permission mode aligns with CLI read-only default', () => {
  const props = packageJson.contributes?.configuration?.properties ?? {};
  const permissionMode = props['himalayaCode.defaultPermissionMode'];
  assert.ok(permissionMode, 'missing defaultPermissionMode setting');
  assert.equal(permissionMode.default, 'read-only');
  assert.deepEqual(permissionMode.enum, ['read-only', 'workspace-write', 'danger-full-access']);
  assert.match(permissionPolicySource, /DEFAULT_PERMISSION_MODE:\s*PublicPermissionMode\s*=\s*'read-only'/);
  assert.match(permissionPolicySource, /PUBLIC_PERMISSION_MODES:\s*readonly PublicPermissionMode\[\]/);
  assert.match(extensionSource, /normalizePermissionMode\(config\.get<string>\('defaultPermissionMode',\s*DEFAULT_PERMISSION_MODE\)\)/);
  assert.match(chatPanelSource, /normalizePermissionMode\(config\.get<string>\('defaultPermissionMode',\s*DEFAULT_PERMISSION_MODE\)\)/);
  assert.match(chatPanelSource, /normalizePermissionMode\(options\.permissionMode \?\? bootstrap\.config\?\.defaultPermissionMode\)/);
});

test('VSIX packaging keeps runtime entrypoints includable', () => {
  assert.equal(packageJson.main, './out/extension.js');
  assert.match(packageJson.scripts['vscode:prepublish'], /prepackage/);
  assert.match(packageJson.scripts['prepackage'], /test:regression/);
  assert.match(packageJson.scripts['prepackage'], /check:prepackage/);
  assert.match(packageJson.scripts['package:vsix'], /vsce package --no-dependencies/);
  assert.match(vscodeignoreSource, /^src\/\*\*$/m);
  assert.match(vscodeignoreSource, /^tests\/\*\*$/m);
  assert.match(vscodeignoreSource, /^rust\/\*\*$/m);
  assert.doesNotMatch(vscodeignoreSource, /^out\/\*\*$/m);
  assert.doesNotMatch(vscodeignoreSource, /^bin\/\*\*$/m);
  assert.doesNotMatch(vscodeignoreSource, /^media\/\*\*$/m);
  assert.doesNotMatch(vscodeignoreSource, /^package\.json$/m);
  assert.doesNotMatch(vscodeignoreSource, /^LICENSE$/m);
});

test('VSIX prepackage checks verify compiled output and staged binary', () => {
  assert.match(prepackageCheckSource, /requireFile\('out\/extension\.js', \{ nonEmpty: true \}\)/);
  assert.match(prepackageCheckSource, /requireFile\('out\/chatPanel\.js', \{ nonEmpty: true \}\)/);
  assert.match(prepackageCheckSource, /requireFile\('out\/streamProtocol\.js', \{ nonEmpty: true \}\)/);
  assert.match(prepackageCheckSource, /requireFile\('bin\/Himalaya-linux-x64', \{ executable: true, nonEmpty: true \}\)/);
  assert.match(prepackageCheckSource, /requireFile\('LICENSE', \{ nonEmpty: true \}\)/);
  assert.match(prepackageCheckSource, /packageJson\.main !== '\.\/out\/extension\.js'/);
});

test('danger confirmation policy config exists with expected enum', () => {
  const props = packageJson.contributes?.configuration?.properties ?? {};
  const policy = props['himalayaCode.dangerousPermissionConfirmationPolicy'];
  assert.ok(policy, 'missing dangerousPermissionConfirmationPolicy setting');
  assert.equal(policy.default, 'always');
  assert.deepEqual(policy.enum, ['always', 'once-per-workspace', 'never']);
});

test('danger confirmation helper and workspace memory key exist', () => {
  assert.match(chatPanelSource, /private\s+readonly\s+dangerApprovalKeyPrefix\s*=\s*'himalayaCode\.dangerApproval\.v1'/);
  assert.match(chatPanelSource, /private\s+async\s+confirmPermissionForRun\(permissionMode:\s*string,\s*prompt:\s*string\):\s*Promise<boolean>/);
  assert.match(chatPanelSource, /getDangerConfirmationPolicy\(\):\s*'always'\s*\|\s*'once-per-workspace'\s*\|\s*'never'/);
  assert.match(chatPanelSource, /workspaceDangerApprovalKey\(\):\s*string/);
});

test('submission path uses execution gate to protect run continuation', () => {
  assert.match(chatPanelSource, /await executeWithPermissionGate\(\{/);
  assert.match(chatPanelSource, /confirmDangerousRun:\s*\(\)\s*=>\s*this\.confirmPermissionForRun\(permissionMode,\s*prompt\)/);
  assert.match(chatPanelSource, /if \(gateBlocked\)\s*\{/);
  assert.match(chatPanelSource, /Execution cancelled before run\./);
});

test('danger permission args are no longer hardcoded to skip permissions', () => {
  assert.match(chatPanelSource, /const args:\s*string\[\]\s*=\s*\['--output-format',\s*'stream-json'\]/);
  assert.match(chatPanelSource, /if \(this\.shouldAllowBroadCwd\(cwd\)\) \{\s*args\.push\('--allow-broad-cwd'\);\s*\}/s);
  assert.match(chatPanelSource, /allowBroadCwd:\s*this\.shouldAllowBroadCwd\(input\.cwd\)/);
  assert.match(chatPanelSource, /private\s+shouldAllowBroadCwd\(cwd\?:\s*string\):\s*boolean/);
  assert.doesNotMatch(chatPanelSource, /const args:\s*string\[\]\s*=\s*\[[^\]]*'--allow-broad-cwd'/);
  assert.doesNotMatch(chatPanelSource, /--dangerously-skip-permissions/);
  assert.doesNotMatch(chatPanelSource, /'--permission-mode',\s*'danger-full-access'/);
});

test('configure model workflow remains present', () => {
  assert.match(chatPanelSource, /async\s+openModelConfigurationWizard\(\):\s*Promise<void>/);
  assert.match(chatPanelSource, /private\s+async\s+configureCloudModelRoute\(\):\s*Promise<void>/);
  assert.match(chatPanelSource, /private\s+async\s+configureLocalModelRoute\(\):\s*Promise<void>/);
  assert.match(chatPanelSource, /await\s+writeModelRoute\(this\.context,\s*\{/);
});

test('webview doctor/status command path is wired', () => {
  assert.match(chatPanelSource, /typedMessage\.command === 'doctor' \|\| typedMessage\.command === 'status'/);
  assert.match(chatPanelSource, /executeUtilityCommand\(typedMessage\.command\)/);
  assert.match(chatPanelSource, /private\s+async\s+executeUtilityCommand\(command:\s*'doctor'\s*\|\s*'status'\):\s*Promise<void>/);
});

test('chat submissions prefer reusable repl worker with one-shot fallback', () => {
  assert.match(chatPanelSource, /private\s+async\s+ensureReplWorker\(/);
  assert.match(chatPanelSource, /const handle = await this\.ensureReplWorker\(/);
  assert.match(chatPanelSource, /const args = this\.buildPromptArgs\(cliPrompt, model, permissionMode, resumeTarget, cwd, attachmentPaths\)/);
  assert.match(chatPanelSource, /handle\.send\(JSON\.stringify\(\{ type: 'prompt', text: cliPrompt, files: attachmentPaths \}\)\)/);
  assert.match(chatPanelSource, /REPL worker unavailable; falling back to one-shot execution\./);
  assert.match(chatPanelSource, /const result = await this\.cli\.run\(args, \{/);
  assert.match(chatPanelSource, /if \(result && result\.exitCode !== 0\) \{/);
  assert.match(chatPanelSource, /this\.host\.webview\.postMessage\(\{ type: 'assistantDone' \}\)/);
});

test('chat submissions default cli cwd to the VS Code workspace', () => {
  assert.match(chatPanelSource, /const workspaceFolder = vscode\.workspace\.workspaceFolders\?\.\[0\]\?\.uri\.fsPath/);
  assert.match(chatPanelSource, /const cwd = input\.cwd\?\.trim\(\) \|\| workspaceFolder/);
  assert.match(chatPanelSource, /prepareAttachmentDescriptors\(files, cwd, 'picker'\)/);
  assert.match(chatPanelSource, /const result = await this\.cli\.run\(args, \{\s*cwd,/s);
});

test('compiled chat webview inline script is valid JavaScript', () => {
  const oldLoad = Module._load;
  Module._load = (request, parent, isMain) => {
    if (request === 'vscode') {
      return {
        workspace: {
          workspaceFolders: [{ uri: { fsPath: root } }],
          isTrusted: true,
          getConfiguration: () => ({ get: (_key, defaultValue) => defaultValue }),
        },
        window: {
          ViewColumn: { Beside: 2 },
          showOpenDialog: async () => [],
          showWarningMessage: async () => undefined,
          showErrorMessage: async () => undefined,
          showInformationMessage: async () => undefined,
        },
        Disposable: class {},
        EventEmitter: class { constructor() { this.event = () => undefined; } fire() {} },
        TreeItem: class {},
        TreeItemCollapsibleState: { None: 0 },
        commands: { executeCommand: async () => undefined },
        Uri: {
          file: (filePath) => ({ fsPath: filePath }),
          joinPath: (base, ...segments) => ({ fsPath: [base && base.fsPath, ...segments].filter(Boolean).join('/') }),
        },
      };
    }
    return oldLoad(request, parent, isMain);
  };

  try {
    const { HimalayaChatPanel } = require(path.join(root, 'out', 'chatPanel.js'));
    const host = {
      webview: {
        onDidReceiveMessage: () => ({ dispose() {} }),
        postMessage: () => undefined,
        cspSource: 'vscode-resource',
        asWebviewUri: (uri) => ({ toString: () => 'vscode-resource://' + ((uri && uri.fsPath) || '') }),
      },
      onDidDispose: () => ({ dispose() {} }),
    };
    const context = {
      workspaceState: { get: () => undefined, update: async () => undefined },
      globalState: { get: () => undefined, update: async () => undefined },
      extensionUri: { fsPath: root },
    };
    const bootstrap = {
      trust: true,
      config: {
        defaultModel: 'sonnet',
        defaultPermissionMode: 'read-only',
        defaultModelBackend: 'auto',
        ollamaBaseUrl: '',
        allowUntrustedRuns: false,
        binaryPath: '',
      },
      history: { records: [], activeRecordId: null },
      modelCatalog: { recentModels: [], localModels: [] },
      sessions: { groups: [] },
      identity: {},
    };
    const panel = new HimalayaChatPanel(context, {}, { appendLine() {} }, {}, host, bootstrap);
    const html = panel.buildClaudeLikeHtml(host.webview, bootstrap, {});
    // Skip external <script src> tags (e.g. the bundled marked loader) and grab the inline body.
    const script = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g)]
      .map((m) => m[1])
      .find((body) => body && body.trim().length > 0);
    assert.ok(script, 'chat webview script should be present');
    assert.doesNotThrow(() => new vm.Script(script, { filename: 'chat-webview-inline.js' }));
  } finally {
    Module._load = oldLoad;
  }
});

test('chat panel replays stream-json transcripts through webview messages', async () => {
  const vscodeMock = {
    workspace: {
      workspaceFolders: [{ uri: { fsPath: root } }],
      isTrusted: true,
      getConfiguration: () => ({ get: (_key, defaultValue) => defaultValue }),
    },
    window: {
      ViewColumn: { Beside: 2 },
      showOpenDialog: async () => [],
      showWarningMessage: async () => undefined,
      showErrorMessage: async () => undefined,
      showInformationMessage: async () => undefined,
    },
    Disposable: class {},
    EventEmitter: class { constructor() { this.event = () => undefined; } fire() {} },
    TreeItem: class {},
    TreeItemCollapsibleState: { None: 0 },
    commands: { executeCommand: async () => undefined },
    Uri: {
      file: (filePath) => ({ fsPath: filePath }),
      joinPath: (base, ...segments) => ({ fsPath: [base && base.fsPath, ...segments].filter(Boolean).join('/') }),
    },
    ViewColumn: { Beside: 2 },
  };

  const oldLoad = Module._load;
  Module._load = (request, parent, isMain) => {
    if (request === 'vscode') {
      return vscodeMock;
    }
    return oldLoad(request, parent, isMain);
  };

  try {
    for (const modulePath of [
      path.join(root, 'out', 'chatPanel.js'),
      path.join(root, 'out', 'cli.js'),
      path.join(root, 'out', 'modelRoute.js'),
    ]) {
      delete require.cache[require.resolve(modulePath)];
    }

    const { HimalayaChatPanel } = require(path.join(root, 'out', 'chatPanel.js'));
    const contextState = new Map();
    const context = {
      workspaceState: {
        get: (key, defaultValue) => contextState.has(key) ? contextState.get(key) : defaultValue,
        update: async (key, value) => { contextState.set(key, value); },
      },
      globalState: {
        get: (_key, defaultValue) => defaultValue,
        update: async () => undefined,
      },
      secrets: {
        get: async () => undefined,
        store: async () => undefined,
        delete: async () => undefined,
      },
      extensionUri: { fsPath: root },
      extensionPath: root,
    };
    const records = [];
    const history = {
      records: () => records,
      createDraft: async (input) => {
        const record = {
          id: `record-${records.length + 1}`,
          title: input.title,
          createdAt: Date.now(),
          updatedAt: Date.now(),
          pinned: false,
          model: input.model,
          modelBackend: input.modelBackend,
          permissionMode: input.permissionMode,
          resumeTarget: input.resumeTarget,
          cwd: input.cwd,
          messages: [],
          recoveryEvidence: [],
        };
        records.push(record);
        return record;
      },
      setActiveRecord: async () => undefined,
      upsert: async (record) => {
        const index = records.findIndex((item) => item.id === record.id);
        if (index >= 0) {
          records[index] = record;
        } else {
          records.push(record);
        }
      },
      appendMessage: async (recordId, message) => {
        const record = records.find((item) => item.id === recordId);
        if (record) {
          record.messages.push(message);
        }
      },
      replaceAssistantTail: async (recordId, text) => {
        const record = records.find((item) => item.id === recordId);
        const tail = record?.messages[record.messages.length - 1];
        if (tail && tail.role === 'assistant') {
          tail.text = text;
        }
      },
      appendRecoveryEvidence: async (recordId, evidence) => {
        const record = records.find((item) => item.id === recordId);
        if (record) {
          record.recoveryEvidence.push(evidence);
        }
      },
    };
    const posted = [];
    const host = {
      webview: {
        onDidReceiveMessage: () => ({ dispose() {} }),
        postMessage: (message) => { posted.push(message); return Promise.resolve(true); },
        cspSource: 'vscode-resource',
        asWebviewUri: (uri) => ({ toString: () => 'vscode-resource://' + ((uri && uri.fsPath) || '') }),
      },
      onDidDispose: () => ({ dispose() {} }),
    };
    const outputLines = [];
    const transcript = fs.readFileSync(goldenStreamTranscriptPath, 'utf8');
    const runCalls = [];
    const cli = {
      run: async (args, options) => {
        runCalls.push(args);
        for (const chunk of [transcript.slice(0, 73), transcript.slice(73, 211), transcript.slice(211)]) {
          options.onStdout(chunk);
        }
        return { exitCode: 0, stdout: transcript, stderr: '' };
      },
      startRepl: async () => { throw new Error('unexpected repl start'); },
    };
    const bootstrap = {
      trust: true,
      config: {
        defaultModel: 'sonnet',
        defaultPermissionMode: 'read-only',
        defaultModelBackend: 'auto',
        ollamaBaseUrl: '',
        allowUntrustedRuns: false,
        binaryPath: '',
      },
      history: { records: [], activeRecordId: null },
      modelCatalog: { recentModels: [], localModels: [] },
      sessions: { groups: [] },
      identity: {},
    };

    const panel = new HimalayaChatPanel(
      context,
      cli,
      { appendLine: (line) => { outputLines.push(line); } },
      history,
      host,
      bootstrap,
    );
    panel.initialize({ showReasoning: true });
    panel.replCanReuse = false;

    await panel.executePromptSubmission({ prompt: 'Replay stream', model: 'sonnet', permissionMode: 'read-only', cwd: root });

    const runtimeEvents = posted.filter((message) => message.type === 'runtimeEvent');
    const runtimeKinds = new Set(runtimeEvents.map((message) => message.kind));

    assert.deepEqual(runCalls[0].slice(0, 2), ['--output-format', 'stream-json']);
    assert.equal(posted.find((message) => message.type === 'sessionMeta').sessionId, 'golden-session');
    assert.deepEqual(posted.filter((message) => message.type === 'assistantChunk').map((message) => message.text), ['Golden ', 'replay']);
    assert.equal(posted.find((message) => message.type === 'reasoningStep').step.content, 'plan golden replay');
    assert.equal(posted.find((message) => message.type === 'toolStep' && message.step === 'use').name, 'read_file');
    assert.equal(posted.find((message) => message.type === 'toolStep' && message.step === 'result').output, 'fixture ok');
    assert.equal(posted.find((message) => message.type === 'decisioningEvent').event.kind, 'task_decomposition');
    for (const kind of [
      'plan_execution_event',
      'task_ledger_event',
      'model_route_event',
      'team_execution_event',
      'recovery_event',
      'recovery_action_event',
      'task_execution_event',
      'task_scheduler_daemon_run',
      'task_scheduler_daemon_status',
      'task_scheduler_daemon_logs',
      'route_feedback_summary',
      'benchmark_suite',
      'benchmark_task',
      'benchmark_run',
      'worker_spawn',
      'worker_supervisor_tick',
    ]) {
      assert.equal(runtimeKinds.has(kind), true, `${kind} should replay into the webview`);
    }
    assert.equal(runtimeEvents.find((message) => message.kind === 'route_feedback_summary').event.feedback_count, 2);
    assert.equal(runtimeEvents.find((message) => message.kind === 'benchmark_run').event.run.summary.total_tasks, 1);
    assert.equal(runtimeEvents.find((message) => message.kind === 'worker_spawn').event.worker.process.pid, 123);
    assert.equal(runtimeEvents.find((message) => message.kind === 'worker_supervisor_tick').event.tick.active_workers, 1);
    const recoveryMessage = posted.find((message) => message.type === 'recoverySuggestion');
    assert.equal(recoveryMessage.failureClass, 'trust_gate');
    assert.equal(posted.find((message) => message.type === 'permissionRequest').requiredMode, 'workspace-write');
    assert.equal(posted.at(-1).type, 'assistantDone');
    assert.equal(records[0].resumeTarget, 'golden-session');
    assert.equal(records[0].messages.at(-1).text, 'Golden replay');
    assert.equal(records[0].recoveryEvidence.find((e) => e.failureClass === 'trust_gate')?.failureClass, 'trust_gate');
    assert.equal(outputLines.length, 0);
  } finally {
    Module._load = oldLoad;
  }
});
test('task board surfaces worker supervisor state', () => {
  assert.match(chatPanelSource, /workerSupervisor:\s*null/);
  assert.match(chatPanelSource, /selectedWorkerId:\s*null/);
  assert.match(chatPanelSource, /function rememberTaskBoardWorkerSupervisor\(tick\)/);
  assert.match(chatPanelSource, /function renderTaskBoardWorkerSupervisor\(\)/);
  assert.match(chatPanelSource, /function renderTaskBoardWorkerDetail\(worker\)/);
  assert.match(chatPanelSource, /task-board-worker-detail-grid/);
  assert.match(chatPanelSource, /renderTaskBoardOperations/);
  assert.match(chatPanelSource, /Scheduler daemon/);
  assert.match(chatPanelSource, /Route feedback/);
  assert.match(chatPanelSource, /Benchmark/);
  assert.match(chatPanelSource, /data-worker-id/);
  assert.match(chatPanelSource, /worker\.process/);
  assert.match(chatPanelSource, /worker\.isolation/);
  assert.match(chatPanelSource, /Worker Detail/);
  assert.match(chatPanelSource, /kind === 'worker_supervisor_tick'/);
  assert.match(chatPanelSource, /Live task status, worker health, active node, and recovery timeline/);
});

test('task board is collapsible to preserve chat space', () => {
  assert.match(chatPanelSource, /collapsed:\s*true/);
  assert.match(chatPanelSource, /task-board-toggle/);
  assert.match(chatPanelSource, /data-task-board-toggle="1"/);
  assert.match(chatPanelSource, /taskBoardSurface\.classList\.toggle\('collapsed', collapsed\)/);
  assert.match(chatPanelSource, /state\.taskBoard\.collapsed = !state\.taskBoard\.collapsed/);
  assert.match(chatPanelSource, /class="task-board-body"/);
  assert.match(chatPanelSource, /aria-expanded/);
  assert.match(chatPanelSource, /task-board-summary/);
});

test('context auto-compaction is surfaced as a notice in the thread', () => {
  // Extension host forwards the runtime context_event to the webview.
  assert.match(chatPanelSource, /case 'context_event':/);
  assert.match(chatPanelSource, /kind === 'context_compact'/);
  assert.match(chatPanelSource, /type:\s*'contextCompacted'/);
  // Webview renders a notice bubble for the compaction.
  assert.match(chatPanelSource, /case 'contextCompacted':/);
  assert.match(chatPanelSource, /addBubble\('notice'/);
  assert.match(chatPanelSource, /Context auto-compacted/);
  assert.match(chatPanelSource, /role === 'notice'\s*\?\s*'msg notice'/);
});
test('assistant placeholder message is created before streaming chunks', () => {
  assert.match(chatPanelSource, /await this\.history\.appendMessage\(record\.id,\s*\{\s*role:\s*'assistant',\s*text:\s*''/s);
  assert.match(chatPanelSource, /createAssistantTailPersistence\(record\.id, \(\) => assistantText\)/);
  assert.match(chatPanelSource, /assistantTailPersistence\.flush\(\)/);
});

test('bootstrap config passes dangerous confirmation policy', () => {
  assert.match(extensionSource, /dangerousPermissionConfirmationPolicy:\s*config\.get<string>\('dangerousPermissionConfirmationPolicy',\s*'always'\)/);
});

test('permission policy helpers behave as expected', async () => {
  const {
    normalizeDangerousPermissionConfirmationPolicy,
    buildWorkspaceDangerApprovalKey,
    shouldAutoAllowDangerRun,
  } = require(permissionPolicyPath);

  assert.equal(normalizeDangerousPermissionConfirmationPolicy('always'), 'always');
  assert.equal(normalizeDangerousPermissionConfirmationPolicy('once-per-workspace'), 'once-per-workspace');
  assert.equal(normalizeDangerousPermissionConfirmationPolicy('never'), 'never');
  assert.equal(normalizeDangerousPermissionConfirmationPolicy('unexpected'), 'always');

  assert.equal(buildWorkspaceDangerApprovalKey([], 'k'), 'k:no-workspace');
  assert.equal(
    buildWorkspaceDangerApprovalKey(['/b', '/a'], 'k'),
    'k:/a|/b',
  );

  assert.equal(shouldAutoAllowDangerRun('never', false), true);
  assert.equal(shouldAutoAllowDangerRun('once-per-workspace', true), true);
  assert.equal(shouldAutoAllowDangerRun('once-per-workspace', false), false);
  assert.equal(shouldAutoAllowDangerRun('always', true), false);
});

test('stream protocol helpers behave as expected', async () => {
  const {
    STREAM_PROTOCOL_VERSION,
    KNOWN_STREAM_EVENT_TYPES,
    parseStreamEventLine,
    isKnownStreamEventType,
    readProtocolVersion,
    classifyProtocolVersion,
  } = require(streamProtocolPath);

  assert.equal(streamJsonSchema.properties.protocol_version.const, STREAM_PROTOCOL_VERSION);
  assert.equal(streamJsonSchema.required.includes('type'), true);
  assert.equal(streamJsonSchema.required.includes('protocol_version'), true);
  assert.equal(streamJsonSchema.$defs.baseEvent.required.includes('type'), true);
  assert.equal(streamJsonSchema.$defs.baseEvent.required.includes('protocol_version'), true);
  assert.deepEqual(streamJsonSchema.$defs.eventType.enum, KNOWN_STREAM_EVENT_TYPES);
  const schemaBranchTypes = new Set();
  for (const branch of streamJsonSchema.allOf[0].oneOf) {
    const defName = branch.$ref.replace('#/$defs/', '');
    const definition = streamJsonSchema.$defs[defName];
    const typeSchema = definition.allOf[1].properties.type;
    if (typeSchema.const) {
      schemaBranchTypes.add(typeSchema.const);
    }
    if (typeSchema.enum) {
      for (const type of typeSchema.enum) {
        schemaBranchTypes.add(type);
      }
    }
  }
  assert.deepEqual([...schemaBranchTypes].sort(), [...KNOWN_STREAM_EVENT_TYPES].sort());

  const fixtures = [
    ['message_start', '{"type":"message_start","protocol_version":1}'],
    ['message_stop', '{"type":"message_stop","protocol_version":1}'],
    ['text_delta', '{"type":"text_delta","text":"hi","protocol_version":1}'],
    ['tool_use', '{"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"fixture.txt"},"protocol_version":1}'],
    ['tool_result', '{"type":"tool_result","id":"toolu_1","name":"read_file","output":"ok","is_error":false,"protocol_version":1}'],
    ['done', '{"type":"done","iterations":1,"protocol_version":1}'],
    ['session_meta', '{"type":"session_meta","session_id":"session-1","session_path":"/tmp/session.jsonl","model":"sonnet","protocol_version":1}'],
    ['command_match', '{"type":"command_match","command":"doctor","protocol_version":1}'],
    ['tool_match', '{"type":"tool_match","tool":"read_file","protocol_version":1}'],
    ['permission_request', '{"type":"permission_request","tool":"write_file","input":"{}","current_mode":"read-only","required_mode":"workspace-write","reason":"requires workspace-write","protocol_version":1}'],
    ['permission_denial', '{"type":"permission_denial","tool":"write_file","reason":"requires workspace-write","protocol_version":1}'],
    ['reasoning_step', '{"type":"reasoning_step","reasoning_step":{"step_type":"analysis","content":"thinking","signature":"sig"},"protocol_version":1}'],
    ['reasoning_step', '{"type":"reasoning_step","reasoning_step":{"step_type":"redacted_thinking","data":{"opaque":"redacted"}},"protocol_version":1}'],
    ['decisioning_event', '{"type":"decisioning_event","decisioning_event":{"kind":"tool_selection","title":"Tool selection","summary":"Selected 1 tool.","selected_tools":["read_file"],"tool_scores":[{"name":"read_file","score":0.9,"success_rate":0.8,"latency_ms":60,"cost":0.05,"parallelizable":true,"capabilities":["read"],"selected":true}]},"protocol_version":1}'],
    ['decisioning_event', '{"type":"decisioning_event","decisioning_event":{"kind":"task_decomposition","title":"Task decomposition","summary":"Split into 2 steps.","task_id":"task-1","plan_dag":{"task_id":"task-1","root_id":"task-1","nodes":[{"kind":"task","id":"task-1","title":"Do work","parallelizable":false,"estimated_effort":2,"candidate_tools":["read_file"],"notes":[]},{"kind":"step","id":"task-1-analyze","title":"Analyze","parallelizable":false,"estimated_effort":1,"candidate_tools":["read_file"],"notes":[]}],"edges":[{"from":"task-1","to":"task-1-analyze","kind":"contains"}]},"details":[]},"protocol_version":1}'],
    ['plan_execution_event', '{"type":"plan_execution_event","plan_execution_event":{"seq":1,"task_id":"task-1","node_id":"task-1-analyze","kind":"node_started","status":"running","message":"started","attempt":2,"blocking_reason":"permission","verification_gate":"targeted"},"protocol_version":1}'],
    ['task_ledger_event', '{"type":"task_ledger_event","task_ledger_event":{"seq":2,"task_id":"task-1","event":"status","status":"running","message":"working","timestamp":123},"protocol_version":1}'],
    ['model_route_event', '{"type":"model_route_event","model_route_event":{"phase":"verification","model":"opus","provider":"anthropic","reason":"selected verifier","confidence":0.85,"fallback_model":"sonnet"},"protocol_version":1}'],
    ['team_execution_event', '{"type":"team_execution_event","team_execution_event":{"seq":3,"team_id":"team-1","task_id":"task-1","role":"verifier","kind":"verification_passed","model_route":{"phase":"verification","model":"opus","provider":"anthropic","reason":"selected verifier"},"message":"passed"},"protocol_version":1}'],
    ['recovery_event', '{"type":"recovery_event","recovery_event":{"recovery_attempted":{"scenario":"compile_red_cross_crate","recipe":{"scenario":"compile_red_cross_crate","steps":["clean_build"],"max_attempts":1,"escalation_policy":"alert_human"},"result":{"recovered":{"steps_taken":1}}}},"protocol_version":1}'],
    ['recovery_action_event', '{"type":"recovery_action_event","recovery_action_event":{"task_id":"task-1","results":[{"action":{"kind":"retry_node","scenario":"prompt_misdelivery","risk":"safe","node_id":"node-1","message":"retry"},"executed":true,"blocked":false,"reason":"scheduled"}]},"protocol_version":1}'],
    ['task_execution_event', '{"type":"task_execution_event","task_execution_event":{"task_id":"task-1","steps":[{"task_id":"task-1","node_id":"node-1","kind":"resume_node","message":"resumed"}],"completed":true,"blocked":false,"message":"done"},"protocol_version":1}'],
    ['local_command', '{"type":"local_command","command":"workspace","status":"ok","summary":"workspace context loaded","protocol_version":1}'],
    ['task_execution', '{"type":"task_execution","outcome":{"task_id":"task-1","steps":[{"task_id":"task-1","node_id":"node-1","kind":"resume_node","message":"resumed"}],"completed":true,"blocked":false,"message":"done"},"protocol_version":1}'],
    ['task_recovery', '{"type":"task_recovery","execution":{"task_id":"task-1","results":[{"action":{"kind":"retry_node","scenario":"prompt_misdelivery","risk":"safe","node_id":"node-1","message":"retry"},"executed":true,"blocked":false,"reason":"scheduled"}]},"protocol_version":1}'],
    ['task_packet_create', '{"type":"task_packet_create","task":{"task_id":"task-1"},"ledger":[],"verification_handoff":{},"protocol_version":1}'],
    ['task_packet_run', '{"type":"task_packet_run","task":{"task_id":"task-1"},"ledger":[],"verification_handoff":{},"protocol_version":1}'],
    ['task_packet_status', '{"type":"task_packet_status","task":{"task_id":"task-1"},"ledger":[],"verification_handoff":{},"protocol_version":1}'],
    ['task_scheduler_tick', '{"type":"task_scheduler_tick","tick":{"status":"blocked","selected_task_id":"task-1","queue":[]},"protocol_version":1}'],
    ['task_scheduler_queue', '{"type":"task_scheduler_queue","queue":[{"task_id":"task-1","status":"pending","task_status":"created"}],"protocol_version":1}'],
    ['task_scheduler_explain', '{"type":"task_scheduler_explain","explanation":{"task_id":"task-1","selected":true},"protocol_version":1}'],
    ['task_scheduler_daemon_run', '{"type":"task_scheduler_daemon_run","runs":[],"state":null,"protocol_version":1}'],
    ['task_scheduler_daemon_status', '{"type":"task_scheduler_daemon_status","state":null,"state_path":"/tmp/state.json","events_path":"/tmp/events.jsonl","protocol_version":1}'],
    ['task_scheduler_daemon_logs', '{"type":"task_scheduler_daemon_logs","events":[],"events_path":"/tmp/events.jsonl","health":{"status":"degraded","next_action":"Run report"},"protocol_version":1}'],
    ['task_scheduler_daemon_report', '{"type":"task_scheduler_daemon_report","summary":{},"policy_recommendation":{},"integration":{},"health":{},"runs_path":"/tmp/runs.jsonl","protocol_version":1}'],
    ['task_scheduler_daemon_evaluation', '{"type":"task_scheduler_daemon_evaluation","evaluation":{},"runs_path":"/tmp/runs.jsonl","protocol_version":1}'],
    ['task_scheduler_daemon_replay', '{"type":"task_scheduler_daemon_replay","replay":{},"runs_path":"/tmp/runs.jsonl","protocol_version":1}'],
    ['autonomous_preflight_blocked', '{"type":"autonomous_preflight_blocked","operation":"policy apply","next_action":"Run dry-run","health":{},"protocol_version":1}'],
    ['cron_list', '{"type":"cron_list","crons":[],"protocol_version":1}'],
    ['cron_create', '{"type":"cron_create","cron_id":"cron-1","protocol_version":1}'],
    ['cron_delete', '{"type":"cron_delete","cron_id":"cron-1","protocol_version":1}'],
    ['cron_fired', '{"type":"cron_fired","cron_id":"cron-1","task_id":"task-1","protocol_version":1}'],
    ['cron_fire_failed', '{"type":"cron_fire_failed","cron_id":"cron-1","reason":"not due","protocol_version":1}'],
    ['cron_run', '{"type":"cron_run","summary":{},"created_tasks":[],"failed":[],"scheduler":{},"protocol_version":1}'],
    ['route_feedback_summary', '{"type":"route_feedback_summary","summaries":[],"feedback_count":0,"feedback_path":"/tmp/feedback.json","protocol_version":1}'],
    ['route_optimizer_report', '{"type":"route_optimizer_report","report":{},"protocol_version":1}'],
    ['route_optimizer_replay', '{"type":"route_optimizer_replay","replay":{},"protocol_version":1}'],
    ['route_policy_proposal', '{"type":"route_policy_proposal","proposal":{},"protocol_version":1}'],
    ['route_policy_apply', '{"type":"route_policy_apply","apply":{},"protocol_version":1}'],
    ['route_policy_rollback', '{"type":"route_policy_rollback","rollback":{},"protocol_version":1}'],
    ['route_policy_list', '{"type":"route_policy_list","proposals":[],"protocol_version":1}'],
    ['policy_review', '{"type":"policy_review","review":{},"protocol_version":1}'],
    ['policy_apply_plan', '{"type":"policy_apply_plan","plan":{},"protocol_version":1}'],
    ['policy_apply', '{"type":"policy_apply","apply":{},"protocol_version":1}'],
    ['policy_rollback', '{"type":"policy_rollback","rollback":{},"protocol_version":1}'],
    ['policy_ledger', '{"type":"policy_ledger","ledger":{},"protocol_version":1}'],
    ['policy_replay', '{"type":"policy_replay","replay":{},"protocol_version":1}'],
    ['benchmark_suite', '{"type":"benchmark_suite","suite_id":"complex-coding-agent-v1","version":"2026.05","tasks":[],"protocol_version":1}'],
    ['benchmark_task', '{"type":"benchmark_task","task":{"id":"task-1","title":"Task"},"protocol_version":1}'],
    ['benchmark_run', '{"type":"benchmark_run","run":{"summary":{"total_tasks":10}},"protocol_version":1}'],
    ['benchmark_autonomous', '{"type":"benchmark_autonomous","run":{"suite_id":"autonomous-agent-loop-v1"},"protocol_version":1}'],
    ['worker_list', '{"type":"worker_list","workers":[],"protocol_version":1}'],
    ['worker_create', '{"type":"worker_create","worker":{"worker_id":"worker-1","status":"spawning"},"protocol_version":1}'],
    ['worker_spawn', '{"type":"worker_spawn","worker":{"worker_id":"worker-1","status":"running","process":{"pid":123,"command":["sh","-c","exit 0"],"started_at":123}},"protocol_version":1}'],
    ['worker_probe', '{"type":"worker_probe","worker":{"worker_id":"worker-1","status":"finished"},"protocol_version":1}'],
    ['worker_observe', '{"type":"worker_observe","worker":{"worker_id":"worker-1","status":"ready_for_prompt"},"protocol_version":1}'],
    ['worker_ready', '{"type":"worker_ready","ready":{"worker_id":"worker-1","ready":true},"protocol_version":1}'],
    ['worker_resolve_trust', '{"type":"worker_resolve_trust","worker":{"worker_id":"worker-1","status":"ready_for_prompt"},"protocol_version":1}'],
    ['worker_prompt', '{"type":"worker_prompt","worker":{"worker_id":"worker-1","status":"prompt_accepted"},"protocol_version":1}'],
    ['worker_complete', '{"type":"worker_complete","worker":{"worker_id":"worker-1","status":"finished"},"protocol_version":1}'],
    ['worker_restart', '{"type":"worker_restart","worker":{"worker_id":"worker-1","status":"spawning"},"protocol_version":1}'],
    ['worker_terminate', '{"type":"worker_terminate","worker":{"worker_id":"worker-1","status":"failed"},"protocol_version":1}'],
    ['worker_supervisor_tick', '{"type":"worker_supervisor_tick","tick":{"status":"running","active_workers":1},"protocol_version":1}'],
    ['user_question', '{"type":"user_question","question":"Pick one","options":["a","b"],"protocol_version":1}'],
  ];

  for (const [type, line] of fixtures) {
    const parsed = parseStreamEventLine(line);
    assert.equal(parsed.ok, true, `${type} fixture should parse`);
    assert.equal(parsed.event.type, type);
    assert.equal(readProtocolVersion(parsed.event), STREAM_PROTOCOL_VERSION);
    assert.equal(isKnownStreamEventType(parsed.event.type), true);
  }

  // Regression: events with explicit null optional fields (provider, message,
  // fallback_model) must NOT be rejected as malformed. The CLI legitimately
  // emits provider:null / message:null / fallback_model:null.
  const nullFieldFixtures = [
    '{"type":"task_ledger_event","task_ledger_event":{"seq":30,"task_id":"task_x","event":"status_changed","status":"running","message":null,"timestamp":1780576450000},"protocol_version":1}',
    '{"type":"model_route_event","model_route_event":{"phase":"coding","model":"qwen3.6:35b","provider":null,"reason":"selected coding","confidence":0.95,"fallback_model":"qwen3.6:35b"},"protocol_version":1}',
  ];
  for (const line of nullFieldFixtures) {
    const parsed = parseStreamEventLine(line);
    assert.equal(parsed.ok, true, `null optional fields should parse: ${line.slice(0, 60)}`);
  }

  const ok = parseStreamEventLine('{"type":"text_delta","text":"hi","protocol_version":1}');
  assert.equal(ok.ok, true);
  assert.equal(ok.event.type, 'text_delta');
  assert.equal(ok.event.text, 'hi');
  assert.equal(readProtocolVersion(ok.event), STREAM_PROTOCOL_VERSION);

  const sessionMeta = parseStreamEventLine('{"type":"session_meta","session_id":"session-1","session_path":"/tmp/session.jsonl","model":"sonnet","protocol_version":1}');
  assert.equal(sessionMeta.ok, true);
  assert.equal(sessionMeta.event.session_id, 'session-1');
  assert.equal(sessionMeta.event.session_path, '/tmp/session.jsonl');
  assert.equal(sessionMeta.event.model, 'sonnet');
  assert.equal(isKnownStreamEventType('session_meta'), true);

  const invalidKnownShapes = [
    '{"type":"text_delta","protocol_version":1}',
    '{"type":"tool_use","id":"toolu_1","name":"read_file","protocol_version":1}',
    '{"type":"tool_result","name":"read_file","output":"ok","is_error":"false","protocol_version":1}',
    '{"type":"done","iterations":"1","protocol_version":1}',
    '{"type":"session_meta","session_id":"session-1","protocol_version":1}',
    '{"type":"permission_request","tool":"write_file","current_mode":"read-only","required_mode":"workspace-write","reason":"requires workspace-write","protocol_version":1}',
    '{"type":"reasoning_step","reasoning_step":{"step_type":"redacted_thinking"},"protocol_version":1}',
    '{"type":"decisioning_event","decisioning_event":{"kind":"tool_selection","summary":"missing title"},"protocol_version":1}',
    '{"type":"plan_execution_event","plan_execution_event":{"seq":"1","task_id":"task-1","node_id":"node-1","kind":"node_started","status":"running"},"protocol_version":1}',
    '{"type":"task_execution_event","task_execution_event":{"task_id":"task-1","steps":[],"completed":true,"blocked":false},"protocol_version":1}',
    '{"type":"local_command","command":"workspace","summary":"missing status","protocol_version":1}',
    '{"type":"task_scheduler_tick","queue":[],"protocol_version":1}',
    '{"type":"task_scheduler_queue","tick":{},"protocol_version":1}',
    '{"type":"task_scheduler_daemon_run","state":null,"protocol_version":1}',
    '{"type":"task_scheduler_daemon_status","state":null,"protocol_version":1}',
    '{"type":"task_scheduler_daemon_report","summary":{},"policy_recommendation":{},"protocol_version":1}',
    '{"type":"autonomous_preflight_blocked","operation":"policy apply","protocol_version":1}',
    '{"type":"cron_run","summary":{},"protocol_version":1}',
    '{"type":"route_feedback_summary","feedback_count":0,"protocol_version":1}',
    '{"type":"route_policy_apply","proposal":{},"protocol_version":1}',
    '{"type":"policy_apply","plan":{},"protocol_version":1}',
    '{"type":"benchmark_suite","suite_id":"complex-coding-agent-v1","version":"2026.05","protocol_version":1}',
    '{"type":"benchmark_task","tasks":[],"protocol_version":1}',
    '{"type":"benchmark_run","summary":{},"protocol_version":1}',
    '{"type":"worker_list","worker":{},"protocol_version":1}',
    '{"type":"worker_create","workers":[],"protocol_version":1}',
    '{"type":"worker_spawn","workers":[],"protocol_version":1}',
    '{"type":"worker_probe","workers":[],"protocol_version":1}',
    '{"type":"worker_ready","worker":{},"protocol_version":1}',
    '{"type":"worker_complete","workers":[],"protocol_version":1}',
    '{"type":"worker_supervisor_tick","workers":[],"protocol_version":1}',
  ];
  for (const line of invalidKnownShapes) {
    const invalid = parseStreamEventLine(line);
    assert.equal(invalid.ok, false, `${line} should fail stream schema validation`);
    assert.equal(invalid.reason, 'invalid-shape');
  }

  const malformed = parseStreamEventLine('{"foo":1}');
  assert.equal(malformed.ok, false);
  assert.equal(malformed.reason, 'invalid-shape');

  const badProtocolVersion = parseStreamEventLine('{"type":"message_start","protocol_version":"1"}');
  assert.equal(badProtocolVersion.reason, 'invalid-shape');

  const nonJson = parseStreamEventLine('not-json');
  assert.equal(nonJson.ok, false);
  assert.equal(nonJson.reason, 'not-json');

  const redacted = parseStreamEventLine('{"type":"reasoning_step","reasoning_step":{"step_type":"redacted_thinking","data":{"opaque":"redacted"}},"protocol_version":1}');
  assert.equal(redacted.ok, true);
  assert.equal(redacted.event.reasoning_step.data.opaque, 'redacted');

  const permissionRequest = parseStreamEventLine('{"type":"permission_request","tool":"write_file","input":"{\\"path\\":\\"generated/denied.txt\\"}","current_mode":"read-only","required_mode":"workspace-write","reason":"requires workspace-write","protocol_version":1}');
  assert.equal(permissionRequest.ok, true);
  assert.equal(permissionRequest.event.tool, 'write_file');
  assert.equal(permissionRequest.event.current_mode, 'read-only');
  assert.equal(permissionRequest.event.required_mode, 'workspace-write');

  const toolUse = parseStreamEventLine('{"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"fixture.txt"},"protocol_version":1}');
  assert.equal(toolUse.ok, true);
  assert.equal(toolUse.event.id, 'toolu_1');
  assert.equal(toolUse.event.name, 'read_file');
  assert.equal(toolUse.event.input.path, 'fixture.txt');

  const decisioning = parseStreamEventLine('{"type":"decisioning_event","decisioning_event":{"kind":"safety_assessment","title":"Safety assessment","summary":"Risk score 0.00 -> Allow","risk_score":0,"risk_level":"low","action":"allow"},"protocol_version":1}');
  assert.equal(decisioning.ok, true);
  assert.equal(decisioning.event.decisioning_event.kind, 'safety_assessment');
  assert.equal(decisioning.event.decisioning_event.risk_level, 'low');

  const decisioningDag = parseStreamEventLine('{"type":"decisioning_event","decisioning_event":{"kind":"task_decomposition","title":"Task decomposition","summary":"Split into 2 steps.","task_id":"task-1","plan_dag":{"task_id":"task-1","root_id":"task-1","nodes":[{"kind":"task","id":"task-1","title":"Do work","parallelizable":false,"estimated_effort":2,"candidate_tools":["read_file"],"notes":[]}],"edges":[{"from":"task-1","to":"task-1-analyze","kind":"contains"}]}},"protocol_version":1}');
  assert.equal(decisioningDag.ok, true);
  assert.equal(decisioningDag.event.decisioning_event.plan_dag.root_id, 'task-1');
  assert.equal(decisioningDag.event.decisioning_event.plan_dag.edges[0].kind, 'contains');

  const planExecution = parseStreamEventLine('{"type":"plan_execution_event","plan_execution_event":{"seq":1,"task_id":"task-1","node_id":"task-1-analyze","kind":"node_started","status":"running","message":"started"},"protocol_version":1}');
  assert.equal(planExecution.ok, true);
  assert.equal(planExecution.event.plan_execution_event.kind, 'node_started');
  assert.equal(planExecution.event.plan_execution_event.status, 'running');

  const modelRoute = parseStreamEventLine('{"type":"model_route_event","model_route_event":{"phase":"verification","model":"opus","provider":"anthropic","reason":"selected verifier"},"protocol_version":1}');
  assert.equal(modelRoute.ok, true);
  assert.equal(modelRoute.event.model_route_event.phase, 'verification');
  assert.equal(modelRoute.event.model_route_event.model, 'opus');

  const teamExecution = parseStreamEventLine('{"type":"team_execution_event","team_execution_event":{"seq":3,"team_id":"team-1","task_id":"task-1","role":"verifier","kind":"verification_passed","message":"passed"},"protocol_version":1}');
  assert.equal(teamExecution.ok, true);
  assert.equal(teamExecution.event.team_execution_event.role, 'verifier');
  assert.equal(teamExecution.event.team_execution_event.kind, 'verification_passed');

  assert.match(streamProtocolSource, /completed_nodes: string\[\]/);
  assert.match(streamProtocolSource, /resumable_nodes: string\[\]/);
  assert.match(streamProtocolSource, /last_result\?: string/);
  assert.match(streamProtocolSource, /needs_danger_full_access/);
  assert.match(streamProtocolSource, /'task_execution'/);
  assert.match(streamProtocolSource, /'recovery_action_event'/);
  assert.match(streamProtocolSource, /'task_execution_event'/);
  assert.match(streamProtocolSource, /'autonomous_preflight_blocked'/);
  assert.match(streamProtocolSource, /'policy_apply'/);
  assert.match(streamProtocolSource, /'route_policy_apply'/);
  assert.match(streamProtocolSource, /recovery_action_event\?: RecoveryActionExecution/);
  assert.match(streamProtocolSource, /task_execution_event\?: TaskExecutionOutcome/);
  assert.match(streamProtocolSource, /export type RecoveryActionExecution/);

  const recovery = parseStreamEventLine('{"type":"recovery_suggestion","source_event":"permission_denial","failure_class":"trust_gate","tool":"write_file","reason":"requires permission","action":"retry_with_danger_full_access","suggestion":"Retry with full access","protocol_version":1}');
  assert.equal(recovery.ok, true);
  assert.equal(recovery.event.source_event, 'permission_denial');
  assert.equal(recovery.event.failure_class, 'trust_gate');
  assert.equal(recovery.event.action, 'retry_with_danger_full_access');

  assert.equal(isKnownStreamEventType('unknown_event'), false);

  assert.equal(readProtocolVersion({ type: 'message_start', protocol_version: 1 }), 1);
  assert.equal(readProtocolVersion({ type: 'message_start', schema_version: 2 }), 2);
  assert.equal(readProtocolVersion({ type: 'message_start' }), undefined);

  assert.equal(classifyProtocolVersion(undefined), 'missing');
  assert.equal(classifyProtocolVersion(STREAM_PROTOCOL_VERSION), 'match');
  assert.equal(classifyProtocolVersion(STREAM_PROTOCOL_VERSION + 1), 'mismatch');
});

test('chat panel preserves cli session metadata across turns', () => {
  assert.match(chatPanelSource, /case 'session_meta'/);
  assert.match(chatPanelSource, /this\.selectedCliSessionId = sessionId/);
  assert.match(chatPanelSource, /resumeTarget: sessionId/);
  assert.match(chatPanelSource, /type: 'sessionMeta'/);
  assert.match(chatPanelSource, /case 'sessionMeta': \{/);
  assert.match(chatPanelSource, /state\.resumeTarget = sessionId/);
  assert.match(chatPanelSource, /setStatus\('Session ' \+ sessionId\.slice\(0, 12\), 'done'\)/);
});

test('chat panel appends follow-up turns to active history record', () => {
  assert.match(chatPanelSource, /const selectedRecord = this\.selectedHistoryId\s*\? this\.history\.records\(\)\.find\(\(item\) => item\.id === this\.selectedHistoryId\)\s*:\s*undefined/s);
  assert.match(chatPanelSource, /const existingRecord = startFreshForTask \? undefined : selectedRecord/);
  assert.match(chatPanelSource, /: input\.resumeTarget\?\.trim\(\) \|\| existingRecord\?\.resumeTarget\?\.trim\(\) \|\| this\.selectedCliSessionId \|\| undefined/);
  assert.match(chatPanelSource, /const record = existingRecord \?\? await this\.history\.createDraft\(/);
  assert.match(chatPanelSource, /await this\.history\.setActiveRecord\(existingRecord\.id\)/);
  assert.match(chatPanelSource, /resumeTarget: INIT\.resumeTarget \|\| ACTIVE_RECORD\.resumeTarget \|\| ''/);
  assert.match(chatPanelSource, /state\.resumeTarget = rec\.resumeTarget \|\| ''/);
  assert.match(chatPanelSource, /state\.resumeTarget = activeRecord && activeRecord\.resumeTarget \? activeRecord\.resumeTarget : state\.resumeTarget/);
});

test('chat participant forwards recent VS Code chat history', () => {
  assert.doesNotMatch(chatParticipantSource, /void chatContext/);
  assert.match(chatParticipantSource, /const promptWithHistory = buildPromptWithChatHistory\(prompt, chatContext\)/);
  assert.match(chatParticipantSource, /function buildPromptWithChatHistory\(prompt: string, chatContext: vscode\.ChatContext\): string/);
  assert.match(chatParticipantSource, /chatContext\.history\.slice\(-8\)/);
  assert.match(chatParticipantSource, /turn instanceof vscode\.ChatRequestTurn/);
  assert.match(chatParticipantSource, /turn instanceof vscode\.ChatResponseTurn/);
  assert.match(chatParticipantSource, /Previous messages in this VS Code chat session:/);
  assert.match(chatParticipantSource, /Current user request:/);
  assert.match(chatParticipantSource, /function trimHistoryText\(text: string\): string/);
});

test('assistant history persistence is throttled during streaming', () => {
  assert.match(chatPanelSource, /private\s+createAssistantTailPersistence\(/);
  assert.match(chatPanelSource, /assistantTailPersistence\.schedule\(\)/);
  assert.match(chatPanelSource, /assistantTailPersistence\.flush\(\)/);
  assert.doesNotMatch(chatPanelSource, /void this\.history\.replaceAssistantTail\(record\.id, assistantText\)/);
});
test('permission retry paths share one danger-full-access helper', () => {
  assert.match(chatPanelSource, /private\s+offerDangerFullAccessRetry\(/);
  assert.match(chatPanelSource, /const offerPermissionRetryOnce = \(toolName: string, reason: string, source: string\) => \{/);
  assert.match(chatPanelSource, /offerPermissionRetryOnce\(toolName, output, 'tool_result'\)/);
  assert.match(chatPanelSource, /offerPermissionRetryOnce\(deniedTool, denialReason, event\.type\)/);
  assert.doesNotMatch(chatPanelSource, /user approved via stream event/);
});
test('permission closed-loop authorizes tools via host stdin response', () => {
  // #4: the CLI no longer auto-denies; extension shows a modal and writes back.
  assert.match(chatPanelSource, /case 'permission_request'/);
  assert.match(chatPanelSource, /resolvePermissionRequest\(requestedTool/);
  assert.match(chatPanelSource, /type: 'permission_response', decision/);
  assert.match(chatPanelSource, /sessionAllowedTools\.add\(tool\)/);
  assert.match(chatPanelSource, /Allow for this session/);
  assert.match(chatPanelSource, /Deny/);
  assert.match(chatPanelSource, /private sessionAllowedTools = new Set/);
  // Still forwards structured fields to the webview for awareness.
  assert.match(chatPanelSource, /type: 'permissionRequest'/);
  assert.match(chatPanelSource, /case 'permissionRequest': \{/);
  assert.match(chatPanelSource, /Permission requested for/);
});
test('tool and permission webview events render structured fields', () => {
  assert.match(chatPanelSource, /case 'toolStep': \{/);
  assert.match(chatPanelSource, /renderToolCard\(msg\)/);
  assert.match(chatPanelSource, /function renderToolCard\(msg\)/);
  assert.match(chatPanelSource, /msg\.step === 'result'/);
  assert.match(chatPanelSource, /case 'permissionDenial': \{/);
  assert.match(chatPanelSource, /Permission denied for/);
});

test('rich tool cards render per-tool fields, diffs, and clickable paths', () => {
  // Per-tool icon/label metadata + structured input dispatch.
  assert.match(chatPanelSource, /function toolMeta\(name\)/);
  assert.match(chatPanelSource, /function toolUseBody\(name, input\)/);
  assert.match(chatPanelSource, /function parseToolInput\(msg\)/);
  // edit_file renders a diff preview.
  assert.match(chatPanelSource, /function renderDiff\(oldStr, newStr\)/);
  assert.match(chatPanelSource, /diff-add/);
  assert.match(chatPanelSource, /diff-del/);
  // bash/code gets token highlighting.
  assert.match(chatPanelSource, /function highlightCode\(code, lang\)/);
  assert.match(chatPanelSource, /tok-cmd/);
  // Clickable file path chips post openFile back to the host.
  assert.match(chatPanelSource, /function filePathChip\(rawPath\)/);
  assert.match(chatPanelSource, /data-open-path/);
  assert.match(chatPanelSource, /type: 'openFile'/);
  // Results attach to the originating use-card and collapse long output.
  assert.match(chatPanelSource, /const pendingToolCardsById = \{\}/);
  assert.match(chatPanelSource, /const pendingToolCardsByName = \{\}/);
  assert.match(chatPanelSource, /function buildResultBlock\(output, isError\)/);
  // CLI forwards structured input alongside the legacy JSON string.
  assert.match(chatPanelSource, /inputData: event\.input \?\? null/);
  // Extension host opens the file (with optional line) on request.
  assert.match(chatPanelSource, /case 'openFile':/);
  assert.match(chatPanelSource, /private async openFileFromCard\(rawPath: string, line: number \| null\)/);
  assert.match(chatPanelSource, /vscode\.window\.showTextDocument\(doc, options\)/);
});


test('decisioning visualization hooks remain wired', () => {
  assert.match(chatPanelSource, /case 'decisioning_event'/);
  assert.match(chatPanelSource, /addDecisioningEvent/);
  assert.match(chatPanelSource, /msg decisioning-step/);
});

test('decisioning workbench cards render structured UI safely', () => {
  assert.match(chatPanelSource, /function sanitizeDecisioningClassToken\(value\)/);
  assert.match(chatPanelSource, /function renderDecisioningOverview\(event, riskLevel\)/);
  assert.match(chatPanelSource, /decisioning-overview/);
  assert.match(chatPanelSource, /Workbench Overview/);
  assert.match(chatPanelSource, /list\.slice\(\)\.sort\(function\(left, right\)/);
  assert.match(chatPanelSource, /decisioning-node-overflow/);
  assert.match(chatPanelSource, /function renderDecisioningDetailItem\(item, index\)/);
  assert.match(chatPanelSource, /decisioning-detail-json/);
  assert.match(chatPanelSource, /const safeKindClass = sanitizeDecisioningClassToken\(kind\)/);
  assert.match(chatPanelSource, /const safeLevelClass = depth >= 4 \? 'level-deep' : 'level-' \+ depth/);
});

test('runtime execution stream events render through the webview', () => {
  assert.match(chatPanelSource, /case 'plan_execution_event'/);
  assert.match(chatPanelSource, /case 'task_ledger_event'/);
  assert.match(chatPanelSource, /case 'model_route_event'/);
  assert.match(chatPanelSource, /case 'team_execution_event'/);
  assert.match(chatPanelSource, /case 'recovery_event'/);
  assert.match(chatPanelSource, /type: 'runtimeEvent'/);
  assert.match(chatPanelSource, /event: event\.plan_execution_event/);
  assert.match(chatPanelSource, /function recoveryEventSummary\(value\)/);
  assert.match(chatPanelSource, /Recovery', attempted\.scenario, outcome/);
  assert.match(chatPanelSource, /const attempt = value\.attempt \? 'attempt ' \+ value\.attempt : undefined/);
  assert.match(chatPanelSource, /const blocked = value\.blocking_reason \? 'blocked: ' \+ value\.blocking_reason : undefined/);
  assert.match(chatPanelSource, /const confidence = typeof value\.confidence === 'number' \? 'confidence ' \+ Math\.round\(value\.confidence \* 100\) \+ '%' : undefined/);
  assert.match(chatPanelSource, /const fallback = value\.fallback_model \? 'fallback ' \+ value\.fallback_model : undefined/);
  assert.match(chatPanelSource, /Task', value\.task_id, label, value\.status/);
  assert.match(chatPanelSource, /Raw event/);
  assert.match(chatPanelSource, /case 'task_execution'/);
  assert.match(chatPanelSource, /case 'task_recovery'/);
  assert.match(chatPanelSource, /case 'recovery_action_event'/);
  assert.match(chatPanelSource, /case 'task_execution_event'/);
  assert.match(chatPanelSource, /const label = kind === 'recovery_action_event' \? 'Recovery action' : 'Task recovery'/);
  assert.match(chatPanelSource, /return \[label, execution\.task_id, results\]/);
  assert.match(chatPanelSource, /const label = kind === 'task_execution_event' \? 'Task execution event' : 'Task execution'/);
  assert.match(chatPanelSource, /return \[label, outcome\.task_id, status, stepCount, outcome\.message\]/);
  assert.match(chatPanelSource, /Raw event/);
  assert.match(chatPanelSource, /case 'runtimeEvent': \{/);
});

test('runtime events render as structured cards, not single-line text', () => {
  // New rich rendering path replaces the old "Runtime · kind" one-liner.
  assert.match(chatPanelSource, /function renderRuntimeEventDetail\(kind, event\)/);
  assert.match(chatPanelSource, /function runtimeEventTone\(kind, event\)/);
  assert.match(chatPanelSource, /function toneFromStatus\(s\)/);
  assert.match(chatPanelSource, /const detail = renderRuntimeEventDetail\(kind, event\)/);
  assert.match(chatPanelSource, /div\.className = 'msg runtime-event tone-' \+ tone/);
  // model_route renders a confidence bar + phase→model route line.
  assert.match(chatPanelSource, /function rtConfidenceBar\(conf\)/);
  assert.match(chatPanelSource, /class="rt-route"/);
  // recovery_action renders a timeline.
  assert.match(chatPanelSource, /class="rt-timeline"/);
  // task_execution renders an ordered step list.
  assert.match(chatPanelSource, /class="rt-steps"/);
  // status pills with tone classes.
  assert.match(chatPanelSource, /function rtPill\(text, tone\)/);
  assert.match(chatPanelSource, /rt-pill-/);
  // Raw event remains available for debugging.
  assert.match(chatPanelSource, /<details class="rt-raw">/);
});


test('task board MVP renders task status, node, and recovery events', () => {
  assert.match(chatPanelSource, /task-board-surface/);
  assert.match(chatPanelSource, /const taskBoardSurface = document\.getElementById\('taskBoardSurface'\)/);
  assert.match(chatPanelSource, /function taskBoardInitialState\(\)/);
  assert.match(chatPanelSource, /function updateTaskBoardFromRuntimeEvent\(kind, event\)/);
  assert.match(chatPanelSource, /kind === 'task_list'/);
  assert.match(chatPanelSource, /kind === 'task_packet_create' \|\| kind === 'task_packet_run' \|\| kind === 'task_packet_status'/);
  assert.match(chatPanelSource, /kind === 'task_scheduler_tick'/);
  assert.match(chatPanelSource, /kind === 'task_scheduler_queue'/);
  assert.match(chatPanelSource, /state\.taskBoard\.currentNode = \{/);
  assert.match(chatPanelSource, /function renderTaskBoardTask\(task\)/);
  assert.match(chatPanelSource, /function renderTaskBoardNode\(node\)/);
  assert.match(chatPanelSource, /function renderTaskBoardRecovery\(\)/);
  assert.match(chatPanelSource, /rememberTaskBoardRecovery\('recoverySuggestion', msg\)/);
  assert.match(chatPanelSource, /Live task status, worker health, active node, and recovery timeline from stream events/);
});

test('task board visualizes task progress and plan node DAG', () => {
  // Task progress bar + status distribution.
  assert.match(chatPanelSource, /function renderTaskBoardProgress\(tasks\)/);
  assert.match(chatPanelSource, /renderTaskBoardProgress\(tasks\)/);
  assert.match(chatPanelSource, /class="tb-progress"/);
  assert.match(chatPanelSource, /class="tb-bar"/);
  assert.match(chatPanelSource, /done \+ '\/' \+ total \+ ' done<\/span>'/);
  // Shared tone mapping.
  assert.match(chatPanelSource, /function taskBoardTone\(value\)/);
  // Plan node accumulation + dot-grid progress.
  assert.match(chatPanelSource, /function rememberPlanNode\(nodeId, status, kind\)/);
  assert.match(chatPanelSource, /rememberPlanNode\(value\.node_id, value\.status, value\.kind\)/);
  assert.match(chatPanelSource, /planNodes:\s*\{\}/);
  assert.match(chatPanelSource, /planNodeOrder:\s*\[\]/);
  assert.match(chatPanelSource, /function renderPlanNodeProgress\(\)/);
  assert.match(chatPanelSource, /class="tb-node-grid"/);
  assert.match(chatPanelSource, /class="tb-node-dot/);
  // Progress CSS present.
  assert.match(chatPanelSource, /\.tb-seg-ok \{ background: #4ec9b0; \}/);
});


test('recovery suggestion stream events render and persist evidence', () => {
  assert.match(chatPanelSource, /case 'recovery_suggestion'/);
  assert.match(chatPanelSource, /const recoveryEvidence:\s*RecoveryEvidence\s*=\s*\{/);
  assert.match(chatPanelSource, /this\.history\.appendRecoveryEvidence\(record\.id, recoveryEvidence\)/);
  assert.match(chatPanelSource, /type:\s*'recoverySuggestion'/);
  assert.match(chatPanelSource, /sourceEvent:\s*recoveryEvidence\.sourceEvent/);
  assert.match(chatPanelSource, /failureClass:\s*recoveryEvidence\.failureClass/);
  assert.match(chatPanelSource, /case 'recoverySuggestion': \{/);
  assert.match(chatPanelSource, /addRecoverySuggestion/);
  assert.match(chatPanelSource, /Failure class:/);
  assert.match(chatPanelSource, /body \+= '\\\\nFailure class: ' \+ failureClass/);
  assert.match(chatPanelSource, /body \+= '\\\\nAction: ' \+ action/);
  assert.match(chatPanelSource, /body \+= '\\\\nReason: ' \+ reason\.slice\(0, 240\)/);
  assert.match(chatPanelSource, /msg recovery-suggestion/);

  assert.match(historySource, /export interface RecoveryEvidence/);
  assert.match(historySource, /recoveryEvidence\?:\s*RecoveryEvidence\[\]/);
  assert.match(historySource, /async appendRecoveryEvidence\(recordId:\s*string, evidence:\s*RecoveryEvidence\):\s*Promise<void>/);
});

test('attachment descriptors are shared and persisted in history', () => {
  assert.match(chatPanelSource, /extractPromptAttachmentReferences/);
  assert.match(chatPanelSource, /extractReferencePathCandidate/);
  assert.match(chatPanelSource, /\.map\(\(file\) => extractReferencePathCandidate\(file\)\)/);
  assert.match(chatPanelSource, /const files = \[\.\.\.explicitFiles, \.\.\.extractPromptAttachmentReferences\(prompt\)\]/);
  assert.match(chatPanelSource, /prepareAttachmentDescriptors\(files, cwd, 'picker'\)/);
  assert.match(chatPanelSource, /Attachments included in request:/);
  assert.match(chatPanelSource, /attachments:\s*preparedAttachments\.descriptors/);
  assert.match(chatPanelSource, /addBubble\(m\.role, m\.text, m\.attachments\)/);
  assert.match(historySource, /attachments\?: AttachmentDescriptor\[\]/);
});

test('history snapshot sends lightweight index and lazy-loads selected records', () => {
  assert.match(historySource, /records:\s*this\.records\(\)\.map\(\(record\) => this\.snapshotRecord\(record, activeRecordId\)\)/);
  assert.match(historySource, /messages:\s*\[\]/);
  assert.match(historySource, /MAX_HISTORY_RECORDS\s*=\s*80/);
  assert.match(historySource, /MAX_MESSAGES_PER_RECORD\s*=\s*80/);
  assert.match(historySource, /MAX_MESSAGE_TEXT_CHARS\s*=\s*24_000/);
  assert.match(historySource, /storageVersion\?:\s*number/);
  assert.match(chatPanelSource, /private\s+webviewRecord\(record:\s*ChatHistoryRecord\):\s*ChatHistoryRecord/);
  assert.match(chatPanelSource, /type:\s*'historyRecord'/);
  assert.match(chatPanelSource, /function applyHistoryRecord\(rec\)/);
  assert.match(chatPanelSource, /case 'historyRecord':/);
});

test('history drawer can delete records and deprecated decisioning demo is removed', () => {
  assert.match(historySource, /async remove\(recordId:\s*string\):\s*Promise<void>/);
  assert.match(historySource, /deleteRecordBody\(recordId\)/);
  assert.match(chatPanelSource, /typedMessage\.action === 'delete' && typedMessage\.historyId/);
  assert.match(chatPanelSource, /vscode\.window\.showWarningMessage/);
  assert.match(chatPanelSource, /class="history-delete"/);
  assert.match(chatPanelSource, /Confirm deletion in VS Code/);
  assert.match(chatPanelSource, /type:\s*'historyDeleted'/);
  assert.match(chatPanelSource, /type:\s*'historyDeleteCancelled'/);
  assert.match(chatPanelSource, /case 'historyDeleted':/);
  assert.match(chatPanelSource, /case 'historyDeleteCancelled':/);
  assert.doesNotMatch(chatPanelSource, /window\.confirm\(/);
  assert.doesNotMatch(chatPanelSource, /btnDemo|showDecisioningDemo|toggle-decisioning-demo|decisioning-demo/);
});

test('local model discovery is cached and time bounded', () => {
  assert.match(cliSource, /LOCAL_MODEL_CACHE_TTL_MS\s*=\s*30_000/);
  assert.match(cliSource, /LOCAL_MODEL_HTTP_TIMEOUT_MS\s*=\s*1_200/);
  assert.match(cliSource, /LOCAL_MODEL_CLI_TIMEOUT_MS\s*=\s*2_000/);
  assert.match(cliSource, /private localModelCache/);
  assert.match(cliSource, /async listLocalModels\(options:\s*\{\s*force\?:\s*boolean\s*\}\s*=\s*\{\}\)/);
  assert.match(cliSource, /new AbortController\(\)/);
  assert.match(cliSource, /timeout:\s*LOCAL_MODEL_CLI_TIMEOUT_MS/);
});

test('attachment path extraction accepts descriptor and Uri-shaped values', () => {
  const { extractReferencePathCandidate } = require(attachmentPathsPath);

  assert.equal(extractReferencePathCandidate('/tmp/paper.pdf'), '/tmp/paper.pdf');
  assert.equal(extractReferencePathCandidate({ fsPath: '/tmp/from-fs-path.pdf' }), '/tmp/from-fs-path.pdf');
  assert.equal(extractReferencePathCandidate({ uri: { fsPath: '/tmp/from-uri.pdf' } }), '/tmp/from-uri.pdf');
  assert.equal(extractReferencePathCandidate({ value: { uri: { path: '/tmp/from-nested-value.pdf' } } }), '/tmp/from-nested-value.pdf');
  assert.equal(extractReferencePathCandidate({ unexpected: true }), undefined);
});

test('recovery evidence replays from history records', () => {
  assert.match(chatPanelSource, /recoveryEvidence:\s*\(ACTIVE_RECORD\.recoveryEvidence \|\| \[\]\)\.slice\(\)/);
  assert.match(chatPanelSource, /state\.recoveryEvidence = \(rec\.recoveryEvidence \|\| \[\]\)\.slice\(\)/);
  assert.match(chatPanelSource, /function replayRecoveryEvidence\(\)/);
  assert.match(chatPanelSource, /replayRecoveryEvidence\(\)/);
  assert.match(chatPanelSource, /state\.recoveryEvidence\.push\(\{/);
  assert.match(chatPanelSource, /state\.messages\.length === 0 && state\.recoveryEvidence\.length === 0/);
});

test('chat bootstrap carries memory-derived identity labels', () => {
  assert.match(chatPanelSource, /export interface ChatIdentity/);
  assert.match(chatPanelSource, /preferredLanguage\?:\s*string/);
  assert.match(chatPanelSource, /export function readWorkspaceIdentity\(\): ChatIdentity/);
  assert.match(chatPanelSource, /identity\?: ChatIdentity/);
  assert.match(extensionSource, /readWorkspaceIdentity/);
  assert.match(extensionSource, /identity:\s*readWorkspaceIdentity\(\)/);
  assert.match(chatPanelSource, /kind === 'user_identity' \|\| topic === 'user_identity'/);
  assert.match(chatPanelSource, /kind === 'assistant_identity' \|\| topic === 'assistant_identity'/);
  assert.match(chatPanelSource, /kind === 'language_preference' \|\| topic === 'language_preference'/);
});

test('chat submissions preserve preferred interaction language across turns', () => {
  assert.match(chatPanelSource, /function\s+detectPreferredResponseLanguage\(text:\s*string\):\s*string \| undefined/);
  assert.match(chatPanelSource, /function\s+detectDominantResponseLanguage\(text:\s*string\):\s*string \| undefined/);
  assert.match(chatPanelSource, /function\s+resolvePreferredResponseLanguage\(prompt:\s*string,\s*storedLanguage:\s*string \| undefined\):\s*string \| undefined/);
  assert.match(chatPanelSource, /'请用中文'/);
  assert.match(chatPanelSource, /'中文交流'/);
  assert.match(chatPanelSource, /return 'Chinese'/);
  assert.match(chatPanelSource, /private\s+readonly\s+languagePreferenceKey\s*=\s*'himalayaCode\.preferredResponseLanguage\.v1'/);
  assert.match(chatPanelSource, /await this\.context\.workspaceState\.update\(this\.languagePreferenceKey, detectedLanguage\)/);
  assert.match(chatPanelSource, /preferredLanguage:\s*detectedLanguage/);
  assert.match(chatPanelSource, /this\.currentBootstrap\.identity\?\.preferredLanguage/);
  assert.match(chatPanelSource, /this\.context\.workspaceState\.get<string>\(this\.languagePreferenceKey\)/);
  assert.match(chatPanelSource, /function\s+languagePromptPrefix\(language:\s*string\):\s*string/);
  assert.match(chatPanelSource, /Persistent interaction language: respond to the user in Chinese/);
  assert.match(chatPanelSource, /prose headings and explanations must be Chinese/);
  assert.match(chatPanelSource, /const cliPrompt = applyLanguagePreferenceToPrompt\(prompt, preferredLanguage\)/);
  assert.match(chatPanelSource, /detectDominantResponseLanguage\(prompt\) \?\? storedLanguage/);
  assert.match(chatPanelSource, /role:\s*'user',\s*\n\s*text:\s*prompt/);
  assert.match(chatPanelSource, /args\.push\('prompt', prompt\)/);
});

test('webview message labels follow user-declared identity', () => {
  assert.match(chatPanelSource, /function labelForRole\(role\)/);
  assert.match(chatPanelSource, /state\.identity && state\.identity\.userDisplayName/);
  assert.match(chatPanelSource, /state\.identity && state\.identity\.assistantDisplayName/);
  assert.match(chatPanelSource, /function updateIdentityFromPrompt\(text\)/);
  assert.match(chatPanelSource, /updateIdentityFromPrompt\(text\);/);
  assert.match(chatPanelSource, /labelForRole\('assistant'\)/);
  assert.match(chatPanelSource, /const label = labelForRole\(role\);/);
  assert.match(chatPanelSource, /\.msg\.user \.msg-role \{ color: var\(--accent-text\); text-transform: none; \}/);
  assert.match(chatPanelSource, /\.msg\.assistant \.msg-role \{ color: var\(--success\); text-transform: none; \}/);
});

test('assistant output renders markdown via the bundled marked script', () => {
  // marked.umd.js is loaded into the live webview (not the deleted media/webview.js).
  assert.match(chatPanelSource, /asWebviewUri\(\s*[\s\S]*?'media',\s*'marked\.umd\.js'/);
  assert.match(chatPanelSource, /<script nonce="\$\{nonce\}" src="\$\{markedUri\}"><\/script>/);
  // CSP must allow the bundled script source alongside the nonce.
  assert.match(chatPanelSource, /script-src 'nonce-\$\{nonce\}' \$\{webview\.cspSource\}/);
  assert.match(chatPanelSource, /localResourceRoots:\s*\[vscode\.Uri\.joinPath\(this\.context\.extensionUri,\s*'media'\)\]/);
  // The inline renderer parses markdown and degrades gracefully without marked.
  assert.match(chatPanelSource, /function renderMarkdown\(md\)/);
  assert.match(chatPanelSource, /marked\.parse\(String\(md == null \? '' : md\)/);
  // Streaming buffers text and renders incrementally at a safe fence boundary.
  assert.match(chatPanelSource, /function findSafeRenderBoundary\(text\)/);
  assert.match(chatPanelSource, /streamBuffer \+= \(text == null \? '' : text\)/);
  assert.match(chatPanelSource, /body\.innerHTML = renderMarkdown\(streamBuffer\)/);
  // History-replayed assistant bubbles render markdown too.
  assert.match(chatPanelSource, /role === 'assistant' \? renderMarkdown\(text \|\| ''\) : esc\(text \|\| ''\)/);
  // Rendered markdown bodies override the pre-wrap whitespace of raw bubbles.
  assert.match(chatPanelSource, /\.msg\.assistant \.msg-body \{ white-space: normal/);
  assert.match(chatPanelSource, /\.msg\.assistant \.msg-body pre \{/);
});

test('orphaned standalone webview assets are removed in favor of the inline UI', () => {
  const mediaDir = path.join(root, 'media');
  assert.ok(!fs.existsSync(path.join(mediaDir, 'webview.js')), 'media/webview.js should be deleted');
  assert.ok(!fs.existsSync(path.join(mediaDir, 'webview.css')), 'media/webview.css should be deleted');
  assert.ok(fs.existsSync(path.join(mediaDir, 'marked.umd.js')), 'media/marked.umd.js must still ship');
});

test('reasoning visualization is fully gated by the showReasoning toggle', () => {
  // #2: decisioning events carry chain-of-thought; must NOT leak when toggle off.
  assert.match(chatPanelSource, /event\.decisioning_event && Boolean\(this\.currentOptions\.showReasoning\)/);
  // reasoning_step forwarding stays gated too.
  assert.match(chatPanelSource, /const show = Boolean\(this\.currentOptions\.showReasoning\)/);
  // Defense in depth: webview render also checks state.showReasoning.
  assert.match(chatPanelSource, /function addDecisioningEvent\(event\)\s*\{[\s\S]*?if \(!state\.showReasoning\) \{ return; \}/);
});

test('new chat auto-resumes latest session unless explicitly new', () => {
  // #1: extension tracks an explicit "new session" intent and passes --new.
  assert.match(chatPanelSource, /private forceNewSession = false/);
  assert.match(chatPanelSource, /this\.forceNewSession = true/);
  assert.match(chatPanelSource, /const startFresh = this\.forceNewSession && !effectiveResumeTarget/);
  assert.match(chatPanelSource, /forceNewSession: startFresh/);
  // cli.ts adds --new only when no resume target and forceNewSession set.
  assert.match(cliSource, /forceNewSession\?: boolean/);
  assert.match(cliSource, /else if \(options\.forceNewSession\)\s*\{[\s\S]*?args\.push\('--new'\)/);
});

test('attachment-backed document generation starts a fresh session', () => {
  assert.match(chatPanelSource, /function\s+shouldStartFreshForAttachmentDocumentTask\(prompt:\s*string,\s*hasAttachment:\s*boolean\):\s*boolean/);
  assert.match(chatPanelSource, /依据附件\|根据附件\|基于附件/);
  assert.match(chatPanelSource, /pptx\?\|powerpoint\|幻灯片\|演示文稿\|答辩/);
  assert.match(chatPanelSource, /生成\|制作\|创建\|输出\|导出\|整理/);
  assert.match(chatPanelSource, /const startFreshForTask = shouldStartFreshForAttachmentDocumentTask\(prompt, attachmentPaths\.length > 0\)/);
  assert.match(chatPanelSource, /this\.selectedCliSessionId = null/);
  assert.match(chatPanelSource, /this\.forceNewSession = true/);
  assert.match(chatPanelSource, /const existingRecord = startFreshForTask \? undefined : selectedRecord/);
  assert.match(chatPanelSource, /const resumeTarget = startFreshForTask[\s\S]*?\? undefined[\s\S]*?: input\.resumeTarget\?\.trim\(\)/);
});

test('skills are surfaced in the extension (list, install, invoke)', () => {
  // #3: cli.ts exposes skill backend calls.
  assert.match(cliSource, /async listSkills\(cwd\?: string\): Promise<HimalayaSkillInfo\[\]>/);
  assert.match(cliSource, /async installSkill\(sourcePath: string, cwd\?: string\)/);
  assert.match(cliSource, /'skills', 'list', '--output-format', 'json'/);
  assert.match(cliSource, /'skills', 'install', sourcePath, '--output-format', 'json'/);
  assert.match(cliSource, /const sourceValue = skill\.source/);
  assert.match(cliSource, /sourceValue && typeof sourceValue === 'object'/);
  assert.match(cliSource, /typeof parsed\.invocation_name === 'string'/);
  // chatPanel wires a Skills button + manager + composer insertion.
  assert.match(chatPanelSource, /id="btnSkills"/);
  assert.match(chatPanelSource, /command === 'manageSkills'/);
  assert.match(chatPanelSource, /async manageSkills\(\): Promise<void>/);
  assert.match(chatPanelSource, /type: 'insertComposerText', text: '\$' \+ picked\.skillName/);
  assert.match(chatPanelSource, /CLI resolves `\$skill args` on submit/);
  assert.match(chatPanelSource, /case 'insertComposerText':/);
});

test('HimalayaCli parses structured skill list and install JSON', async () => {
  const oldLoad = Module._load;
  Module._load = (request, parent, isMain) => {
    if (request === 'vscode') {
      return {
        workspace: {
          getConfiguration: () => ({ get: () => '' }),
          workspaceFolders: []
        }
      };
    }
    return oldLoad.call(Module, request, parent, isMain);
  };
  try {
    const cliPath = path.join(root, 'out', 'cli.js');
    delete require.cache[require.resolve(cliPath)];
    const { HimalayaCli } = require(cliPath);
    const cli = new HimalayaCli({}, { appendLine() {}, append() {}, show() {} });
    const calls = [];
    cli.run = async (args, options) => {
      calls.push({ args, options });
      if (args[1] === 'list') {
        return {
          exitCode: 0,
          stdout: JSON.stringify({
            skills: [
              {
                name: 'demo',
                description: 'Demo skill',
                path: '/workspace/.Himalaya/skills/demo/SKILL.md',
                source: { id: 'project-Himalaya', label: 'Project roots' },
                origin: { id: 'skills_dir', detail_label: null },
                shadowed_by: null
              },
              {
                name: 'legacy',
                path: '/home/user/.Himalaya/commands/legacy.md',
                origin: { id: 'legacy_commands_dir', detail_label: 'legacy /commands' },
                shadowed_by: { id: 'project-Himalaya', label: 'Project roots' }
              },
              { description: 'ignored without a name' }
            ]
          }),
          stderr: ''
        };
      }
      return {
        exitCode: 0,
        stdout: JSON.stringify({
          kind: 'skills',
          action: 'install',
          result: 'installed',
          invocation_name: 'demo'
        }),
        stderr: ''
      };
    };

    assert.deepEqual(await cli.listSkills('/workspace'), [
      {
        name: 'demo',
        description: 'Demo skill',
        source: 'Project roots',
        path: '/workspace/.Himalaya/skills/demo/SKILL.md',
        shadowed: false
      },
      {
        name: 'legacy',
        description: '',
        source: 'legacy /commands',
        path: '/home/user/.Himalaya/commands/legacy.md',
        shadowed: true
      }
    ]);
    assert.deepEqual(await cli.installSkill('/tmp/demo', '/workspace'), {
      ok: true,
      message: 'Installed demo'
    });
    assert.deepEqual(calls[0].args, ['skills', 'list', '--output-format', 'json']);
    assert.equal(calls[0].options.cwd, '/workspace');
    assert.deepEqual(calls[1].args, ['skills', 'install', '/tmp/demo', '--output-format', 'json']);
    assert.equal(calls[1].options.cwd, '/workspace');
  } finally {
    Module._load = oldLoad;
  }
});

test('cloud model config is unified with the CLI provider.json format', () => {
  const os = require('node:os');
  const providerConfig = require(path.join(root, 'out', 'providerConfig.js'));
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'himalaya-pc-'));
  const credHome = path.join(tmp, 'cfghome');
  const prevHome = process.env.Himalaya_CONFIG_HOME;
  process.env.Himalaya_CONFIG_HOME = credHome;
  try {
    providerConfig.saveProviderSelection(tmp, {
      model: 'gpt-4o',
      baseUrl: 'https://api.example.com/v1',
      apiKey: 'sk-test-xyz'
    });

    // provider.json must be byte-compatible with the CLI (.Himalaya/provider.json,
    // model + base_url, NO api_key in the project file).
    const cfgPath = path.join(tmp, '.Himalaya', 'provider.json');
    assert.ok(fs.existsSync(cfgPath), 'provider.json should be written');
    const cfg = JSON.parse(fs.readFileSync(cfgPath, 'utf8'));
    assert.equal(cfg.model, 'gpt-4o');
    assert.equal(cfg.base_url, 'https://api.example.com/v1');
    assert.ok(!('api_key' in cfg), 'api key must not be in project provider.json');

    // API key goes to the credentials file under the config home.
    const credPath = path.join(credHome, 'provider_credentials.json');
    assert.ok(fs.existsSync(credPath), 'credentials file should be written');
    assert.equal(JSON.parse(fs.readFileSync(credPath, 'utf8')).api_key, 'sk-test-xyz');

    // Round-trip load returns the full selection.
    const loaded = providerConfig.loadProviderSelection(tmp);
    assert.equal(loaded.model, 'gpt-4o');
    assert.equal(loaded.baseUrl, 'https://api.example.com/v1');
    assert.equal(loaded.apiKey, 'sk-test-xyz');

    // listProviderProfiles always exposes a "default" profile.
    const profiles = providerConfig.listProviderProfiles(tmp);
    assert.ok(profiles.default, 'default profile present');
    assert.equal(profiles.default.model, 'gpt-4o');
  } finally {
    if (prevHome === undefined) { delete process.env.Himalaya_CONFIG_HOME; }
    else { process.env.Himalaya_CONFIG_HOME = prevHome; }
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('cloud model wizard fetches real model catalogue from provider', () => {
  // #3: listCloudModels hits GET /v1/models, with fallback.
  assert.match(cliSource, /async listCloudModels\(baseUrl: string, apiKey: string\): Promise<HimalayaCloudModelInfo\[\]>/);
  assert.match(cliSource, /\`\$\{cleanBase\}\/models\`/);
  assert.match(cliSource, /Authorization.*Bearer.*apiKey/);
  // configureCloudModelRoute merges provider models into the picker
  assert.match(chatPanelSource, /this\.cli\.listCloudModels\(baseUrl, apiKey\)/);
  assert.match(chatPanelSource, /seen\.has\(alias\)/);
});

test('cloud model wizard mirrors selection into the shared provider.json', () => {
  // Still mirrors to CLI-compatible provider.json (from #4).
  assert.match(chatPanelSource, /import \{.*loadProviderSelection.*saveProviderSelection.*\} from '\.\/providerConfig'/);
  assert.match(chatPanelSource, /saveProviderSelection\(workspaceRoot, \{/);
  assert.match(chatPanelSource, /const savedSelection = workspaceRoot \? loadProviderSelection\(workspaceRoot\) : null/);
  assert.match(chatPanelSource, /let effectiveApiKey = savedSelection\?\.apiKey \|\| ''/);
  assert.match(chatPanelSource, /const effectiveApiKey = cloudApiKey\.trim\(\) \|\| apiKey/);
});


test('himalaya.manageSkills is a registered command accessible from the palette', () => {
  // #5: command entry in package.json + handler in extension.ts.
  assert.ok(packageJson.contributes.commands.some(function(cmd) { return cmd.command === 'himalaya.manageSkills'; }), 'command registered');
  assert.match(extensionSource, /'himalaya.manageSkills'/);
  assert.match(chatPanelSource, /async openSkills\(\): Promise<void> \{ await this\.surface\?\.manageSkills\(\)/);
});

test('execution gate blocks run callback when danger confirmation is denied', async () => {
  const { executeWithPermissionGate } = require(executionGatePath);

  let calls = 0;
  const result = await executeWithPermissionGate({
    permissionMode: 'danger-full-access',
    confirmDangerousRun: async () => false,
    onAllowed: async () => {
      calls += 1;
    },
  });

  assert.equal(result, 'cancelled');
  assert.equal(calls, 0);
});

test('execution gate allows run callback when approved or non-danger mode', async () => {
  const { executeWithPermissionGate } = require(executionGatePath);

  let calls = 0;
  const approved = await executeWithPermissionGate({
    permissionMode: 'danger-full-access',
    confirmDangerousRun: async () => true,
    onAllowed: async () => {
      calls += 1;
    },
  });

  const readOnly = await executeWithPermissionGate({
    permissionMode: 'read-only',
    confirmDangerousRun: async () => false,
    onAllowed: async () => {
      calls += 1;
    },
  });

  assert.equal(approved, 'allowed');
  assert.equal(readOnly, 'allowed');
  assert.equal(calls, 2);
});
