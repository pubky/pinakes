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
/** Nexus's batch view, asked by `uris`, has one row per URI it knows; each tagged URI is matched
 * by the row's `details.uri`. It lists at most five tags per resource; a full list without the
 * run's label is confirmed through that resource's own tag route, by Nexus's ID for it.
 * Returns how many needed it. */
export async function ownTags(hydrated, uris, label, author, tagsOf) {
  const own = tags => tags.some(t => t.label === label && t.taggers.includes(author));
  let confirmed = 0;
  for (const uri of uris) {
    const row = hydrated.find(r => r.details.uri === uri);
    assert(row, `Missing Nexus hydration: ${uri}`);
    if (own(row.tags)) continue;
    assert(row.tags.length >= 5, `Missing Nexus hydration: ${uri}`);
    assert(own((await tagsOf(row.details.id)).tags), `Missing own tag on Nexus: ${uri}`);
    confirmed++;
  }
  return confirmed;
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
        // Pinakes is read by the tagged URI: its IDs are its own, never Nexus's.
        const d = await waitFor(`Pinakes ${tag.kind}`, async () => {
          const r = await fetch(`${pinakes}/v1/resources/by-uri?${new URLSearchParams({ uri: tag.uri })}`, { signal: AbortSignal.timeout(15000) });
          if (r.status === 404) return false;
          assert(r.ok, `Pinakes ${tag.kind}: ${r.status}`);
          const d = await r.json();
          assert(!['missing','error','unreachable'].includes(d.status), `Pinakes ${tag.kind} finalized ${d.status}`);
          return d.status === 'ok' && d;
        }, timeout, signal);
        docs.set(tag.kind, d);
        report.samples.push({ kind: tag.kind, uri: tag.uri, resource_id: d.id, type: d.type, status: d.status, site: d.site });
        // A music link is typed and named by what it is, whatever its page says.
        if (tag.site) assert.equal(d.site, tag.site, `Pinakes ${tag.kind} site`);
        if (tag.site) assert.equal(d.type, tag.type, `Pinakes ${tag.kind} type`);
        if (tag.name) assert.equal(d.name, tag.name, `Pinakes ${tag.kind} name`);
      }
      // The type filter matches ancestors, so a MusicEvent sample is found under type=Event.
      for (const type of new Set(tags.map(tag => tag.type))) {
        const wanted = tags.filter(tag => tag.type === type).map(tag => docs.get(tag.kind).id);
        await waitFor(`Pinakes type ${type}`, async () => {
          const found = await search({ type, limit: '500' });
          return wanted.every(id => found.items.some(row => row.id === id));
        }, timeout, signal);
      }
      report.assertions.push(`${tags.length} SDK tags reached Nexus and resolved with expected types`);
      // A sample may already be resolved from an older public reference while
      // Pinakes is still ingesting this run's new tag. Wait for the scoped rows too.
      const results = await waitFor('Pinakes tagged search', async () => {
        const result = await search({ facet: 'tags', limit: '500' });
        return [...docs.values()].every(d => result.items.some(row => row.id === d.id)) && result;
      }, timeout, signal);
      assert(results.facets.tags.some(f => f.label === label && f.count === results.items.length));
      report.assertions.push('search and tag facets');
      const point = docs.get('geo').point;
      assert(point && Number.isFinite(point.lat) && Number.isFinite(point.lon));
      const bbox = `${point.lon - .01},${point.lat - .01},${point.lon + .01},${point.lat + .01}`;
      const bounded = await search({ bbox });
      assert(bounded.items.some(row => row.id === docs.get('geo').id));
      assert(bounded.items.every(row => row.point && row.point.lat >= point.lat-.01 && row.point.lat <= point.lat+.01 && row.point.lon >= point.lon-.01 && row.point.lon <= point.lon+.01));
      report.assertions.push('bbox contains geo pin');
      const event = docs.get('event'); assert(event.start);
      const from = new Date(Date.parse(event.start) - 86400000).toISOString();
      const to = new Date(Date.parse(event.start) + 86400000).toISOString();
      const events = await search({ type: 'Event', from, to, sort: 'start' });
      assert(events.items.some(row => row.id === event.id));
      assert(events.items.every(row => Date.parse(row.start) >= Date.parse(from) && Date.parse(row.start) <= Date.parse(to)));
      report.assertions.push('Event ancestry and time range');
      const occurrences = await json(`${pinakes}/v1/resources/${docs.get('ics').id}/occurrences`);
      assert(occurrences.items.length > 0, 'ICS should expand occurrences');
      const expectedOccurrences = samples.find(s => s.kind === 'ics').expectedOccurrences;
      if (expectedOccurrences !== undefined) {
        // The mock fixture's series starts seven days after the run; a series crossing a DST
        // transition is pinned with a fixed clock by dst_transitions_give_each_series_its_exact_count
        // in tests/events.rs.
        const starts = occurrences.items.map(o => o.start).join(', ');
        assert.equal(occurrences.items.length, expectedOccurrences,
          `ICS occurrences: expected ${expectedOccurrences}, got ${occurrences.items.length} (${starts}). ` +
          'If the series start falls near a DST change, compare with dst_transitions_give_each_series_its_exact_count in crates/pinakes-core/tests/events.rs.');
      }
      const looked = await json(`${pinakes}/v1/occurrences/lookup`, { resource_ids: [docs.get('ics').id, '0'.repeat(32)] });
      assert.deepEqual(looked.items.map(o => [o.id, o.start, o.occurrence.uid]), occurrences.items.map(o => [o.id, o.start, o.occurrence.uid]),
        'the occurrence lookup of the feed and an unknown ID answers the feed\'s occurrences');
      report.assertions.push('ICS occurrences');
      positiveVenue ||= !!event.details.venue?.id;
      const venue = event.details.venue?.id ?? docs.get('osm').id;
      const atVenue = await search({ type: 'Event', venue });
      for (const row of atVenue.items.filter(row => !row.occurrence)) {
        assert.equal((await json(`${pinakes}/v1/resources/${row.id}`)).details.venue?.id, venue);
      }
      if (positiveVenue) assert(atVenue.items.some(row => row.id === event.id), 'Fixture venue must match');
      report.assertions.push(positiveVenue ? 'positive venue match' : `venue filter (${atVenue.items.length} matches)`);
      // An alias reads as its root, so these are the cards' (root) IDs.
      const ids = [...new Set(report.samples.map(s => s.resource_id))];
      const changes = await json(`${pinakes}/v1/changes/lookup`, { resource_ids: ids, since: 0, limit: 500 });
      for (const id of ids) assert(changes.lines.some(line => line.resource_id === id && line.class === 'created'), `Missing created change: ${id}`);
      const firstPage = await json(`${pinakes}/v1/changes/lookup`, { resource_ids: ids, since: 0, limit: 1 });
      assert(firstPage.lines.length === 1 && firstPage.has_more);
      const nextPage = await json(`${pinakes}/v1/changes/lookup`, { resource_ids: ids, since: 0, limit: 1, cursor: firstPage.next_cursor });
      assert(nextPage.lines[0].seq > firstPage.lines[0].seq);
      // The same lookup by the links the samples were tagged with: every line of the lookup by
      // IDs, and the aliases' own lines besides (an alias spelling is the alias). A link that
      // became an alias after its first fetch (an API naming the object's root) has that
      // fetch's lines too, before its `aliased` line.
      const uris = report.samples.map(s => s.uri);
      const byLinks = await json(`${pinakes}/v1/changes/lookup`, { uris, since: 0, limit: 500 });
      const linked = new Set(byLinks.lines.map(line => line.seq)), byId = new Set(changes.lines.map(line => line.seq));
      const aliases = new Set(byLinks.lines.filter(line => line.class === 'aliased').map(line => line.resource_id));
      assert(changes.lines.every(line => linked.has(line.seq)), 'a change lookup by links has every line of the lookup by IDs');
      assert(byLinks.lines.every(line => byId.has(line.seq) || aliases.has(line.resource_id)), 'a change lookup by links adds only aliases\' own lines');
      report.assertions.push('change lookup by IDs and by links, and its pages');
      // Pinakes and Nexus combine by URI, as a client does: each card's own `uri` and its
      // `details.aliases`, and for an occurrence `occurrence.feed_uri` (its own URL was never
      // tagged). Those links cover every tagged one, and Nexus hydrates each.
      const occurrence = occurrences.items[0];
      assert.equal(occurrence.occurrence.feed_uri, docs.get('ics').uri, 'an occurrence names its feed\'s URI');
      const asked = [...new Set([...docs.values()].flatMap(d => [d.uri, ...d.details.aliases]).concat(occurrence.occurrence.feed_uri))];
      for (const uri of uris) assert(asked.includes(uri), `A card's links must cover the tagged link ${uri}`);
      const hydrated = [];
      for (let i = 0; i < asked.length; i += 100) {
        hydrated.push(...await json(`${nexus}/v0/stream/resources/by_ids`, { uris: asked.slice(i, i + 100), viewer_id: author }));
      }
      const confirmed = await ownTags(hydrated, uris, label, author, id => json(`${nexus}/v0/resource/${id}/tags?limit_tags=100&viewer_id=${author}`));
      report.assertions.push(confirmed ? `Nexus hydration by URIs and own tags (${confirmed} through the resource tag route)` : 'Nexus hydration by URIs and own tags');
      await waitFor('group hints', async () => {
        // Samples naming `group_with` must be one resource (an alias: ISBN-10 and -13, OSM
        // spellings) or share one group (a publisher page with the DOI/ISBN resource whose
        // registry names it as landing page and which it cites). A group is a hint: each card
        // names the other members, and each member keeps its own card.
        const pairs = samples.filter(s => s.group_with).map(s => [s.kind, s.group_with]);
        for (const [kind, other] of pairs) {
          const ids = [docs.get(kind).id, docs.get(other).id];
          // An alias (another spelling) reads as its root: one resource, no group needed.
          if (ids[0] === ids[1]) continue;
          const card = await json(`${pinakes}/v1/resources/${ids[0]}`);
          // Every member is in `details.also_on` (the card shows one per music service).
          if (!card.details.also_on.some(entry => entry.id === ids[1])) return false;
          const detail = await json(`${pinakes}/v1/groups/${card.group.id}`);
          assert(!('best' in detail), 'a group has no best card');
          assert(ids.every(id => detail.members.some(member => member.id === id)), 'the group lists both members');
          assert.equal(detail.members.find(member => member.id === ids[0]).name, card.name, 'a member keeps its own card');
        }
        // The book and its edition page stay two cards; the Book card names the page.
        if (docs.has('isbn') && docs.has('isbn_page')) {
          const book = await json(`${pinakes}/v1/resources/${docs.get('isbn').id}`);
          const page = await json(`${pinakes}/v1/resources/${docs.get('isbn_page').id}`);
          if (!book.also_on.some(entry => entry.id === page.id)) return false;
          assert.equal(book.type, 'Book');
          // An Open Library edition page is a book too (typed by its link), with its own card.
          assert.equal(page.type, 'Book');
          assert.notEqual(page.id, book.id);
        }
        const collapsed = await search({ collapse: 'group', limit: '500' });
        if (!collapsed.items.length) return false;
        if (pairs.length) assert(collapsed.items.length < results.items.length, 'grouped samples must collapse');
        for (const row of collapsed.items) if (row.group) {
          const members = row.also_on.filter(entry => entry.id);
          assert(members.length > 0 && members.length <= Math.min(row.group.size - 1, 8) && row.also_on.length <= 8);
          assert(!row.also_on.some(entry => entry.id === row.id && entry.uri === row.uri), 'a hint names the other members');
        }
        return true;
      }, timeout, signal);
      report.assertions.push(`group hints, members and collapse (${samples.filter(s => s.group_with).map(s => `${s.kind}~${s.group_with}`).join(', ') || 'no pairs'})`);
      // Samples naming `maybe_same_with` are one event on two pages: each card names the other
      // as possibly the same event, a hint that is never a group.
      const twins = samples.filter(s => s.maybe_same_with).map(s => [s.kind, s.maybe_same_with]);
      await waitFor('same-event hints', async () => {
        for (const [kind, other] of twins) {
          const [a, b] = [docs.get(kind).id, docs.get(other).id];
          const card = await json(`${pinakes}/v1/resources/${a}`);
          if (!card.maybe_same?.some(entry => entry.id === b)) return false;
          if (!(await json(`${pinakes}/v1/resources/${b}`)).maybe_same?.some(entry => entry.id === a)) return false;
          assert(card.maybe_same.length <= 3 && card.details.maybe_same.length <= 10);
          for (const entry of card.maybe_same) {
            assert(entry.uri && entry.start && entry.score >= 0.5 && entry.score <= 1, 'a same-event entry is {id, uri, site, start, score}');
          }
          assert(!card.group?.members.some(member => member.id === b), 'a same-event hint is never a group');
        }
        return true;
      }, timeout, signal);
      report.assertions.push(`same-event hints (${twins.map(([a, b]) => `${a}~${b}`).join(', ') || 'no pairs'})`);
      // A sample naming `derived` services shows each as a derived link: the same thing on a
      // service nobody tagged, found by an exact hub lookup.
      for (const sample of samples.filter(s => s.derived)) {
        const card = await waitFor(`also on for ${sample.kind}`, async () => {
          const card = await json(`${pinakes}/v1/resources/${docs.get(sample.kind).id}`);
          return sample.derived.every(site => card.also_on.some(e => e.derived && e.site === site)) && card;
        }, timeout, signal);
        assert(card.also_on.every(e => !e.derived || (e.via && e.uri !== card.uri)), 'a derived link names its hub and is never the card');
        report.assertions.push(`also on: ${sample.kind} derived ${sample.derived.join(', ')} (${card.also_on.filter(e => e.derived).length} derived links)`);
      }
      // A sample naming `facts` shows each in its details (a TIDAL track's own ISRC, also when no
      // other link shares it).
      for (const sample of samples.filter(s => s.facts)) {
        await waitFor(`facts for ${sample.kind}`, async () => {
          const card = await json(`${pinakes}/v1/resources/${docs.get(sample.kind).id}`);
          const facts = card.details?.facts || {};
          return Object.entries(sample.facts).every(([name, value]) => String(facts[name] || '').split('; ').includes(value));
        }, timeout, signal);
        report.assertions.push(`facts: ${sample.kind} ${Object.entries(sample.facts).map(([k, v]) => `${k}=${v}`).join(', ')}`);
      }
      if (docs.has('isbn') && docs.has('isbn_page')) report.assertions.push('a book and its edition page keep their own cards');
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
