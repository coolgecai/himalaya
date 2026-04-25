const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const chatPanelPath = path.join(root, 'src', 'chatPanel.ts');
const extensionPath = path.join(root, 'src', 'extension.ts');
const packageJsonPath = path.join(root, 'package.json');
const permissionPolicyPath = path.join(root, 'out', 'permissionPolicy.js');
const streamProtocolPath = path.join(root, 'out', 'streamProtocol.js');
const executionGatePath = path.join(root, 'out', 'executionGate.js');
const chatPanelSource = fs.readFileSync(chatPanelPath, 'utf8');
const extensionSource = fs.readFileSync(extensionPath, 'utf8');
const packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));

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
  assert.match(chatPanelSource, /const args:\s*string\[\]\s*=\s*\['--output-format',\s*'stream-json',\s*'--allow-broad-cwd'\]/);
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

test('assistant placeholder message is created before streaming chunks', () => {
  assert.match(chatPanelSource, /await this\.history\.appendMessage\(record\.id,\s*\{\s*role:\s*'assistant',\s*text:\s*''/s);
  assert.match(chatPanelSource, /replaceAssistantTail\(record\.id, assistantText\)/);
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
    parseStreamEventLine,
    isKnownStreamEventType,
    readProtocolVersion,
    classifyProtocolVersion,
  } = require(streamProtocolPath);

  const ok = parseStreamEventLine('{"type":"text_delta","text":"hi"}');
  assert.equal(ok.ok, true);
  assert.equal(ok.event.type, 'text_delta');
  assert.equal(ok.event.text, 'hi');

  const malformed = parseStreamEventLine('{"foo":1}');
  assert.equal(malformed.ok, false);
  assert.equal(malformed.reason, 'invalid-shape');

  const nonJson = parseStreamEventLine('not-json');
  assert.equal(nonJson.ok, false);
  assert.equal(nonJson.reason, 'not-json');

  assert.equal(isKnownStreamEventType('tool_use'), true);
  assert.equal(isKnownStreamEventType('decisioning_event'), true);
  assert.equal(isKnownStreamEventType('unknown_event'), false);

  assert.equal(readProtocolVersion({ type: 'message_start', protocol_version: 1 }), 1);
  assert.equal(readProtocolVersion({ type: 'message_start', schema_version: 2 }), 2);
  assert.equal(readProtocolVersion({ type: 'message_start' }), undefined);

  assert.equal(classifyProtocolVersion(undefined), 'missing');
  assert.equal(classifyProtocolVersion(STREAM_PROTOCOL_VERSION), 'match');
  assert.equal(classifyProtocolVersion(STREAM_PROTOCOL_VERSION + 1), 'mismatch');
});

test('decisioning visualization hooks remain wired', () => {
  assert.match(chatPanelSource, /case 'decisioning_event'/);
  assert.match(chatPanelSource, /addDecisioningEvent/);
  assert.match(chatPanelSource, /msg decisioning-step/);
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
