import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { setTimeout as sleep } from 'node:timers/promises';
import { PubkySpecsBuilder } from 'pubky-app-specs';

export async function json(url, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(url, { method, headers: body === undefined ? {} : { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15000) });
  assert(response.ok, `${method} ${new URL(url).pathname}: HTTP ${response.status}`);
  return response.status === 204 ? null : response.json();
}
/** The JSON-RPC answer in an MCP response body: the body itself (`application/json`), or the
 * last `data:` event of an event stream. */
export function mcpAnswer(text, contentType) {
  if (!contentType?.startsWith('text/event-stream')) return JSON.parse(text);
  const events = text.split('\n').filter(line => line.startsWith('data:')).map(line => line.slice(5).trim()).filter(Boolean);
  assert(events.length > 0, 'an MCP event stream with no data');
  return JSON.parse(events.at(-1));
}
/** One message to an MCP endpoint, as a client sends it; a tool call's answer is its JSON text. */
export async function mcp(url, message) {
  const response = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json', accept: 'application/json, text/event-stream', 'mcp-protocol-version': '2025-06-18' }, body: JSON.stringify(message), signal: AbortSignal.timeout(15000) });
  assert(response.ok, `POST /mcp ${message.method}: HTTP ${response.status}`);
  const answer = mcpAnswer(await response.text(), response.headers.get('content-type'));
  assert(!answer.error, `${message.method}: ${JSON.stringify(answer.error)}`);
  return answer.result;
}
/** A tool's answer as JSON; a tool error fails the run with its text. */
export async function tool(url, name, args) {
  const result = await mcp(url, { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name, arguments: args } });
  const text = result.content?.[0]?.text;
  assert(!result.isError, `tool ${name}: ${text}`);
  return JSON.parse(text);
}
/** The samples a run tags once the stack has started: a sample the stack left out (one that
 * `needs` an also-on source this build does not have) is not tagged, and is named under
 * `skipped` with the stack's reason, after the samples the preflight left out. */
export function withoutStackSkipped(samples, skipped, stackSkipped = []) {
  const left = new Set(stackSkipped.map(s => s.kind));
  return { samples: samples.filter(s => !left.has(s.kind)), skipped: [...skipped, ...stackSkipped] };
}
/** Why a recipe is left out rather than failed, by its failure class (`resolver::error_class`):
 * this host could not check it, which says nothing of the recipe. */
const LEFT_OUT = {
  dns: host => `${host} could not be reached from this host (dns)`,
  connect: host => `${host} could not be reached from this host (connect)`,
  timeout: host => `${host} could not be reached from this host (timeout)`,
  wall: host => `${host} answers this host with a bot wall`,
  rate_limited: host => `${host} asked this host to wait (rate limited)`,
};
/** The public `/v1/resolve` recipes as `e2e_stack --live-recipes` resolved them: each must be a
 * card of the type it promises, or of a type under it (a City is a Place), or is left out with
 * the reason when this host could not check it. */
export function judgeRecipes(results) {
  const failed = [], skipped = [];
  for (const r of results) {
    if (r.status === 'ok' && (r.type === r.want || r.type_path?.includes(r.want))) continue;
    const host = /^[a-z]+:\/\/([^/?#]+)/i.exec(r.uri)?.[1] ?? r.uri.split(':')[0];
    if (LEFT_OUT[r.error]) skipped.push({ name: r.name, uri: r.uri, reason: LEFT_OUT[r.error](host) });
    else failed.push({ name: r.name, uri: r.uri, reason: r.status === 'unrouted' ? `no resolver takes it, the recipe promises ${r.want}` : `card ${r.status}${r.type ? ` ${r.type}` : ''}${r.error ? ` (${r.error})` : ''}, the recipe promises ${r.want}` });
  }
  return { checked: results.length, failed, skipped };
}
/** The newest item's link in an RSS feed: the first `<item>`'s `<link>`. */
export function newestLink(rss) {
  const item = /<item\b[\s\S]*?<\/item>/.exec(rss)?.[0];
  return item && /<link>\s*(?:<!\[CDATA\[)?\s*(https?:\/\/[^\s<\]]+)/.exec(item)?.[1];
}
/** The contact a run gives the services it reads when the operator set none
 * (`PINAKES__HTTP__CONTACT`); the e2e driver falls back to the same one (`E2E_CONTACT`). */
export const E2E_CONTACT = 'info@synonym.to';
/** The User-Agent Pinakes' egress sends (`resolver::egress::user_agent`): its version and the
 * contact, so a site sees the run's preflight fetches as it will see Pinakes'. */
export const userAgent = (version, contact) => contact ? `pinakes/${version} (+https://pubky.app; ${contact})` : `pinakes/${version} (+https://pubky.app)`;
/** The User-Agent a running instance sends, as its `/v1/info` reports it: its contact may be set
 * in its file alone, so the preflight takes the agent from the instance, never from the run. */
export function instanceAgent(info) {
  assert(typeof info?.user_agent === 'string' && info.user_agent.startsWith('pinakes/'), 'the instance\'s /v1/info does not report its User-Agent (user_agent): run one built from this checkout');
  return info.user_agent;
}
/** The preflight's fetches of a page's text and of its status, with that User-Agent. */
export function preflight(agent) {
  const get = url => fetch(url, { headers: { 'user-agent': agent }, signal: AbortSignal.timeout(15000) });
  return {
    text: async url => {
      const response = await get(url);
      assert(response.ok, `${url}: HTTP ${response.status}`);
      return response.text();
    },
    status: async url => {
      const response = await get(url);
      await response.body?.cancel();
      return response.status;
    },
  };
}
/** A sample naming `latest_in` (a walled site's RSS feed) takes that feed's newest item as its
 * link: feeds list recent items only, so a fixed article drops out of them within days. */
export async function freshen(samples, fetchText) {
  for (const sample of samples.filter(s => s.latest_in)) {
    const link = newestLink(await fetchText(sample.latest_in));
    assert(link, `No item in ${sample.latest_in} for the ${sample.kind} sample`);
    sample.uri = link;
  }
  return samples;
}
/** Samples marked `needs_page` are cards read from the page itself. A site that refuses this
 * host's page fetches (401 or 403) cannot make that card from here, whatever Pinakes does, so the
 * sample is left out of the run and reported with the reason; every other sample stays strict.
 * A sample another one joins (`group_with`, `maybe_same_with`) is never left out. */
export async function dropRefusedPages(samples, fetchStatus) {
  const skipped = [];
  for (const sample of samples.filter(s => s.needs_page)) {
    const status = await fetchStatus(sample.uri);
    if (status !== 401 && status !== 403) continue;
    assert(!samples.some(s => s.group_with === sample.kind || s.maybe_same_with === sample.kind), `The ${sample.kind} sample is joined by another and cannot be left out`);
    skipped.push({ kind: sample.kind, uri: sample.uri, reason: `${new URL(sample.uri).host} refuses this host's page fetches (HTTP ${status})` });
  }
  return { samples: samples.filter(s => !skipped.some(k => k.kind === s.kind)), skipped };
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
/** The MCP social tool's `item` for `uri` answers as Nexus's batch view (`hydrated`, asked by
 * URI just before): the run's label when that view lists it, and the view's own labels when its
 * five are full without it (`ownTags` confirmed the tag through the resource tag route). */
export function socialAgrees(item, hydrated, uri, label) {
  const row = hydrated.find(r => r.details.uri === uri);
  assert(row, `Missing Nexus hydration: ${uri}`);
  assert(item, `Missing social answer: ${uri}`);
  const labels = tags => tags.map(t => t.label).sort();
  if (row.tags.some(t => t.label === label)) {
    assert(item.tags.some(t => t.label === label), `the social tool returns the run's tag from Nexus: ${uri}`);
  } else {
    assert.deepEqual(labels(item.tags), labels(row.tags), `the social tool answers as Nexus's batch view: ${uri}`);
  }
}
/** The fields the report keeps of a sample's card. */
const reported = card => ({ resource_id: card.id, type: card.type, status: card.status, site: card.site });
/** A sample's card meets the sample's expectations: a music link is typed and named by what it is,
 * whatever its page says, and a media page is named after its object (by the page hook at once,
 * or, when its record was slow, once the also-on title reads the card again). Every card kept is
 * of the sample's type, ancestors included (`isA`: an Event sample's card may be a MusicEvent);
 * the type search confirms only the first card read, never one that replaces it. Returns the
 * card to keep. */
export async function expectCard(tag, card, readByUri, timeout, signal, isA = async (c, type) => c.type === type) {
  // The card judged is the one given or, when the sample's link became a spelling of another
  // root while its type was awaited (`isA` answers with that root), the root: judged in its turn.
  const judge = async c => {
    assert.equal(c.status, 'ok', `Pinakes ${tag.kind} status`);
    if (tag.site) assert.equal(c.site, tag.site, `Pinakes ${tag.kind} site`);
    const verdict = await isA(c, tag.type, tag.uri);
    if (verdict !== true && verdict?.id && verdict.id !== c.id) return judge(verdict);
    assert(verdict === true, `Pinakes ${tag.kind} type: ${c.type} is no ${tag.type}`);
    return c;
  };
  card = await judge(card);
  if (!tag.name) return card;
  const named = card.name === tag.name ? card : await waitFor(`Pinakes ${tag.kind} name`, async () => {
    const v = await readByUri(tag.uri);
    return v && v.name === tag.name && v;
  }, timeout, signal).catch(error => { if (signal?.aborted) throw error; return card; });
  // Every expectation holds on the card that is kept, not only on the first one read.
  const kept = await judge(named);
  assert.equal(kept.name, tag.name, `Pinakes ${tag.kind} name`);
  return kept;
}
/** A card read by its URI can be read once more under another ID: a fetch can show the link to be
 * a spelling of another (a Mastodon @-link of its /users/…/statuses/ form), and its card is then
 * the root's, the only one search lists. Each sample search does not list is read again by its
 * URI; a card under another ID must meet the sample's expectations again (`check`) before it
 * replaces the first, in `docs` and in every reported field. */
export async function rereadMoved(tags, docs, report, readByUri, check) {
  for (const tag of tags) {
    const card = await readByUri(tag.uri);
    if (!card || card.status !== 'ok' || card.id === docs.get(tag.kind).id) continue;
    const kept = await check(tag, card);
    docs.set(tag.kind, kept);
    Object.assign(report.samples.find(s => s.kind === tag.kind), reported(kept));
  }
}
/** Search until every tag's card is among the results, re-reading the cards that moved (an
 * alias now answered by its root) between tries; the last result. */
export async function findsEvery(label, query, tags, docs, reread, timeout, signal) {
  return waitFor(label, async () => {
    const result = await query();
    const missing = tags.filter(tag => !result.items.some(row => row.id === docs.get(tag.kind).id));
    await reread(missing);
    return missing.length === 0 && result;
  }, timeout, signal);
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
      const readByUri = async uri => {
        const r = await fetch(`${pinakes}/v1/resources/by-uri?${new URLSearchParams({ uri })}`, { signal: AbortSignal.timeout(15000) });
        return r.ok ? r.json() : null;
      };
      // A card is of a type when the type search (ancestors included) lists it under the run's tag.
      // While that is awaited the sample's link may become a spelling of another root (a fetch
      // showed it to be one): search lists the root, never this card, so the root, read by the
      // URI, is the answer, for `expectCard` to judge instead (`rereadMoved`'s case, earlier).
      const isA = async (card, type, uri) => card.type === type || waitFor(`Pinakes ${type} lists ${card.type} ${card.id}`, async () => {
        const found = await search({ type, limit: '500' });
        if (found.items.some(row => row.id === card.id)) return true;
        const root = await readByUri(uri);
        return root && root.status === 'ok' && root.id !== card.id && root;
      }, timeout, signal).catch(error => { if (signal?.aborted) throw error; return false; });
      const check = (tag, card) => expectCard(tag, card, readByUri, timeout, signal, isA);
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
        report.samples.push({ kind: tag.kind, uri: tag.uri, ...reported(d) });
        const kept = await check(tag, d);
        docs.set(tag.kind, kept);
        Object.assign(report.samples.at(-1), reported(kept));
      }
      const reread = missing => rereadMoved(missing, docs, report, readByUri, check);
      // The type filter matches ancestors, so a MusicEvent sample is found under type=Event.
      for (const type of new Set(tags.map(tag => tag.type))) {
        await waitFor(`Pinakes type ${type}`, async () => {
          const found = await search({ type, limit: '500' });
          const missing = tags.filter(tag => tag.type === type && !found.items.some(row => row.id === docs.get(tag.kind).id));
          await reread(missing);
          return missing.length === 0;
        }, timeout, signal);
      }
      report.assertions.push(`${tags.length} SDK tags reached Nexus and resolved with expected types`);
      // Several types in one search are any of them, and the type facet places each type.
      const kinds = [...new Set(tags.map(tag => tag.type))].slice(0, 10);
      const anyKind = await findsEvery(`Pinakes type=${kinds.join(',')}`, () => search({ type: kinds.join(','), facet: 'type', limit: '500' }),
        tags.filter(t => kinds.includes(t.type)), docs, reread, timeout, signal);
      for (const entry of anyKind.facets.type) {
        assert(entry.type_path.length > 0 && entry.type_path.at(-1) === entry.type, `facet type_path ends at ${entry.type}`);
      }
      report.assertions.push(`type list (${kinds.length} types) and the type facet's type_path`);
      // A sample may already be resolved from an older public reference while
      // Pinakes is still ingesting this run's new tag. Wait for the scoped rows too.
      const results = await waitFor('Pinakes tagged search', async () => {
        const result = await search({ facet: 'tags', limit: '500' });
        const missing = tags.filter(tag => !result.items.some(row => row.id === docs.get(tag.kind).id));
        await reread(missing);
        return missing.length === 0 && result;
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
      positiveVenue ||= !!event.venue?.id;
      const venue = event.venue?.id ?? docs.get('osm').id;
      const atVenue = await search({ type: 'Event', venue });
      for (const row of atVenue.items.filter(row => !row.occurrence)) {
        assert.equal((await json(`${pinakes}/v1/resources/${row.id}`)).venue?.id, venue);
      }
      if (positiveVenue) assert(atVenue.items.some(row => row.id === event.id), 'Fixture venue must match');
      report.assertions.push(positiveVenue ? 'positive venue match' : `venue filter (${atVenue.items.length} matches)`);
      // Job postings: the on-site one is placed at its workplace; `remote` tells the two apart.
      if (docs.has('job') && docs.has('job_remote')) {
        const [onSite, remote] = [docs.get('job'), docs.get('job_remote')];
        assert(onSite.point, 'an on-site posting is placed at its workplace');
        const remoteOnes = await search({ type: 'JobPosting', remote: 'true' });
        const onSiteOnes = await search({ type: 'JobPosting', remote: 'false' });
        assert(remoteOnes.items.some(row => row.id === remote.id) && !remoteOnes.items.some(row => row.id === onSite.id), 'remote=true lists the remote posting only');
        assert(onSiteOnes.items.some(row => row.id === onSite.id) && !onSiteOnes.items.some(row => row.id === remote.id), 'remote=false lists the on-site posting only');
        report.assertions.push('job postings: workplace, remote filter');
      }
      // A recipe is found by whole words of its ingredients, not by part of one.
      if (docs.has('recipe')) {
        const recipe = docs.get('recipe');
        const withBoth = await search({ ingredient: 'potatoes,butter' });
        const byPart = await search({ ingredient: 'potato' });
        assert(withBoth.items.some(row => row.id === recipe.id), 'ingredient=potatoes,butter lists the recipe');
        assert(!byPart.items.some(row => row.id === recipe.id), 'ingredient=potato matches no part of a word');
        report.assertions.push('recipes: ingredient search');
      }
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
      // `aliases`, and for an occurrence `occurrence.feed_uri` (its own URL was never
      // tagged). Those links cover every tagged one, and Nexus hydrates each.
      const occurrence = occurrences.items[0];
      assert.equal(occurrence.occurrence.feed_uri, docs.get('ics').uri, 'an occurrence names its feed\'s URI');
      const asked = [...new Set([...docs.values()].flatMap(d => [d.uri, ...d.aliases]).concat(occurrence.occurrence.feed_uri))];
      for (const uri of uris) assert(asked.includes(uri), `A card's links must cover the tagged link ${uri}`);
      const hydrated = [];
      for (let i = 0; i < asked.length; i += 100) {
        hydrated.push(...await json(`${nexus}/v0/stream/resources/by_ids`, { uris: asked.slice(i, i + 100), viewer_id: author }));
      }
      const confirmed = await ownTags(hydrated, uris, label, author, id => json(`${nexus}/v0/resource/${id}/tags?limit_tags=100&viewer_id=${author}`));
      report.assertions.push(confirmed ? `Nexus hydration by URIs and own tags (${confirmed} through the resource tag route)` : 'Nexus hydration by URIs and own tags');
      // Agents: the guide, and MCP's tools answering as the routes do, `social` through the
      // Nexus this instance reads.
      const guide = await fetch(`${pinakes}/llms.txt`, { signal: AbortSignal.timeout(15000) });
      assert(guide.ok && guide.headers.get('content-type').startsWith('text/plain'), 'GET /llms.txt is the plain-text guide');
      const guideText = await guide.text();
      assert(guideText.includes('/mcp') && !guideText.includes('{{BASE}}'), 'the guide names this host');
      const initialized = await mcp(`${pinakes}/mcp`, { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'pinakes-e2e', version: '1' } } });
      assert.equal(initialized.serverInfo.name, 'pinakes');
      const [firstKind, firstDoc] = [...docs.entries()][0];
      const firstUri = tags.find(t => t.kind === firstKind).uri;
      const read = await tool(`${pinakes}/mcp`, 'card', { uri: firstUri });
      assert.equal(read.card.id, firstDoc.id, 'the card tool reads the tagged link');
      const found = await tool(`${pinakes}/mcp`, 'search', { tags: [label], limit: 500 });
      for (const doc of docs.values()) assert(found.items.some(card => card.id === doc.id), `the search tool lists ${doc.uri}`);
      const social = await tool(`${pinakes}/mcp`, 'social', { uris: [firstUri], viewer: author });
      socialAgrees(social.items[0], hydrated, firstUri, label);
      report.assertions.push('agents: the guide, and the MCP card, search and social tools');
      await waitFor('group hints', async () => {
        // Samples naming `group_with` must be one resource (an alias: ISBN-10 and -13, OSM
        // spellings) or share one group (a publisher page with the DOI/ISBN resource whose
        // registry names it as landing page and which it cites). A group is a hint: each card
        // names the other members, and each member keeps its own card.
        // A pair whose other sample is not in this run (its source's samples gone with it) is left out.
        const pairs = samples.filter(s => s.group_with && docs.has(s.group_with)).map(s => [s.kind, s.group_with]);
        for (const [kind, other] of pairs) {
          const ids = [docs.get(kind).id, docs.get(other).id];
          // An alias (another spelling) reads as its root: one resource, no group needed.
          if (ids[0] === ids[1]) continue;
          const card = await json(`${pinakes}/v1/resources/${ids[0]}`);
          // Every member is in a read's `also_on` (a search card shows one per music service).
          if (!card.also_on.some(entry => entry.id === ids[1])) return false;
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
      const twins = samples.filter(s => s.maybe_same_with && docs.has(s.maybe_same_with)).map(s => [s.kind, s.maybe_same_with]);
      await waitFor('same-event hints', async () => {
        for (const [kind, other] of twins) {
          const [a, b] = [docs.get(kind).id, docs.get(other).id];
          const card = await json(`${pinakes}/v1/resources/${a}`);
          if (!card.maybe_same?.some(entry => entry.id === b)) return false;
          if (!(await json(`${pinakes}/v1/resources/${b}`)).maybe_same?.some(entry => entry.id === a)) return false;
          assert(card.maybe_same.length <= 10);
          for (const entry of card.maybe_same) {
            assert(entry.uri && entry.start && entry.score >= 0.5 && entry.score <= 1, 'a same-event entry is {id, uri, site, start, score}');
          }
          assert(!card.group?.members.some(member => member.id === b), 'a same-event hint is never a group');
        }
        return true;
      }, timeout, signal);
      report.assertions.push(`same-event hints (${twins.map(([a, b]) => `${a}~${b}`).join(', ') || 'no pairs'})`);
      // One author tagged every sample with this run's label, so what a sample's taggers also
      // referenced includes the other samples, never the sample itself or its group, when one
      // shared tagger is enough (`limits.related_min_shared`; the mock's). Else this run's tags
      // alone relate nothing, and only the exclusion is checked.
      const [seedKind, seedDoc] = [...docs.entries()][0];
      const floor = (await json(`${pinakes}/v1/info`)).limits.related_min_shared;
      const related = floor <= 1
        ? await waitFor('related', async () => {
          const page = await search({ related: seedDoc.id, limit: '500' });
          return page.items.length > 0 && page;
        }, timeout, signal)
        : await search({ related: seedDoc.id, limit: '500' });
      const own = new Set([seedDoc.id, ...(seedDoc.group?.members ?? []).map(member => member.id)]);
      assert(related.items.every(row => !own.has(row.id)), 'related never lists the resource or its group');
      if (floor <= 1) assert(related.items.some(row => [...docs.values()].some(d => d.id === row.id)), 'related lists another sample');
      report.assertions.push(`related (${related.items.length} rows for ${seedKind}, floor ${floor})`);
      // `boost` ranks what the viewer's set referenced higher and never filters: the same rows as
      // without it, whoever the author follows.
      const rowIds = page => page.items.map(row => row.id).sort();
      const plain = await search({ limit: '500' });
      const boosted = await search({ boost: 'following', viewer: author, limit: '500' });
      assert.deepEqual(rowIds(boosted), rowIds(plain), 'a boost never filters');
      report.assertions.push(`boost=following keeps every row (${boosted.items.length})`);
      // `wot_2` needs the Nexus reach-set route: an instance without it says so with 501 (and a
      // plain message), one with it boosts like the other sets.
      const wot = await fetch(`${pinakes}/v1/search?${new URLSearchParams({ tags: label, boost: 'wot_2', viewer: author, limit: '500' })}`, { signal: AbortSignal.timeout(15000) });
      if (wot.status === 501) {
        assert.match((await wot.json()).error, /not supported/, 'wot_2 without a reach set says why');
      } else {
        assert.equal(wot.status, 200, `boost=wot_2: HTTP ${wot.status}`);
        assert.deepEqual(rowIds(await wot.json()), rowIds(plain), 'a wot_2 boost never filters');
      }
      report.assertions.push(`boost=wot_2 answers ${wot.status}`);
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
      // A sample naming `facts` shows each in its read (a TIDAL track's own ISRC, also when no
      // other link shares it).
      for (const sample of samples.filter(s => s.facts)) {
        await waitFor(`facts for ${sample.kind}`, async () => {
          const card = await json(`${pinakes}/v1/resources/${docs.get(sample.kind).id}`);
          const facts = card.facts || {};
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
