import { createRequire } from 'node:module';
import { readFile, readdir, writeFile, mkdir, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
import { randomUUID } from 'node:crypto';
import { E2E_CONTACT, dropRefusedPages, freshen, instanceAgent, json, liveRecipes, preflight, runFlow, userAgent, withoutStackSkipped } from './harness.mjs';

const mock = process.argv.includes('--mock');
const recipesOnly = process.argv.includes('--live-recipes');
// The stack `scripts/e2e` built, where cargo put it.
const stack = `${process.env.CARGO_TARGET_DIR || 'target'}/debug/examples/e2e_stack`;
let child, session, liveSamples, skipped = [];
const control = new AbortController();
process.on('SIGINT', () => control.abort(new Error('Interrupted')));
process.on('SIGTERM', () => control.abort(new Error('Interrupted')));
await mkdir('.runtime', { recursive: true });
const journalFile = `.runtime/e2e-${randomUUID()}.json`;
/** Stops the stack unless it is gone: one a signal killed has no exit code, and its `exit` event
 * has passed. */
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const stopped = once(child, 'exit'); child.kill('SIGTERM'); await stopped;
}
/** The public recipes, live (`liveRecipes`), each one left out or failed logged. */
async function recipeSweep() {
  const recipes = await liveRecipes(() => (child = spawn(stack, ['--live-recipes'], { stdio: ['ignore', 'pipe', 'inherit'] })), control.signal);
  for (const s of recipes.skipped) console.error(`recipe left out: ${s.name} (${s.uri}): ${s.reason}`);
  for (const f of recipes.failed) console.error(`recipe failed: ${f.name} (${f.uri}): ${f.reason}`);
  return recipes;
}
let report;
try {
  if (recipesOnly) {
    const recipes = await recipeSweep();
    report = { ok: !recipes.failed.length, mode: 'recipes', recipes };
  } else {
    let pinakes = process.env.PINAKES_E2E_PINAKES ?? 'http://127.0.0.1:8090';
    let nexus = process.env.PINAKES_E2E_NEXUS ?? 'http://127.0.0.1:8080';
    let author, storage, samples, positiveVenue = false, keypair;
    if (!mock) {
      // Node's package entrypoint supplies the WASM loader and cookie-aware fetch.
      const { Keypair } = createRequire(import.meta.url)('@synonymdev/pubky');
      const secret = await readFile(process.env.PINAKES_E2E_SECRET_FILE);
      if (secret.length !== 32) throw new Error('Secret file must hold 32 raw bytes');
      keypair = Keypair.fromSecret(secret); secret.fill(0);
      author = keypair.publicKey.z32();
      // A pubky:// sample names an object of the test identity: `{author}` is its public key.
      // The shared samples, then each source's own (`e2e/samples/<name>.json`, gone with the source).
      const files = process.env.PINAKES_E2E_SAMPLES ? [process.env.PINAKES_E2E_SAMPLES]
        : ['e2e/samples.live.json', ...(await readdir('e2e/samples')).filter(f => f.endsWith('.json')).sort().map(f => `e2e/samples/${f}`)];
      samples = [];
      for (const file of files) samples.push(...JSON.parse((await readFile(file, 'utf8')).replaceAll('{author}', author)));
      for (const sample of samples) if (sample.kind === 'ics' && process.env.PINAKES_E2E_ICS_URL) sample.uri = process.env.PINAKES_E2E_ICS_URL;
      // The preflight fetches go out as Pinakes' egress will: the stack this checkout builds sends
      // its version with the operator's contact, else the run's; a running instance says what it
      // sends in its `/v1/info`.
      const agent = process.argv.includes('--isolated-live')
        ? userAgent(/\[workspace\.package\][^[]*?\bversion\s*=\s*"([^"]+)"/.exec(await readFile('Cargo.toml', 'utf8'))[1], process.env.PINAKES__HTTP__CONTACT?.trim() || E2E_CONTACT)
        : instanceAgent(await json(`${pinakes}/v1/info`));
      const fetches = preflight(agent);
      await freshen(samples, fetches.text);
      ({ samples, skipped } = await dropRefusedPages(samples, fetches.status));
      for (const s of skipped) console.error(`left out: ${s.kind} (${s.uri}): ${s.reason}`);
      liveSamples = `.runtime/e2e-samples-${randomUUID()}.json`;
      await writeFile(liveSamples, JSON.stringify(samples, null, 2));
    }
    if (mock || process.argv.includes('--isolated-live')) {
      child = spawn(stack, mock ? [] : ['--live', liveSamples], { stdio: ['ignore', 'pipe', 'inherit'] });
      const lines = createInterface({ input: child.stdout });
      const startup = await Promise.race([once(lines, 'line').then(([line]) => JSON.parse(line)), once(child, 'exit').then(([code]) => { throw new Error(`Stack exited before ready: ${code}`); })]);
      pinakes = startup.pinakes; nexus = startup.nexus;
      // Samples that need a source this build does not have are left out by the stack, as named.
      for (const s of startup.skipped ?? []) console.error(`left out: ${s.kind} (${s.uri}): ${s.reason}`);
      // The mock's samples come from the stack itself (`/mock/config`), without those it left out.
      ({ samples, skipped } = withoutStackSkipped(samples ?? [], skipped, startup.skipped));
    }
    if (mock) {
      const config = await json(`${pinakes}/mock/config`);
      ({ author, samples } = config); positiveVenue = config.positive_venue;
      storage = { exists: async path => (await json(`${pinakes}/mock/tags?${new URLSearchParams({ path })}`)).exists, putJson: (path, tag) => json(`${pinakes}/mock/tags`, { path, tag }, 'PUT'), delete: path => json(`${pinakes}/mock/tags`, { path }, 'DELETE') };
    } else {
      const { Pubky, PublicKey } = createRequire(import.meta.url)('@synonymdev/pubky');
      const pubky = new Pubky(); const signer = pubky.signer(keypair);
      try { session = await signer.signin('pinakes.e2e'); }
      catch (error) {
        if (error.data?.statusCode !== 404 || !process.env.PINAKES_E2E_SIGNUP_TOKEN) throw new Error(`Pubky sign-in failed (${error.name})`);
        try {
          await signer.signup(PublicKey.from(process.env.PINAKES_E2E_HOMESERVER), process.env.PINAKES_E2E_SIGNUP_TOKEN);
          session = await signer.signin('pinakes.e2e');
        } catch { throw new Error('Pubky signup or subsequent sign-in failed'); }
      }
      storage = session.storage;
      if (!await storage.exists('/pub/pubky.app/profile.json')) {
        const { PubkySpecsBuilder } = await import('pubky-app-specs');
        const b = new PubkySpecsBuilder(author); const user = b.createUser('Pinakes E2E', null, null, []);
        await storage.putJson('/pub/pubky.app/profile.json', user.user.toJson()); user.free(); b.free();
      }
      // The object a pubky:// sample tags: created once, like the profile, and kept.
      for (const sample of samples.filter(s => s.kind === 'pubky')) {
        const path = new URL(sample.uri).pathname;
        if (!await storage.exists(path)) await storage.putJson(path, { name: 'Pinakes E2E object', description: 'A Pubky object of another app, fetched by Pinakes over HTTPS.' });
      }
    }
    const journal = (tags, cleanup) => writeFile(journalFile, JSON.stringify({ author, tags, cleanup }, null, 2), { mode: 0o600 });
    report = await runFlow({ storage, author, samples, pinakes, nexus, positiveVenue, journal, signal: control.signal, timeout: Number(process.env.PINAKES_E2E_TIMEOUT_MS ?? (mock ? 120000 : 600000)) });
    report.mode = mock ? 'mock' : 'live'; report.journal = journalFile; if (skipped.length) report.skipped = skipped;
    // An isolated live run also resolves the public recipes, after its own stack has stopped.
    if (process.argv.includes('--isolated-live')) {
      await stop(child);
      report.recipes = await recipeSweep();
      if (report.recipes.failed.length) report.ok = false;
    }
  }
} catch (error) {
  report = { ...report, ok: false, mode: recipesOnly ? 'recipes' : mock ? 'mock' : 'live', error: error.message ?? String(error), journal: journalFile };
} finally {
  if (session) { try { await session.signout(); } catch { report.session_cleanup = 'signout failed'; } }
  await stop(child);
  if (liveSamples) await rm(liveSamples, { force: true });
}
console.log(JSON.stringify(report, null, 2));
if (process.env.PINAKES_E2E_REPORT) await writeFile(process.env.PINAKES_E2E_REPORT, JSON.stringify(report, null, 2));
process.exitCode = report.ok ? 0 : 1;
