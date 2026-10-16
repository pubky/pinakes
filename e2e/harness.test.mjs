import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ownTags, withTags } from './harness.mjs';
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
test('a full by_ids tag list is confirmed through the resource tag route', async () => {
  const tag = (label, taggers = ['me']) => ({ label, taggers });
  const others = ['a', 'b', 'c', 'd', 'e'].map(l => tag(l));
  const hydrated = [{ details: { id: 'own' }, tags: [tag('run')] }, { details: { id: 'full' }, tags: others }, { details: { id: 'short' }, tags: others.slice(1) }];
  const asked = [];
  const tagsOf = async id => { asked.push(id); return { tags: [...others, tag('run')] }; };
  assert.equal(await ownTags(hydrated, ['own', 'full'], 'run', 'me', tagsOf), 1);
  assert.deepEqual(asked, ['full']);
  await assert.rejects(ownTags(hydrated, ['short'], 'run', 'me', tagsOf), /Missing Nexus hydration: short/);
  await assert.rejects(ownTags(hydrated, ['absent'], 'run', 'me', tagsOf), /Missing Nexus hydration: absent/);
  await assert.rejects(ownTags(hydrated, ['full'], 'run', 'someone', tagsOf), /Missing own tag on Nexus: full/);
});
