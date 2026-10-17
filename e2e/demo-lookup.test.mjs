import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { exitCode, formatTable, lookupAll, main, parseArgs, validateEntries, verdict } from './demo-lookup.mjs';

test('arguments default to the dev daemon and the curated list, and are validated', () => {
  assert.deepEqual(parseArgs([]), { base: 'http://127.0.0.1:8090', file: 'e2e/samples.lookup.json', timeoutSecs: 90 });
  assert.equal(parseArgs(['--base', 'http://staging.example:8090/']).base, 'http://staging.example:8090');
  assert.throws(() => parseArgs(['--base', 'ftp://x']), /http\(s\) origin/);
  assert.throws(() => parseArgs(['--timeout', '0']), /1-600/);
  assert.throws(() => parseArgs(['--write']), /usage/);
});

test('expectations accept one value or a list, and every failure is reported', () => {
  const entry = { kind: 'doi', uri: 'doi:10.1/x', type: ['ScholarlyArticle', 'CreativeWork'], status: 'ok' };
  assert.equal(verdict(entry, { type: 'CreativeWork', status: 'ok' }), null);
  assert.equal(verdict({ kind: 'osm', uri: 'x', type: 'Place' }, { type: 'Restaurant', typePath: ['Thing', 'Place', 'Restaurant'], status: 'ok' }), null);
  assert.match(verdict(entry, { type: 'WebPage', status: 'ok' }), /type WebPage/);
  assert.match(verdict(entry, { type: 'CreativeWork', status: 'missing' }), /status missing ≠ ok/);
  assert.equal(verdict(entry, { status: 'pending' }), 'still pending');
  assert.equal(verdict({ kind: 'web', uri: 'https://x.example/' }, { error: 'resolve HTTP 400' }), 'resolve HTTP 400');
  assert.equal(verdict({ kind: 'web', uri: 'https://x.example/' }, { type: null, status: 'missing' }), null);
  assert.throws(() => validateEntries([{ kind: 'x', uri: 'https://x.example/', type: 3 }]), /string/);
  assert.throws(() => validateEntries([]), /non-empty/);
});

test('the table aligns columns, clips long URIs and marks failures', () => {
  const table = formatTable([
    { kind: 'web', uri: `https://example.org/${'a'.repeat(80)}`, type: 'WebPage', source: 'web', status: 'ok', group: null, problem: null },
    { kind: 'doi', uri: 'doi:10.1/x', type: null, source: null, status: null, group: 'abcdef12×2', problem: 'resolve HTTP 400' },
  ]).split('\n');
  assert.match(table[0], /^kind\s+uri\s+type\s+source\s+status\s+group\s+check$/);
  assert.ok(table[2].includes('…') && table[2].endsWith('ok'));
  assert.ok(table[3].endsWith('FAIL: resolve HTTP 400') && table[3].includes('abcdef12×2'));
  const at = line => line.indexOf(line.includes('doi:') ? 'doi:' : line.includes('https:') ? 'https:' : 'uri');
  assert.ok(table.filter((_, i) => i !== 1).every(line => at(line) === at(table[0])), 'uri column aligned');
});

test('lookups only POST /v1/resolve and GET resources, paced and bounded', async () => {
  let clock = 0;
  const calls = [];
  const replies = {
    'POST /v1/resolve': [
      { status: 202, body: { resource_id: 'a', status: 'pending' } },
      { status: 200, body: { resource_id: 'b', status: 'ok', type: 'Book', document: { source: 'isbn' } } },
      { status: 400, body: { error: 'resolver is disabled' } },
      { status: 202, body: { resource_id: 'd', status: 'pending' } },
    ],
    'GET /v1/resources/a?fields=document': [
      { status: 429, body: {} },
      { status: 200, body: { resource_id: 'a', status: 'ok', type: 'Event', document: { source: 'luma' }, group: { id: '0123456789abcdef', member_count: 2 } } },
    ],
  };
  const fetch = async (url, init = {}) => {
    const key = `${init.method ?? 'GET'} ${new URL(url).pathname}${new URL(url).search}`;
    calls.push([clock, key]);
    const reply = key.startsWith('GET /v1/resources/d') ? { status: 200, body: { resource_id: 'd', status: 'pending' } } : replies[key].shift();
    return { status: reply.status, json: async () => reply.body };
  };
  const rows = await lookupAll([
    { kind: 'event', uri: 'https://luma.example/e', type: 'Event' },
    { kind: 'isbn', uri: 'isbn:9780140328721', type: 'Book' },
    { kind: 'web', uri: 'https://disabled.example/' },
    { kind: 'slow', uri: 'https://slow.example/' },
  ], { base: 'http://127.0.0.1:8090', timeoutSecs: 3, fetch, now: () => clock, sleep: async ms => { clock += ms; } });
  assert.deepEqual(rows.map(r => [r.kind, r.status, r.problem]), [
    ['event', 'ok', null], ['isbn', 'ok', null], ['web', null, 'resolve HTTP 400: resolver is disabled'], ['slow', 'pending', 'still pending'],
  ]);
  assert.equal(rows[0].group, '01234567×2');
  assert.ok(calls.every(([, key]) => key === 'POST /v1/resolve' || key.startsWith('GET /v1/resources/')));
  const posts = calls.filter(([, key]) => key === 'POST /v1/resolve').map(([at]) => at);
  assert.ok(posts.every((at, i) => i === 0 || at - posts[i - 1] >= 2100), `resolve calls are paced: ${posts}`);
  assert.ok(calls.filter(([, key]) => key.startsWith('GET /v1/resources/d')).length <= 3, 'polling stops at the deadline');
});

test('the curated list is valid and covers every resolver kind', async () => {
  const entries = validateEntries(JSON.parse(await readFile(new URL('./samples.lookup.json', import.meta.url), 'utf8')));
  const kinds = new Set(entries.map(e => e.kind));
  for (const kind of ['osm_node', 'osm_way', 'geo', 'web_jsonld', 'web_opengraph', 'doi', 'doi_page', 'isbn', 'isbn_page',
    'youtube', 'vimeo', 'podcast', 'nostr_note', 'nip52_event', 'luma', 'meetup', 'btcmap', 'ics', 'activitypub_note']) {
    assert.ok(kinds.has(kind), kind);
  }
  assert.equal(new Set(entries.map(e => e.uri)).size, entries.length);
});

test('a 429 on /v1/resolve is retried with backoff, honouring Retry-After, and is bounded', async () => {
  let clock = 0;
  const waits = [];
  const statuses = [429, 429, 200];
  const fetch = async (url, init = {}) => {
    if (init.method === 'POST') {
      const status = statuses.shift() ?? 429;
      return { status, headers: { get: name => (name === 'retry-after' && status === 429 && statuses.length === 1 ? '7' : null) }, json: async () => (status === 200 ? { resource_id: 'a', status: 'ok', type: 'WebPage' } : { error: 'busy' }) };
    }
    throw new Error(`unexpected ${url}`);
  };
  const sleep = async ms => { waits.push(ms); clock += ms; };
  const [row] = await lookupAll([{ kind: 'web', uri: 'https://x.example/' }], { base: 'http://h:1', timeoutSecs: 3, fetch, sleep, now: () => clock });
  assert.equal(row.problem, null);
  assert.deepEqual(waits, [2100, 7000]);
  statuses.length = 0;
  const [busy] = await lookupAll([{ kind: 'web', uri: 'https://x.example/' }], { base: 'http://h:1', timeoutSecs: 3, fetch, sleep, now: () => clock, resolveRetries: 2 });
  assert.equal(busy.problem, 'resolve HTTP 429: busy');
});

test('exit codes: 0 when all rows pass, 1 on any failure, 2 on usage or file errors', async () => {
  assert.equal(exitCode([{ problem: null }, { problem: null }]), 0);
  assert.equal(exitCode([{ problem: null }, { problem: 'still pending' }]), 1);
  const quiet = { out: () => {}, err: () => {} };
  const lookup = async entries => entries.map(e => ({ kind: e.kind, uri: e.uri, status: 'ok', problem: e.kind === 'bad' ? 'type x' : null }));
  const read = async () => JSON.stringify([{ kind: 'ok', uri: 'https://a.example/' }, { kind: 'bad', uri: 'https://b.example/' }]);
  assert.equal(await main([], { ...quiet, lookup, read }), 1);
  assert.equal(await main([], { ...quiet, lookup, read: async () => '[{"kind":"ok","uri":"https://a.example/"}]' }), 0);
  assert.equal(await main(['--write'], quiet), 2);
  assert.equal(await main([], { ...quiet, read: async () => 'not json' }), 2);
  assert.equal(await main(['--help'], quiet), 0);
});
