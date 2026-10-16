import { test } from 'node:test';
import assert from 'node:assert/strict';
import { withTags } from './harness.mjs';
test('a PUT that commits then times out is included in cleanup', async () => {
  const stored = new Set(), report = {}, journal = [];
  const storage = { exists: async p => stored.has(p), putJson: async p => { stored.add(p); if (p === 'second') throw new Error('timeout'); }, delete: async p => stored.delete(p) };
  await assert.rejects(withTags(storage, [{ path: 'first' }, { path: 'second' }], async () => assert.fail('should not reach assertions'), report, async t => journal.push(t.map(v => v.path))), /timeout/);
  assert.equal(stored.size, 0); assert.equal(report.cleanup.length, 2); assert.deepEqual(journal[1], ['first', 'second']);
});
test('assertion failure cleans up without touching existing paths', async () => {
  const stored = new Set(['existing']), report = {};
  const storage = { exists: async p => stored.has(p), putJson: async p => stored.add(p), delete: async p => stored.delete(p) };
  await assert.rejects(withTags(storage, [{ path: 'new' }], async () => { throw new Error('assertion'); }, report), /assertion/);
  assert.deepEqual([...stored], ['existing']);
  await assert.rejects(withTags(storage, [{ path: 'existing' }], async () => {}, report), /overwrite/);
  assert.deepEqual([...stored], ['existing']);
});
