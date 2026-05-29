const path = require('path');
const { HimalayaCli } = require('../out/cli');

const mockContext = { extensionPath: __dirname, globalStorageUri: { fsPath: path.join(__dirname, 'storage') } };
const mockOutput = { appendLine: (s) => console.log('[OUT]', s), append: (s) => process.stdout.write(s), show: () => {} };

const cli = new HimalayaCli(mockContext, mockOutput);

function show(inv) {
  console.log(JSON.stringify(inv, null, 2));
}

console.log('Test: native binary (no .py)');
show(cli.buildExecInvocation('/usr/local/bin/himalaya', ['prompt','hello']));

console.log('\nTest: python script path (should use interpreter)');
show(cli.buildExecInvocation('/home/user/project/src/main.py', ['prompt','hello']));

console.log('\nTest: script path ending with .PY (case-insensitive)');
show(cli.buildExecInvocation('/home/user/project/src/main.PY', ['prompt','hello']));

console.log('\nTest: non .py path with args');
show(cli.buildExecInvocation('/home/user/project/himalaya-cli', ['--output-format','stream-json']));
