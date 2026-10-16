import { createRequire } from 'node:module';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
import { randomUUID } from 'node:crypto';
import { json, runFlow } from './harness.mjs';

const mock = process.argv.includes('--mock');
let child, session;
const control = new AbortController();
process.on('SIGINT', () => control.abort(new Error('Interrupted')));
process.on('SIGTERM', () => control.abort(new Error('Interrupted')));
await mkdir('.runtime', { recursive: true });
const journalFile = `.runtime/e2e-${randomUUID()}.json`;
let report;
try {
  let pinakes = process.env.PINAKES_E2E_PINAKES ?? 'http://127.0.0.1:8090';
  let nexus = process.env.PINAKES_E2E_NEXUS ?? 'http://127.0.0.1:8080';
  let author, storage, samples, positiveVenue = false;
  if (mock || process.argv.includes('--isolated-live')) {
    child = spawn('./target/debug/examples/e2e_stack', mock ? [] : ['--live', process.env.PINAKES_E2E_SAMPLES ?? 'e2e/samples.live.json'], { stdio: ['ignore', 'pipe', 'inherit'] });
    const lines = createInterface({ input: child.stdout });
    const startup = await Promise.race([once(lines, 'line').then(([line]) => JSON.parse(line)), once(child, 'exit').then(([code]) => { throw new Error(`Stack exited before ready: ${code}`); })]);
    pinakes = startup.pinakes; nexus = startup.nexus;
  }
  if (mock) {
    const config = await json(`${pinakes}/mock/config`);
    ({ author, samples } = config); positiveVenue = config.positive_venue;
    storage = { exists: async path => (await json(`${pinakes}/mock/tags?${new URLSearchParams({ path })}`)).exists, putJson: (path, tag) => json(`${pinakes}/mock/tags`, { path, tag }, 'PUT'), delete: path => json(`${pinakes}/mock/tags`, { path }, 'DELETE') };
  } else {
    // Node's package entrypoint supplies the WASM loader and cookie-aware fetch.
    const { Pubky, Keypair, PublicKey } = createRequire(import.meta.url)('@synonymdev/pubky');
    const secret = await readFile(process.env.PINAKES_E2E_SECRET_FILE);
    if (secret.length !== 32) throw new Error('Secret file must hold 32 raw bytes');
    const keypair = Keypair.fromSecret(secret); secret.fill(0);
    const pubky = new Pubky(); const signer = pubky.signer(keypair);
    author = signer.publicKey.z32();
    let created = false;
    try { session = await signer.signin('pinakes.e2e'); }
    catch (error) {
      if (error.data?.statusCode !== 404 || !process.env.PINAKES_E2E_SIGNUP_TOKEN) throw new Error(`Pubky sign-in failed (${error.name})`);
      await signer.signup(PublicKey.from(process.env.PINAKES_E2E_HOMESERVER), process.env.PINAKES_E2E_SIGNUP_TOKEN);
      session = await signer.signin('pinakes.e2e'); created = true;
    }
    storage = session.storage;
    if (!await storage.exists('/pub/pubky.app/profile.json')) {
      if (!created) throw new Error('Existing identity needs a profile before e2e');
      const { PubkySpecsBuilder } = await import('pubky-app-specs');
      const b = new PubkySpecsBuilder(author); const user = b.createUser('Pinakes E2E', null, null, []);
      await storage.putJson('/pub/pubky.app/profile.json', user.user.toJson()); user.free(); b.free();
    }
    samples = JSON.parse(await readFile(process.env.PINAKES_E2E_SAMPLES ?? 'e2e/samples.live.json', 'utf8'));
  }
  const journal = (tags, cleanup) => writeFile(journalFile, JSON.stringify({ author, tags, cleanup }, null, 2), { mode: 0o600 });
  report = await runFlow({ storage, author, samples, pinakes, nexus, positiveVenue, journal, signal: control.signal, timeout: Number(process.env.PINAKES_E2E_TIMEOUT_MS ?? 120000) });
  report.mode = mock ? 'mock' : 'live'; report.journal = journalFile;
} catch (error) {
  report = { ok: false, mode: mock ? 'mock' : 'live', error: error.message ?? String(error), journal: journalFile };
} finally {
  if (session) { try { await session.signout(); } catch { report.session_cleanup = 'signout failed'; } }
  if (child && child.exitCode === null) { const stopped = once(child, 'exit'); child.kill('SIGTERM'); await stopped; }
}
console.log(JSON.stringify(report, null, 2));
if (process.env.PINAKES_E2E_REPORT) await writeFile(process.env.PINAKES_E2E_REPORT, JSON.stringify(report, null, 2));
process.exitCode = report.ok ? 0 : 1;
