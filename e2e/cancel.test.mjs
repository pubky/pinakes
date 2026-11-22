// A cancelled live recipe run (`run.mjs --live-recipes`) drops its stack's database and still
// writes its report. Needs the built `e2e_stack` example and Postgres: `scripts/e2e --mock` runs it.
// Nothing leaves this host: egress allows no host, and the routes that skip it (Pubky, OSM) are off.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { readFile, rm } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { psql } from './harness.mjs';

const cluster = process.env.DATABASE_URL ?? 'postgresql:///postgres?host=/var/run/postgresql';
const offline = { PINAKES__EGRESS__ALLOW_HOSTS: '["e2e.invalid"]', PINAKES__PUBKY__ENABLED: 'false', PINAKES__OSM__ENABLED: 'false', PINAKES__HTTP__CONTACT: 'e2e@example.invalid' };
const query = (database, sql) => psql(cluster, database, sql);
const owned = () => query('postgres', "SELECT datname FROM pg_database WHERE datname LIKE 'pinakes\\_e2e\\_owned\\_%'").split('\n').filter(Boolean);
/** The run's database once its first recipe is stored: the run is past its setup. */
async function running(before) {
  for (const deadline = Date.now() + 120000; Date.now() < deadline; await new Promise(r => setTimeout(r, 200))) {
    for (const database of owned().filter(d => !before.has(d))) {
      try { if (query(database, 'SELECT EXISTS(SELECT 1 FROM resource)') === 't') return database; } catch { /* still migrating */ }
    }
  }
  throw new Error('the recipe run stored no recipe within 2 minutes');
}

for (const [signal, whom] of [['SIGTERM', 'the runner'], ['SIGINT', 'the runner and its stack (as a terminal sends it)']]) {
  test(`a recipe run cancelled by ${signal} to ${whom} drops its database and writes its report`, async t => {
    const before = new Set(owned());
    const report = `.runtime/e2e-cancel-${randomUUID()}.json`;
    const group = signal === 'SIGINT';
    const runner = spawn(process.execPath, ['e2e/run.mjs', '--live-recipes'], { env: { ...process.env, ...offline, PINAKES_E2E_REPORT: report }, stdio: ['ignore', 'ignore', 'pipe'], detached: group });
    let stderr = ''; runner.stderr.on('data', d => { stderr += d; });
    const exited = once(runner, 'exit');
    t.after(async () => { if (runner.exitCode === null && runner.signalCode === null) runner.kill('SIGKILL'); await rm(report, { force: true }); });
    const database = await running(before);
    if (group) process.kill(-runner.pid, signal); else runner.kill(signal);
    let timer;
    const [code] = await Promise.race([exited, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`no exit 30 s after ${signal}: ${stderr}`)), 30000); })]);
    clearTimeout(timer);
    assert.equal(code, 1, stderr);
    const { ok, mode, error } = JSON.parse(await readFile(report, 'utf8'));
    assert.deepEqual({ ok, mode, error }, { ok: false, mode: 'recipes', error: 'Interrupted' });
    assert.ok(!owned().includes(database), `${database} is left over`);
  });
}
