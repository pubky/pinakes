import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { operatorContact, dropRefusedPages, expectCard, findsEvery, freshen, instanceAgent, judgeRecipes, liveRecipes, mcpAnswer, newestLink, ownTags, psql, rereadMoved, socialAgrees, userAgent, withTags, withoutStackSkipped } from './harness.mjs';
test('an interrupted run starts no recipe stack, and one interrupted while it runs is stopped', { timeout: 5000 }, async () => {
  // A stand-in for `e2e_stack --live-recipes`: it prints its line and closes when told to, and
  // closes as a signal ends it when killed.
  const stacks = [];
  const start = () => {
    const child = Object.assign(new EventEmitter(), { stdout: new PassThrough(), killed: [] });
    child.kill = signal => { child.killed.push(signal); child.stdout.end(); setImmediate(() => child.emit('close', null, signal)); };
    child.finish = (code, line) => { if (line) child.stdout.write(`${line}\n`); child.stdout.end(); setImmediate(() => child.emit('close', code, null)); };
    stacks.push(child); return child;
  };
  // Interrupted during the flow: the flow returned its report, and no stack starts.
  const before = new AbortController(); before.abort(new Error('Interrupted'));
  await assert.rejects(liveRecipes(start, before.signal), /Interrupted/);
  assert.equal(stacks.length, 0);
  // Interrupted while the stack runs: it is sent SIGTERM, and the sweep throws.
  const during = new AbortController();
  const sweep = liveRecipes(start, during.signal);
  during.abort(new Error('Interrupted'));
  await assert.rejects(sweep, /Interrupted/);
  assert.deepEqual(stacks[0].killed, ['SIGTERM']);
  // Not interrupted: its line is judged; a stack a signal ended is named by it.
  const done = liveRecipes(start, new AbortController().signal);
  stacks[1].finish(0, JSON.stringify({ recipes: [{ name: 'ok', uri: 'https://ok.example/e/1', want: 'Event', status: 'ok', type: 'Event' }] }));
  assert.deepEqual(await done, { checked: 1, failed: [], skipped: [] });
  const gone = liveRecipes(start, new AbortController().signal);
  stacks[2].kill('SIGKILL');
  await assert.rejects(gone, /exited with SIGKILL/);
});
test('a failed psql call never shows the cluster\'s password', () => {
  for (const cluster of ['postgresql://e2e:Dummy-Secret-42@127.0.0.1:1/postgres', 'postgresql://e2e:Stray%zzSecret-42@127.0.0.1:1/postgres', 'postgresql:///postgres?host=127.0.0.1&port=1&user=e2e&password=Query-Secret-42']) {
    assert.throws(() => psql(cluster, 'postgres', 'SELECT 1'), error => {
      assert.doesNotMatch(error.stack, /Secret-42/); assert.match(error.message, /^psql on postgresql:.*\*\*\*/);
      assert.deepEqual(Object.keys(error), []); return true;
    });
  }
  // A URL that does not parse (a port out of range): the parser's error would carry it whole.
  assert.throws(() => psql('postgresql://e2e:Dummy-Secret-42@127.0.0.1:99999/postgres', 'postgres', 'SELECT 1'), error => {
    assert.doesNotMatch(error.stack, /Secret-42/); assert.equal(error.message, 'psql: the cluster URL does not parse');
    assert.deepEqual(Object.keys(error), []); assert.equal(error.cause, undefined); return true;
  });
});
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
test('the social tool is judged against the batch view, five tags at most', () => {
  const tag = label => ({ label, taggers: ['me'] });
  const five = ['a', 'b', 'c', 'd', 'e'].map(tag);
  const row = (uri, tags) => ({ details: { id: 'n', uri }, tags });
  const item = (tags, mine = true) => ({ uri: 'x', tags: tags.map(t => ({ label: t.label, taggers_count: 1, viewer_tagged: mine && t.label === 'run' })) });
  const [listed, full] = ['https://example.org/listed', 'https://example.org/full'];
  const hydrated = [row(listed, [tag('run'), ...five.slice(1)]), row(full, five)];
  socialAgrees(item([tag('run')]), hydrated, listed, 'run');
  // Five established tags push the run's out of both answers: the tool agrees with the view.
  socialAgrees(item([...five].reverse()), hydrated, full, 'run');
  assert.throws(() => socialAgrees(item(five), hydrated, listed, 'run'), /returns the run's tag/);
  assert.throws(() => socialAgrees(item([tag('run')], false), hydrated, listed, 'run'), /viewer tagged the run's label/);
  assert.throws(() => socialAgrees(item(five.slice(1)), hydrated, full, 'run'), /answers as Nexus's batch view/);
  assert.throws(() => socialAgrees(undefined, hydrated, full, 'run'), /Missing social answer/);
  assert.throws(() => socialAgrees(item(five), hydrated, 'https://example.org/absent', 'run'), /Missing Nexus hydration/);
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
test('a live recipe must be a card of the type it promises or one under it, unless this host could not check it; one the API refuses fails', () => {
  const r = (name, status, type, error, want = 'Event', type_path = type && ['Thing', type]) => ({ name, uri: `https://${name}.example/e/1`, want, status, type, type_path, error });
  const judged = judgeRecipes([
    r('ok', 'ok', 'Event', null),
    r('city', 'ok', 'City', null, 'Place', ['Thing', 'Place', 'City']),
    r('page', 'ok', 'WebPage', null),
    r('gone', 'pending', null, 'http_4xx', 'DataFeed'),
    r('dns', 'pending', null, 'dns'),
    r('slow', 'pending', null, 'timeout'),
    r('down', 'pending', null, 'http_5xx'),
    r('walled', 'unreachable', null, 'wall'),
    r('busy', 'pending', null, 'rate_limited'),
    { name: 'odd', uri: 'nostr:naddr1x', want: 'Event', status: 'refused', error: 'no resolver available for this URI' },
    { name: 'off', uri: 'geo:46.0,8.9', want: 'Place', status: 'refused', error: 'resolver is disabled' },
  ]);
  assert.equal(judged.checked, 11);
  assert.deepEqual(judged.failed, [
    { name: 'page', uri: 'https://page.example/e/1', reason: 'card ok WebPage, the recipe promises Event' },
    { name: 'gone', uri: 'https://gone.example/e/1', reason: 'card pending (http_4xx), the recipe promises DataFeed' },
    { name: 'down', uri: 'https://down.example/e/1', reason: 'card pending (http_5xx), the recipe promises Event' },
    { name: 'odd', uri: 'nostr:naddr1x', reason: 'the API refuses it (no resolver available for this URI), the recipe promises Event' },
    { name: 'off', uri: 'geo:46.0,8.9', reason: 'the API refuses it (resolver is disabled), the recipe promises Place' },
  ]);
  assert.deepEqual(judged.skipped, [
    { name: 'dns', uri: 'https://dns.example/e/1', reason: 'dns.example could not be reached from this host (dns)' },
    { name: 'slow', uri: 'https://slow.example/e/1', reason: 'slow.example could not be reached from this host (timeout)' },
    { name: 'walled', uri: 'https://walled.example/e/1', reason: 'walled.example answers this host with a bot wall' },
    { name: 'busy', uri: 'https://busy.example/e/1', reason: 'busy.example asked this host to wait (rate limited)' },
  ]);
});
test('a card that moved to another ID must meet the sample again, and replaces every reported field', async () => {
  const tag = { kind: 'podcast', uri: 'https://index.example/p/1', type: 'PodcastSeries', site: 'Index', name: 'Darknet Diaries' };
  const first = { id: 'a', status: 'ok', type: 'PodcastSeries', site: 'Index', name: 'Darknet Diaries' };
  const fresh = () => ({ docs: new Map([['podcast', first]]), report: { samples: [{ kind: 'podcast', uri: tag.uri, resource_id: 'a', type: 'PodcastSeries', status: 'ok', site: 'Index' }] } });
  const check = root => (t, card) => expectCard(t, card, async () => root, 50);
  // The root names something else: refused, and nothing is replaced.
  const wrong = { id: 'b', status: 'ok', type: 'PodcastSeries', site: 'Index', name: 'Another show' };
  let { docs, report } = fresh();
  await assert.rejects(rereadMoved([tag], docs, report, async () => wrong, check(wrong)), /name/);
  assert.equal(docs.get('podcast').id, 'a'); assert.equal(report.samples[0].resource_id, 'a');
  // Its site differs: refused too.
  ({ docs, report } = fresh());
  const elsewhere = { ...first, id: 'b', site: 'Other' };
  await assert.rejects(rereadMoved([tag], docs, report, async () => elsewhere, check(elsewhere)), /site/);
  // A root that meets the sample replaces the card and every reported field.
  ({ docs, report } = fresh());
  const root = { id: 'b', status: 'ok', type: 'PodcastSeries', site: 'Index', name: 'Darknet Diaries' };
  await rereadMoved([tag], docs, report, async () => root, check(root));
  assert.equal(docs.get('podcast'), root);
  assert.deepEqual(report.samples[0], { kind: 'podcast', uri: tag.uri, resource_id: 'b', type: 'PodcastSeries', status: 'ok', site: 'Index' });
  // The same ID, or a card not yet ok, changes nothing.
  ({ docs, report } = fresh());
  await rereadMoved([tag], docs, report, async () => first, check(first));
  await rereadMoved([tag], docs, report, async () => ({ ...root, status: 'pending' }), check(root));
  assert.equal(docs.get('podcast').id, 'a');
});
test('the card kept after waiting for a name meets every expectation, not only the name', async () => {
  const tag = { kind: 'music', uri: 'https://music.example/a/1', type: 'MusicAlbum', site: 'Music', name: 'OK Computer' };
  const first = { id: 'a', status: 'ok', type: 'MusicAlbum', site: 'Music', name: 'Untitled' };
  // The name arrives on a card from elsewhere (another site, another type, not ok): refused.
  for (const [named, check] of [
    [{ ...first, name: 'OK Computer', site: 'Other' }, /site/],
    [{ ...first, name: 'OK Computer', type: 'WebPage' }, /type/],
    [{ ...first, name: 'OK Computer', status: 'pending' }, /status/],
  ]) {
    await assert.rejects(expectCard(tag, first, async () => named, 50), check);
  }
  const good = { ...first, name: 'OK Computer' };
  assert.equal(await expectCard(tag, first, async () => good, 50), good);
  // Through the re-read: a moved root whose named card is on another site.
  const docs = new Map([['music', first]]);
  const report = { samples: [{ kind: 'music', uri: tag.uri, resource_id: 'a', type: 'MusicAlbum', status: 'ok', site: 'Music' }] };
  const root = { ...first, id: 'b' };
  const elsewhere = { ...first, id: 'b', name: 'OK Computer', site: 'Other' };
  await assert.rejects(rereadMoved([tag], docs, report, async () => root, (t, c) => expectCard(t, c, async () => elsewhere, 50)), /site/);
  assert.equal(docs.get('music').id, 'a'); assert.equal(report.samples[0].site, 'Music');
});
test('a card that moved after the type search must still be of the sample\'s type, ancestors included', async () => {
  // An Article sample whose root is a WebPage, read again after the type search.
  const tag = { kind: 'article', uri: 'https://x.example/a', type: 'Article' };
  const first = { id: 'a', status: 'ok', type: 'Article' };
  const isA = async (c, type) => c.type === type || (c.type === 'MusicEvent' && type === 'Event');
  const check = (t, c) => expectCard(t, c, async () => c, 50, undefined, isA);
  const docs = new Map([['article', first]]);
  const report = { samples: [{ kind: 'article', uri: tag.uri, resource_id: 'a', type: 'Article', status: 'ok' }] };
  await assert.rejects(rereadMoved([tag], docs, report, async () => ({ id: 'b', status: 'ok', type: 'WebPage' }), check), /type/);
  assert.equal(docs.get('article').id, 'a'); assert.equal(report.samples[0].type, 'Article');
  // A subtype is the type: an Event sample's root may be a MusicEvent.
  const event = { kind: 'event', uri: 'https://x.example/e', type: 'Event' };
  const events = new Map([['event', { id: 'a', status: 'ok', type: 'Event' }]]);
  const eventReport = { samples: [{ kind: 'event', uri: event.uri, resource_id: 'a', type: 'Event', status: 'ok' }] };
  const root = { id: 'b', status: 'ok', type: 'MusicEvent' };
  await rereadMoved([event], events, eventReport, async () => root, check);
  assert.equal(events.get('event'), root); assert.equal(eventReport.samples[0].type, 'MusicEvent');
  // Without a word on ancestry, only the exact type passes.
  await assert.rejects(expectCard(event, root, async () => root, 50), /type/);
});

test('a card whose link became a spelling of another root before the first type search is judged as that root', async () => {
  // The sample's first card (a MusicEvent, id a) is never listed under Event, since
  // the link became a spelling of root b meanwhile; b is listed. `isA` as runFlow answers it.
  const tag = { kind: 'event', uri: 'https://x.example/e', type: 'Event' };
  const a = { id: 'a', status: 'ok', type: 'MusicEvent' };
  const b = { id: 'b', status: 'ok', type: 'MusicEvent' };
  const listed = new Set(['b']);
  const roots = new Map([[tag.uri, b]]);
  const asked = [];
  const isA = async (c, type, uri) => {
    asked.push(c.id);
    if (c.type === type || listed.has(c.id)) return true;
    const root = roots.get(uri);
    return root && root.status === 'ok' && root.id !== c.id && root;
  };
  assert.equal(await expectCard(tag, a, async () => b, 50, undefined, isA), b);
  assert.deepEqual(asked, ['a', 'b'], 'the root is judged in its turn');
  // The root must meet the sample itself: one not listed either is refused, and one not yet ok
  // is not taken (the wait goes on, and ends as the type's).
  roots.set(tag.uri, { id: 'c', status: 'ok', type: 'WebPage' });
  await assert.rejects(expectCard(tag, a, async () => b, 50, undefined, isA), /type/);
  roots.set(tag.uri, { ...b, status: 'pending' });
  await assert.rejects(expectCard(tag, a, async () => b, 50, undefined, isA), /type/);
  // Through the first read's path as runFlow uses it: the root replaces the card kept and every reported field.
  roots.set(tag.uri, b);
  const docs = new Map(); const report = { samples: [{ kind: 'event', uri: tag.uri, resource_id: 'a', type: 'MusicEvent', status: 'ok' }] };
  const kept = await expectCard(tag, a, async () => b, 50, undefined, isA);
  docs.set('event', kept); Object.assign(report.samples[0], { resource_id: kept.id, type: kept.type, status: kept.status, site: kept.site });
  assert.equal(docs.get('event').id, 'b'); assert.equal(report.samples[0].resource_id, 'b');
});
test('the preflight User-Agent is the one Pinakes\' egress sends', () => {
  // `resolver::egress::user_agent` and `USER_AGENT`.
  assert.equal(userAgent('0.1.0', 'ops@example.org'), 'pinakes/0.1.0 (+https://pubky.app; ops@example.org)');
  assert.equal(userAgent('0.2.0', ''), 'pinakes/0.2.0 (+https://pubky.app)');
});
test('a running instance\'s preflight sends the User-Agent its /v1/info reports', () => {
  // The instance's contact may be set in its file alone: the run's contact is never a substitute.
  assert.equal(instanceAgent({ version: '0.1.0', user_agent: 'pinakes/0.1.0 (+https://pubky.app; ops@example.org)' }), 'pinakes/0.1.0 (+https://pubky.app; ops@example.org)');
  assert.throws(() => instanceAgent({ version: '0.1.0' }), /user_agent/);
  assert.throws(() => instanceAgent({ version: '0.1.0', user_agent: 'curl/8' }), /user_agent/);
});

test('an MCP answer is the JSON body, or the last data event of a stream', () => {
  assert.deepEqual(mcpAnswer('{"jsonrpc":"2.0","id":1,"result":{}}', 'application/json'), { jsonrpc: '2.0', id: 1, result: {} });
  const stream = 'event: message\ndata: {"jsonrpc":"2.0","method":"notifications/progress"}\n\nevent: message\ndata: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n\n';
  assert.deepEqual(mcpAnswer(stream, 'text/event-stream'), { jsonrpc: '2.0', id: 1, result: { ok: true } });
  assert.throws(() => mcpAnswer('event: ping\n\n', 'text/event-stream'), /no data/);
});

test('a live sample whose needed source the build lacks is not tagged and is named under skipped', () => {
  const samples = [
    { kind: 'tidal', uri: 'https://tidal.com/album/1', needs: 'tidal' },
    { kind: 'web', uri: 'https://web.example/' },
    { kind: 'isbn_page', uri: 'https://openlibrary.org/books/OL1M', needs: 'books' },
  ];
  const preflight = [{ kind: 'index', uri: 'https://index.example/', reason: 'HTTP 403' }];
  const stack = [{ kind: 'tidal', uri: 'https://tidal.com/album/1', reason: 'needs tidal, which this build does not have' }];
  const { samples: kept, skipped } = withoutStackSkipped(samples, preflight, stack);
  assert.deepEqual(kept.map(s => s.kind), ['web', 'isbn_page']);
  assert.deepEqual(skipped.map(s => [s.kind, s.reason]), [['index', 'HTTP 403'], ['tidal', 'needs tidal, which this build does not have']]);
  // A stack that left nothing out keeps every sample and adds nothing.
  assert.deepEqual(withoutStackSkipped(samples, [], undefined), { samples, skipped: [] });
});

test('a search check re-reads a card that became an alias and finds its root', async () => {
  // The run cached event `a`; it became an alias, and searches now answer its root `b`.
  const tag = { kind: 'event', uri: 'https://events.example/a', type: 'Event' };
  const docs = new Map([['event', { id: 'a' }]]);
  const results = [{ items: [{ id: 'b' }] }, { items: [{ id: 'b' }] }];
  let reads = 0;
  const reread = async missing => {
    for (const t of missing) { reads++; docs.set(t.kind, { id: 'b' }); }
  };
  const found = await findsEvery('type=Event', async () => results.shift(), [tag], docs, reread, 5000);
  assert.deepEqual(found, { items: [{ id: 'b' }] });
  assert.equal(reads, 1);
  assert.equal(docs.get('event').id, 'b');
  // A card that is never found fails the check at its timeout.
  const stuck = new Map([['event', { id: 'a' }]]);
  await assert.rejects(findsEvery('type=Event', async () => ({ items: [] }), [tag], stuck, async () => {}, 300), /type=Event/);
});
test('a live run needs the operator\'s contact: there is no default', () => {
  for (const env of [{}, { PINAKES__HTTP__CONTACT: '' }, { PINAKES__HTTP__CONTACT: '  ' }]) {
    assert.throws(() => operatorContact(env), /PINAKES__HTTP__CONTACT/);
  }
  assert.equal(operatorContact({ PINAKES__HTTP__CONTACT: ' ops@example.org ' }), 'ops@example.org');
});
