import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { setTimeout as sleep } from 'node:timers/promises';
import { PubkySpecsBuilder } from 'pubky-app-specs';

export async function json(url, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(url, { method, headers: body === undefined ? {} : { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15000) });
  assert(response.ok, `${method} ${new URL(url).pathname}: HTTP ${response.status}`);
  return response.status === 204 ? null : response.json();
}
export async function waitFor(label, action, timeout, signal) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    signal?.throwIfAborted();
    const value = await action();
    if (value) return value;
    await sleep(250, undefined, { signal });
  }
  throw new Error(`Timed out: ${label}`);
}
// Register before each write: a timed-out PUT can still have committed remotely.
export async function withTags(storage, tags, action, report, journal = async () => {}) {
  const attempted = [];
  try {
    for (const tag of tags) {
      assert(!await storage.exists(tag.path), 'Refusing to overwrite an existing tag');
      attempted.push(tag); await journal(attempted);
      await storage.putJson(tag.path, tag.value);
    }
    return await action();
  } finally {
    report.cleanup = [];
    for (const tag of attempted) {
      let error;
      for (let attempt = 0; attempt < 3; attempt++) {
        try { if (await storage.exists(tag.path)) await storage.delete(tag.path); error = null; break; }
        catch { error = 'storage delete failed'; await sleep(500); }
      }
      report.cleanup.push({ path: tag.path, deleted: !error, ...(error ? { error } : {}) });
    }
    await journal(attempted, report.cleanup);
  }
}
export async function runFlow({ storage, author, samples, pinakes, nexus, positiveVenue = false, timeout = 120000, journal, signal }) {
  const report = { at: new Date().toISOString(), run: randomUUID(), author, samples: [], assertions: [], cleanup: [] };
  const label = `e2e-${report.run.replaceAll("-", "").slice(0, 16)}`;
  const builder = new PubkySpecsBuilder(author);
  const tags = samples.map(sample => {
    const result = builder.createTag(sample.uri, label);
    const tag = { ...sample, path: `/pub/pinakes.e2e/tags/${result.meta.id}`, value: result.tag.toJson() };
    result.free(); return tag;
  });
  builder.free();
  let cursor = 0;
  const feed = new Map();
  const head = await fetch(`${nexus}/v0/resources/changes?cursor=0&limit=1`, { signal: AbortSignal.timeout(15000) });
  assert(head.ok); cursor = Number(head.headers.get('x-events-length')); assert(Number.isSafeInteger(cursor));
  await head.body?.cancel();
  const poll = async () => {
    const page = await json(`${nexus}/v0/resources/changes?cursor=${cursor}&limit=1000`);
    assert(page.next_cursor >= cursor, 'Nexus cursor went backwards');
    cursor = page.next_cursor;
    for (const line of page.lines) if (line.author === author && tags.some(tag => tag.path === line.object_path)) feed.set(line.object_path, line);
  };
  const search = params => json(`${pinakes}/v1/search?${new URLSearchParams({ tags: label, ...params })}`);
  try {
    await withTags(storage, tags, async () => {
      await waitFor('Nexus puts', async () => { await poll(); return tags.every(tag => feed.get(tag.path)?.op === 'put'); }, timeout, signal);
      const docs = new Map();
      for (const tag of tags) {
        const id = feed.get(tag.path).resource_id;
        const d = await waitFor(`Pinakes ${tag.kind}`, async () => {
          const r = await fetch(`${pinakes}/v1/resources/${id}?fields=document`, { signal: AbortSignal.timeout(15000) });
          if (r.status === 404) return false;
          assert(r.ok, `Pinakes ${tag.kind}: ${r.status}`);
          const d = await r.json();
          assert(!['missing','error','unreachable'].includes(d.status), `Pinakes ${tag.kind} finalized ${d.status}`);
          if (d.status !== 'ok') return false;
          assert(d.type_path?.includes(tag.type), `${tag.kind}: expected ${tag.type}, got ${d.type}`);
          return d;
        }, timeout, signal);
        docs.set(tag.kind, d);
        report.samples.push({ kind: tag.kind, uri: tag.uri, resource_id: id, type: d.type, status: d.status, source: d.document?.source });
      }
      report.assertions.push(`${tags.length} SDK tags reached Nexus and resolved with expected facets`);
      const results = await search({ facet: 'tags', fields: 'facets', limit: '500' });
      for (const d of docs.values()) assert(results.items.some(row => row.resource_id === d.resource_id), `Missing ${d.resource_id} in search`);
      assert(results.facets.tags.some(f => f.label === label && f.count === results.items.length));
      report.assertions.push('search and tag facets');
      const point = docs.get('geo').point;
      assert(point && Number.isFinite(point.lat) && Number.isFinite(point.lon));
      const bbox = `${point.lon - .01},${point.lat - .01},${point.lon + .01},${point.lat + .01}`;
      const bounded = await search({ bbox });
      assert(bounded.items.some(row => row.resource_id === docs.get('geo').resource_id));
      assert(bounded.items.every(row => row.point && row.point.lat >= point.lat-.01 && row.point.lat <= point.lat+.01 && row.point.lon >= point.lon-.01 && row.point.lon <= point.lon+.01));
      report.assertions.push('bbox contains geo pin');
      const event = docs.get('event'); assert(event.start);
      const from = new Date(Date.parse(event.start) - 86400000).toISOString();
      const to = new Date(Date.parse(event.start) + 86400000).toISOString();
      const events = await search({ type: 'Event', from, to, sort: 'start', fields: 'facets' });
      assert(events.items.some(row => row.resource_id === event.resource_id));
      assert(events.items.every(row => row.type_path.includes('Event') && Date.parse(row.start) >= Date.parse(from) && Date.parse(row.start) <= Date.parse(to)));
      report.assertions.push('Event ancestry and time range');
      const occurrences = await json(`${pinakes}/v1/resources/${docs.get('ics').resource_id}/occurrences`);
      assert(occurrences.items.length > 0, 'ICS should expand occurrences');
      const expectedOccurrences = samples.find(s => s.kind === 'ics').expectedOccurrences;
      if (expectedOccurrences !== undefined) assert.equal(occurrences.items.length, expectedOccurrences);
      report.assertions.push('ICS occurrences');
      positiveVenue ||= !!event.document?.venue?.resource_id;
      const venue = event.document?.venue?.resource_id ?? docs.get('osm').resource_id;
      const atVenue = await search({ type: 'Event', venue, fields: 'document' });
      assert(atVenue.items.every(row => row.venue_id === venue || row.document?.venue?.resource_id === venue));
      if (positiveVenue) assert(atVenue.items.some(row => row.resource_id === event.resource_id), 'Fixture venue must match');
      report.assertions.push(positiveVenue ? 'positive venue match' : `venue filter (${atVenue.items.length} matches)`);
      const ids = report.samples.map(s => s.resource_id);
      const changes = await json(`${pinakes}/v1/changes/lookup`, { resource_ids: ids, since: 0, limit: 500 });
      for (const id of ids) assert(changes.lines.some(line => line.resource_id === id && line.class === 'created'), `Missing created change: ${id}`);
      const changePage = await json(`${pinakes}/v1/changes?cursor=0&limit=500`);
      assert(changePage.lines.length > 0 && changePage.next_cursor > 0);
      report.assertions.push('change feed and lookup');
      const hydrated = await json(`${nexus}/v0/stream/resources/by_ids`, { resource_ids: ids, viewer_id: author });
      for (const id of ids) assert(hydrated.some(r => r.details.id === id && r.tags.some(t => t.label === label && t.taggers.includes(author))), `Missing Nexus hydration: ${id}`);
      report.assertions.push('Nexus by_ids hydration and own tags');
      await waitFor('group collapse', async () => {
        const collapsed = await search({ collapse: 'group', limit: '500' });
        if (!collapsed.items.length) return false;
        if (docs.has('isbn_twin')) {
          const twinIds = [docs.get('isbn').resource_id, docs.get('isbn_twin').resource_id];
          if (!collapsed.items.some(row => twinIds.every(id => row.group?.members?.some(m => m.resource_id === id)))) return false;
          assert(collapsed.items.length < results.items.length, 'ISBN twins must collapse into one row');
        }
        for (const row of collapsed.items) if (row.group) assert(row.group.members.length > 0);
        return true;
      }, timeout, signal);
      report.assertions.push('group collapse and membership');
    }, report, journal);
    assert(report.cleanup.every(c => c.deleted), 'Tag cleanup failed; retry using journal');
    await waitFor('Nexus deletions', async () => { await poll(); return tags.every(tag => feed.get(tag.path)?.op === 'del'); }, timeout, signal);
    await waitFor('Pinakes reference cleanup', async () => (await search({})).items.length === 0, timeout, signal);
    report.assertions.push('tags deleted; Nexus deletes and Pinakes reference cleanup confirmed');
    report.ok = true;
  } catch (error) {
    report.ok = false; report.error = error.message ?? String(error);
  }
  return report;
}
