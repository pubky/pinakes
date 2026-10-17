// Read-only demo and resolver regression check: POST /v1/resolve for a curated list of public
// URIs, poll each resource until it is no longer pending, print one table row per entry and
// exit non-zero on any mismatch. Never writes to Pubky or Nexus.
import { readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const USAGE = 'usage: scripts/demo-lookup [--base http://127.0.0.1:8090] [--file e2e/samples.lookup.json] [--timeout 90]';

export function parseArgs(argv) {
  const options = { base: 'http://127.0.0.1:8090', file: 'e2e/samples.lookup.json', timeoutSecs: 90 };
  for (let i = 0; i < argv.length; i++) {
    const [flag, value] = [argv[i], argv[i + 1]];
    if (flag === '--help' || flag === '-h') return { help: true };
    if (!['--base', '--file', '--timeout'].includes(flag) || value === undefined) throw new Error(USAGE);
    if (flag === '--base') options.base = value.replace(/\/+$/, '');
    if (flag === '--file') options.file = value;
    if (flag === '--timeout') options.timeoutSecs = Number(value);
    i++;
  }
  if (!/^https?:\/\/[^/]+$/.test(options.base)) throw new Error(`--base must be an http(s) origin: ${options.base}`);
  if (!(options.timeoutSecs > 0 && options.timeoutSecs <= 600)) throw new Error('--timeout must be 1-600 seconds');
  return options;
}

export function validateEntries(entries) {
  if (!Array.isArray(entries) || entries.length === 0) throw new Error('the lookup file must be a non-empty array');
  for (const e of entries) {
    if (typeof e?.kind !== 'string' || typeof e?.uri !== 'string') throw new Error(`each entry needs kind and uri: ${JSON.stringify(e)}`);
    for (const key of ['type', 'status']) {
      const v = e[key];
      if (v !== undefined && typeof v !== 'string' && !(Array.isArray(v) && v.length && v.every(x => typeof x === 'string'))) {
        throw new Error(`${e.kind}: ${key} must be a string or a non-empty list of strings`);
      }
    }
  }
  return entries;
}

const accepts = (expected, ...actual) => expected === undefined || [expected].flat().some(e => actual.includes(e));

// A failed request, a resource still pending at the deadline or an expectation mismatch fails.
export function verdict(entry, row) {
  if (row.error) return row.error;
  if (row.status === 'pending') return 'still pending';
  const problems = [];
  // A type matches itself or any ancestor in type_path (Place accepts Restaurant).
  if (!accepts(entry.type, row.type, ...(row.typePath ?? []))) problems.push(`type ${row.type ?? '-'} ≠ ${[entry.type].flat().join('|')}`);
  if (!accepts(entry.status, row.status)) problems.push(`status ${row.status} ≠ ${[entry.status].flat().join('|')}`);
  return problems.join('; ') || null;
}

export function toRow(entry, view) {
  const group = view?.group;
  return {
    kind: entry.kind,
    uri: entry.uri,
    type: view?.type ?? null,
    typePath: view?.type_path ?? [],
    source: view?.document?.source ?? null,
    status: view?.status ?? null,
    group: group?.id ? `${group.id.slice(0, 8)}×${group.member_count ?? 0}` : null,
  };
}

const COLUMNS = ['kind', 'uri', 'type', 'source', 'status', 'group', 'check'];
const clip = (s, n) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);

export function formatTable(rows) {
  const cells = rows.map(r => COLUMNS.map(c => clip(String((c === 'check' ? (r.problem ? `FAIL: ${r.problem}` : 'ok') : r[c]) ?? '-'), c === 'uri' ? 64 : 48)));
  const widths = COLUMNS.map((c, i) => Math.max(c.length, ...cells.map(row => row[i].length)));
  const line = row => row.map((cell, i) => cell.padEnd(widths[i])).join('  ').trimEnd();
  return [line(COLUMNS), line(widths.map(w => '-'.repeat(w))), ...cells.map(line)].join('\n');
}

// Pace /v1/resolve under the default [preview] budget (30 per minute per IP and per URI) and
// polling under the public API budget (120 per minute per IP).
export async function lookupAll(entries, { base, timeoutSecs, fetch = globalThis.fetch, sleep = ms => new Promise(r => setTimeout(r, ms)), now = Date.now, resolveGapMs = 2100, pollMs = 1000, log = () => {} }) {
  const rows = [];
  let lastResolve = -Infinity;
  for (const entry of entries) {
    const wait = lastResolve + resolveGapMs - now();
    if (wait > 0) await sleep(wait);
    lastResolve = now();
    let row;
    try {
      const response = await fetch(`${base}/v1/resolve`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ uri: entry.uri }) });
      const body = await response.json().catch(() => ({}));
      if (response.status !== 200 && response.status !== 202) throw new Error(`resolve HTTP ${response.status}${body.error ? `: ${body.error}` : ''}`);
      let view = body;
      const deadline = now() + timeoutSecs * 1000;
      while (view.status === 'pending' && now() < deadline) {
        await sleep(pollMs);
        const poll = await fetch(`${base}/v1/resources/${view.resource_id}?fields=document`);
        if (poll.status === 429) continue;
        if (poll.status !== 200) throw new Error(`resource HTTP ${poll.status}`);
        view = await poll.json();
      }
      row = toRow(entry, view);
    } catch (error) {
      row = { ...toRow(entry, null), error: error.message };
    }
    row.problem = verdict(entry, row);
    log(`${row.problem ? 'FAIL' : 'ok  '} ${entry.kind}`);
    rows.push(row);
  }
  return rows;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) return console.log(USAGE);
  const entries = validateEntries(JSON.parse(await readFile(options.file, 'utf8')));
  const rows = await lookupAll(entries, { ...options, log: line => process.stderr.write(`${line}\n`) });
  console.log(formatTable(rows));
  const failed = rows.filter(r => r.problem).length;
  console.log(`\n${rows.length - failed}/${rows.length} ok against ${options.base}`);
  process.exitCode = failed ? 1 : 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  main().catch(error => { console.error(error.message); process.exitCode = 2; });
}
