const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const root = path.resolve(__dirname, '..');
const attachmentPathsPath = path.join(root, 'out', 'attachmentPaths.js');
const {
  extractPromptAttachmentReferences,
  extractReferencePathCandidate,
  prepareAttachmentDescriptors,
  resolveReferenceString,
} = require(attachmentPathsPath);

test('resolve absolute path remains unchanged for existing file', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'himalaya-'));
  const filename = '基于相关性与问题特性的.pdf';
  const filePath = path.join(tmp, filename);
  fs.writeFileSync(filePath, 'x');
  assert.equal(resolveReferenceString(filePath), filePath);
});

test('expand ~ to home dir for existing file', () => {
  const home = os.homedir();
  const tmpDir = path.join(home, 'himalaya-test-dir');
  fs.mkdirSync(tmpDir, { recursive: true });
  const filename = 'desktop-file.pdf';
  const filePath = path.join(tmpDir, filename);
  fs.writeFileSync(filePath, 'x');

  const tilde = '~' + path.sep + path.relative(home, filePath);
  assert.equal(resolveReferenceString(tilde), filePath);
});

test('decode percent-encoded filenames', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'himalaya-'));
  const filename = 'file name with spaces.txt';
  const filePath = path.join(tmp, filename);
  fs.writeFileSync(filePath, 'x');
  const encoded = encodeURIComponent(filename);
  const res = resolveReferenceString(path.join(tmp, encoded));
  assert.equal(res, filePath);
});

test('non-existent paths return empty string', () => {
  const res = resolveReferenceString('/this/path/does/not/exist/hopefully.txt');
  assert.equal(res, '');
});

test('prepareAttachmentDescriptors returns metadata and canonical paths', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'himalaya-attachments-'));
  const filePath = path.join(tmp, 'notes.md');
  fs.writeFileSync(filePath, '# notes');

  const prepared = prepareAttachmentDescriptors(['notes.md'], tmp, 'reference');
  assert.equal(prepared.paths.length, 1);
  assert.equal(prepared.descriptors.length, 1);
  assert.equal(prepared.rejections.length, 0);
  assert.equal(prepared.descriptors[0].path, fs.realpathSync(filePath));
  assert.equal(prepared.descriptors[0].displayName, 'notes.md');
  assert.equal(prepared.descriptors[0].extension, '.md');
  assert.equal(prepared.descriptors[0].mediaKind, 'text');
  assert.equal(prepared.descriptors[0].source, 'reference');
  assert.equal(prepared.descriptors[0].sizeBytes, 7);
  assert.equal(typeof prepared.descriptors[0].modifiedMs, 'number');
});

test('prepareAttachmentDescriptors rejects sensitive, duplicate, missing and directory inputs', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'himalaya-attachments-'));
  const filePath = path.join(tmp, 'safe.txt');
  const secretPath = path.join(tmp, 'credentials.txt');
  fs.writeFileSync(filePath, 'safe');
  fs.writeFileSync(secretPath, 'secret');

  const prepared = prepareAttachmentDescriptors([
    filePath,
    filePath,
    secretPath,
    path.join(tmp, 'missing.txt'),
    tmp,
  ], tmp, 'picker');

  assert.deepEqual(prepared.paths, [fs.realpathSync(filePath)]);
  assert.deepEqual(
    prepared.rejections.map((rejection) => rejection.reason),
    ['duplicate', 'sensitive', 'not_found', 'not_file']
  );
});

test('extractReferencePathCandidate unwraps nested reference shapes', () => {
  assert.equal(extractReferencePathCandidate({ fsPath: '/tmp/a.txt' }), '/tmp/a.txt');
  assert.equal(extractReferencePathCandidate({ uri: { fsPath: '/tmp/b.txt' } }), '/tmp/b.txt');
  assert.equal(extractReferencePathCandidate({ value: { path: '/tmp/c.txt' } }), '/tmp/c.txt');
});


test('extractPromptAttachmentReferences reads file labels from prompt text', () => {
  assert.deepEqual(
    extractPromptAttachmentReferences('请总结论文\nfile: ES20251121_O_editing.pdf'),
    ['ES20251121_O_editing.pdf']
  );
  assert.deepEqual(
    extractPromptAttachmentReferences('附件：/tmp/paper with spaces.pdf\n文件: notes.md'),
    ['/tmp/paper with spaces.pdf', 'notes.md']
  );
});
