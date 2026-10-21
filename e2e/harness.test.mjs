import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dropRefusedPages, freshen, newestLink, ownTags, withTags } from './harness.mjs';
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
test('a full by-URI tag list is confirmed through the resource tag route', async () => {
  const tag = (label, taggers = ['me']) => ({ label, taggers });
  const others = ['a', 'b', 'c', 'd', 'e'].map(l => tag(l));
  const row = (id, tags) => ({ details: { id, uri: `https://example.org/${id}` }, tags });
  const [own, full, short] = [row('own', [tag('run')]), row('full', others), row('short', others.slice(1))];
  const asked = [];
  const tagsOf = async id => { asked.push(id); return { tags: [...others, tag('run')] }; };
  // Rows are matched by their URI, in any order.
  assert.equal(await ownTags([full, own], ['https://example.org/own', 'https://example.org/full'], 'run', 'me', tagsOf), 1);
  assert.deepEqual(asked, ['full'], 'the tag route takes Nexus\'s ID from its own row');
  await assert.rejects(ownTags([short], ['https://example.org/short'], 'run', 'me', tagsOf), /Missing Nexus hydration: https:\/\/example.org\/short/);
  await assert.rejects(ownTags([own], ['https://example.org/own', 'https://example.org/absent'], 'run', 'me', tagsOf), /Missing Nexus hydration: https:\/\/example.org\/absent/);
  await assert.rejects(ownTags([full], ['https://example.org/full'], 'run', 'someone', tagsOf), /Missing own tag on Nexus: https:\/\/example.org\/full/);
});
test('a latest_in sample takes its feed\'s newest item', async () => {
  const rss = '<rss><channel><link>https://x.example</link><item><title>New</title><link><![CDATA[https://x.example/news/new]]></link></item><item><link>https://x.example/news/old</link></item></channel></rss>';
  assert.equal(newestLink(rss), 'https://x.example/news/new');
  assert.equal(newestLink('<rss><channel><link>https://x.example</link></channel></rss>'), undefined);
  const samples = [{ kind: 'feed', uri: 'https://x.example/news/gone', latest_in: 'https://x.example/rss.xml' }, { kind: 'web', uri: 'https://y.example/' }];
  const asked = [];
  await freshen(samples, async url => { asked.push(url); return rss; });
  assert.deepEqual(asked, ['https://x.example/rss.xml']);
  assert.deepEqual(samples.map(s => s.uri), ['https://x.example/news/new', 'https://y.example/']);
  await assert.rejects(freshen([{ kind: 'feed', latest_in: 'https://x.example/rss.xml' }], async () => '<rss/>'), /No item/);
});
test('a needs_page sample whose site refuses this host is left out with the reason', async () => {
  const samples = [{ kind: 'index', uri: 'https://index.example/p/1', needs_page: true }, { kind: 'open', uri: 'https://open.example/p/2', needs_page: true }, { kind: 'web', uri: 'https://web.example/' }];
  const status = { 'https://index.example/p/1': 403, 'https://open.example/p/2': 200 };
  const asked = [];
  const kept = await dropRefusedPages(samples, async url => { asked.push(url); return status[url]; });
  assert.deepEqual(asked, ['https://index.example/p/1', 'https://open.example/p/2']);
  assert.deepEqual(kept.samples.map(s => s.kind), ['open', 'web']);
  assert.deepEqual(kept.skipped, [{ kind: 'index', uri: 'https://index.example/p/1', reason: "index.example refuses this host's page fetches (HTTP 403)" }]);
  // A sample another one joins is never left out.
  await assert.rejects(dropRefusedPages([...samples, { kind: 'pair', uri: 'https://x.example/', group_with: 'index' }], async url => status[url] ?? 200), /joined by another/);
});
