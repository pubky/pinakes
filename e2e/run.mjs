import { createRequire } from 'node:module';
import { readFile, writeFile, mkdir, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
import { randomUUID } from 'node:crypto';
import { freshen, json, runFlow } from './harness.mjs';

const mock = process.argv.includes('--mock');
let child, session, liveSamples;
const control = new AbortController();
process.on('SIGINT', () => control.abort(new Error('Interrupted')));
process.on('SIGTERM', () => control.abort(new Error('Interrupted')));
await mkdir('.runtime', { recursive: true });
const journalFile = `.runtime/e2e-${randomUUID()}.json`;
let report;
try {
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
    samples = JSON.parse((await readFile(process.env.PINAKES_E2E_SAMPLES ?? 'e2e/samples.live.json', 'utf8')).replaceAll('{author}', author));
    for (const sample of samples) if (sample.kind === 'ics' && process.env.PINAKES_E2E_ICS_URL) sample.uri = process.env.PINAKES_E2E_ICS_URL;
    await freshen(samples);
    liveSamples = `.runtime/e2e-samples-${randomUUID()}.json`;
    await writeFile(liveSamples, JSON.stringify(samples, null, 2));
  }
  if (mock || process.argv.includes('--isolated-live')) {
    child = spawn('./target/debug/examples/e2e_stack', mock ? [] : ['--live', liveSamples], { stdio: ['ignore', 'pipe', 'inherit'] });
    const lines = createInterface({ input: child.stdout });
    const startup = await Promise.race([once(lines, 'line').then(([line]) => JSON.parse(line)), once(child, 'exit').then(([code]) => { throw new Error(`Stack exited before ready: ${code}`); })]);
    pinakes = startup.pinakes; nexus = startup.nexus;
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
  report.mode = mock ? 'mock' : 'live'; report.journal = journalFile;
} catch (error) {
  report = { ok: false, mode: mock ? 'mock' : 'live', error: error.message ?? String(error), journal: journalFile };
} finally {
  if (session) { try { await session.signout(); } catch { report.session_cleanup = 'signout failed'; } }
  if (child && child.exitCode === null) { const stopped = once(child, 'exit'); child.kill('SIGTERM'); await stopped; }
  if (liveSamples) await rm(liveSamples, { force: true });
}
console.log(JSON.stringify(report, null, 2));
if (process.env.PINAKES_E2E_REPORT) await writeFile(process.env.PINAKES_E2E_REPORT, JSON.stringify(report, null, 2));
process.exitCode = report.ok ? 0 : 1;
