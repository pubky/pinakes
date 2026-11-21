"""Regression contracts for the CI trust boundary and release packaging."""
from pathlib import Path
import json
import os
import tempfile
import gzip
import re
import subprocess
import tomllib
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
# The peeled tagged commit, unique per workflow run: another run for the same commit (two tags on
# one commit) cannot overwrite the bytes this run smoke-tests and promotes.
PLATFORM_TAG = '${{ steps.main.outputs.commit }}-${{ github.run_id }}-${{ matrix.arch }}'


def workflow():
    return yaml.load((ROOT / '.github/workflows/ci.yml').read_text(), Loader=yaml.BaseLoader)


def steps(job):
    return '\n'.join(step.get('run', '') for step in job['steps'])


def step_index(job, predicate, what):
    found = [i for i, step in enumerate(job['steps']) if predicate(step)]
    if not found:
        raise AssertionError(f'no step {what}')
    return found[0]


def git(cwd, *args):
    return subprocess.run(['git', *args], cwd=cwd, check=True, capture_output=True, text=True,
                          env={**os.environ, 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1',
                               'GIT_AUTHOR_NAME': 'ci', 'GIT_AUTHOR_EMAIL': 'ci@example.invalid',
                               'GIT_COMMITTER_NAME': 'ci', 'GIT_COMMITTER_EMAIL': 'ci@example.invalid'}).stdout.strip()


def commit(repo, name, text, message):
    (Path(repo) / name).write_text(text)
    git(repo, 'add', name)
    git(repo, 'commit', '--quiet', '--no-gpg-sign', '-m', message)
    return git(repo, 'rev-parse', 'HEAD')


def dockerignore_rules(text):
    """Parse .dockerignore the way moby/patternmatcher does (no character classes or escapes)."""
    rules = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        exclusion = line.startswith('!')
        pattern = line[1:].strip() if exclusion else line
        if '[' in pattern or '\\' in pattern:
            raise AssertionError(f'contract matcher does not model {line!r}')
        pattern = os.path.normpath(pattern).lstrip('/')
        regex, i = '^', 0
        while i < len(pattern):
            if pattern.startswith('**', i):
                i += 2
                if pattern.startswith('/', i):
                    i += 1
                regex += '.*' if i == len(pattern) else '(.*/)?'
                continue
            ch = pattern[i]
            regex += '[^/]*' if ch == '*' else '[^/]' if ch == '?' else re.escape(ch)
            i += 1
        rules.append((line, exclusion, re.compile(regex + '$')))
    return rules


def in_build_context(rules, path):
    """Docker's MatchesOrParentMatches: the last rule matching the path or a parent decides."""
    parts = path.split('/')
    candidates = ['/'.join(parts[:n]) for n in range(1, len(parts) + 1)]
    ignored = False
    for _, exclusion, regex in rules:
        if any(regex.match(c) for c in candidates):
            ignored = not exclusion
    return not ignored


def cargo_target_paths():
    """Every manifest, build script and target file Cargo reads to load the workspace."""
    workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())
    paths = {'Cargo.toml', 'Cargo.lock'}
    for member in workspace['workspace']['members']:
        crate = ROOT / member
        manifest = tomllib.loads((crate / 'Cargo.toml').read_text())
        paths.add(f'{member}/Cargo.toml')
        build = manifest['package'].get('build', 'build.rs' if (crate / 'build.rs').exists() else None)
        if build:
            paths.add(f'{member}/{build}')
        lib = manifest.get('lib', {}).get('path', 'src/lib.rs')
        if (crate / lib).exists():
            paths.add(f'{member}/{lib}')
        if (crate / 'src/main.rs').exists():
            paths.add(f'{member}/src/main.rs')
        for kind, folder in [('bin', 'src/bin'), ('example', 'examples'), ('test', 'tests'), ('bench', 'benches')]:
            for target in manifest.get(kind, []):
                path = target.get('path') or next(
                    (p for p in [f'{folder}/{target["name"]}.rs', f'{folder}/{target["name"]}/main.rs']
                     if (crate / p).exists()), f'{folder}/{target["name"]}.rs')
                paths.add(f'{member}/{path}')
            # Targets Cargo discovers on its own are read when present.
            paths.update(str(p.relative_to(ROOT)) for p in (crate / folder).glob('*.rs'))
            paths.update(str(p.relative_to(ROOT)) for p in (crate / folder).glob('*/main.rs'))
    return paths


class Quality(unittest.TestCase):
    def test_full_gate_and_offline_flow_run_on_pr_main_and_tags(self):
        w = workflow()
        self.assertIn('pull_request', w['on'])
        self.assertEqual(w['on']['push']['branches'], ['main'])
        self.assertEqual(w['on']['push']['tags'], ['v*.*.*'])
        quality = w['jobs']['quality']
        self.assertIn('scripts/ci/setup-postgres', steps(quality))
        self.assertIn('scripts/test', steps(quality))
        self.assertIn('scripts/e2e --mock', steps(quality))
        self.assertIn('scripts/ci/test_contracts.py', steps(quality))
        self.assertNotIn('continue-on-error', str(w))
        self.assertEqual(w['permissions'], {'contents': 'read'})
        self.assertNotIn('pull_request_target', w['on'])
        self.assertNotIn('secrets.', str(quality))

    def test_every_external_action_has_an_exact_commit(self):
        w = workflow()
        for job in w['jobs'].values():
            for step in job['steps']:
                if 'uses' in step:
                    self.assertRegex(step['uses'], r'^[\w./-]+@[0-9a-f]{40}$')
                    if step['uses'].startswith('actions/checkout@'):
                        self.assertEqual(step['with']['persist-credentials'], 'false')

    def test_real_extension_provisioning(self):
        s = (ROOT / 'scripts/ci/setup-postgres').read_text()
        for required in ['postgresql-16-postgis-3', 'pg_trgm', 'ON_ERROR_STOP=1']:
            self.assertIn(required, s)

    def test_extension_versions_are_numeric(self):
        s = (ROOT / 'scripts/ci/setup-postgres').read_text()
        # The documented requirement, compared as numbers: '3.10' < '3.4' as text.
        self.assertIn("coalesce((string_to_array(postgis_lib_version(), '.'))[1:2]::int[], '{}') < ARRAY[3, 4]", s)
        self.assertNotRegex(s, r"(<>|<|>|=) *'[0-9]+(\.[0-9]+)+'")

    def test_apt_falls_back_to_a_mirror_only_for_unresolvable_ubuntu_hosts(self):
        s = (ROOT / 'scripts/ci/setup-postgres').read_text()
        # Probed and rewritten before the first apt-get, in every apt source format the runner uses.
        self.assertLess(s.index('getent hosts'), s.index('sudo apt-get update'))
        for source in ['/etc/apt/apt-mirrors.txt', '/etc/apt/sources.list',
                       '/etc/apt/sources.list.d/*.sources']:
            self.assertIn(source, s)
        archive = re.search(r"^ubuntu_archive='(.+)'$", s, re.M).group(1)

        def rewrite(line):
            return subprocess.run(['sed', '-E', f's#{archive}#MIRROR/#g'], input=line, check=True,
                                  capture_output=True, text=True).stdout
        for line, expected in [
            ('http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n', 'MIRROR/\tpriority:1\n'),
            ('URIs: http://security.ubuntu.com/ubuntu\n', 'URIs: MIRROR/\n'),
            ('URIs: http://ports.ubuntu.com/ubuntu-ports/\n', 'URIs: MIRROR/\n'),
            ('deb https://apt.postgresql.org/pub/repos/apt noble-pgdg main\n', None),
            ('deb https://esm.ubuntu.com/apps/ubuntu noble-apps-security main\n', None),
            ('deb http://notubuntu.com/ubuntu noble main\n', None),
        ]:
            self.assertEqual(rewrite(line), expected or line)

    def test_whitespace_check_covers_the_committed_change(self):
        quality = workflow()['jobs']['quality']
        self.assertEqual(quality['steps'][0]['with']['fetch-depth'], '0')
        lint = quality['steps'][step_index(quality, lambda st: st.get('run') == 'scripts/ci/check', 'running scripts/ci/check')]
        self.assertEqual(lint['env']['PINAKES_CHECK_BASE'],
                         "${{ github.event_name == 'push' && github.event.before || '' }}")
        self.assertIn('scripts/ci/diff-check', (ROOT / 'scripts/ci/check').read_text())
        (ROOT / '.runtime').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=ROOT / '.runtime', prefix='ci-diff-') as d:
            origin, clone = Path(d) / 'origin', Path(d) / 'clone'
            origin.mkdir()
            git(origin, 'init', '--quiet', '--initial-branch=main')
            commit(origin, 'a.txt', 'clean\n', 'base')
            main = commit(origin, 'b.txt', 'clean\n', 'main')
            git(origin, 'checkout', '--quiet', '-b', 'feature')
            commit(origin, 'c.txt', 'trailing \n', 'whitespace')
            feature = commit(origin, 'd.txt', 'clean\n', 'later clean commit')
            git(d, 'clone', '--quiet', '--no-checkout', str(origin), str(clone))

            def check(sha, base):
                git(clone, 'checkout', '--quiet', '--detach', sha)
                env = {**os.environ, 'PINAKES_CHECK_BASE': base,
                       'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1'}
                return subprocess.run([str(ROOT / 'scripts/ci/diff-check')], cwd=clone, env=env,
                                      capture_output=True, text=True)
            # A clean worktree: plain `git diff --check` would pass. The PR range must not.
            self.assertNotEqual(check(feature, main).returncode, 0)
            self.assertNotEqual(check(feature, '').returncode, 0)
            self.assertEqual(check(main, '').returncode, 0)
            # A new tag reports an all-zero before: HEAD on main checks its own commit.
            self.assertEqual(check(main, '0' * 40).returncode, 0)
            git(origin, 'checkout', '--quiet', 'main')
            git(origin, 'merge', '--quiet', '--no-ff', '--no-gpg-sign', '-m', 'merge', 'feature')
            git(clone, 'fetch', '--quiet', 'origin')
            merged = git(clone, 'rev-parse', 'refs/remotes/origin/main')
            self.assertNotEqual(check(merged, main).returncode, 0)
            self.assertNotEqual(check(merged, '0' * 40).returncode, 0)
            # Recorded fixtures and licence texts stay byte-exact.
            git(origin, 'checkout', '--quiet', '-b', 'fixtures')
            (origin / 'crates/core/tests/fixtures/x').mkdir(parents=True)
            (origin / 'data').mkdir()
            commit(origin, 'crates/core/tests/fixtures/x/feed.xml', 'recorded \n\n', 'fixture')
            fixture = commit(origin, 'data/LICENSE.x.md', 'licence \n', 'licence')
            git(clone, 'fetch', '--quiet', 'origin')
            self.assertEqual(check(fixture, '').returncode, 0, check(fixture, '').stdout)
            # A PR merge commit: only its own change counts, not main's other commits.
            git(origin, 'checkout', '--quiet', 'main')
            commit(origin, 'e.txt', 'clean\n', 'main moves on')
            git(origin, 'checkout', '--quiet', '-b', 'pr')
            commit(origin, 'f.txt', 'clean\n', 'clean PR')
            git(origin, 'checkout', '--quiet', '--detach', 'main')
            git(origin, 'merge', '--quiet', '--no-ff', '--no-gpg-sign', '-m', 'pr merge', 'pr')
            pr_merge = git(origin, 'rev-parse', 'HEAD')
            git(clone, 'fetch', '--quiet', 'origin', f'+{pr_merge}:refs/remotes/pull/1/merge')
            self.assertEqual(check(pr_merge, '').returncode, 0)

    def test_ci_database_urls_parse_as_urls(self):
        # sqlx's PgConnectOptions and the tests' url::Url::parse follow WHATWG: a user before an
        # empty host is EmptyHost (url 2.5.8 parser.rs after_double_slash).
        urls = [workflow()['env']['DATABASE_URL']]
        urls += re.findall(r"smoke_db='([^']+)'", (ROOT / 'scripts/ci/container-smoke').read_text())
        self.assertEqual(len(urls), 2)
        for url in urls:
            authority = re.match(r'^postgresql://([^/?#]*)', url)
            self.assertIsNotNone(authority, url)
            userinfo, at, host = authority[1].rpartition('@')
            self.assertFalse(at and not host, f'{url}: user before an empty host')
            self.assertRegex(url, r'^postgresql:///[a-z_]+\?host=/(var/)?run/postgresql&port=55432&user=pinakes_ci$')

    def test_cluster_service_resolves_to_its_config_directory(self):
        script = (ROOT / 'scripts/ci/setup-postgres').read_text()
        clusters = re.findall(r'^sudo pg_createcluster (\S+) (\S+) ', script, re.MULTILINE)
        self.assertEqual(len(clusters), 1)
        version, name = clusters[0]
        # PostgreSQL's unit uses unescaped %I in AssertPathExists. Verify the
        # actual systemd mapping without creating or starting a database.
        instance = subprocess.run(
            ['systemd-escape', '--unescape', '--instance', f'postgresql@{version}-{name}.service'],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        self.assertEqual(f'/etc/postgresql/{instance}/postgresql.conf',
                         f'/etc/postgresql/{version}/{name}/postgresql.conf')


class NativeImages(unittest.TestCase):
    def test_both_platforms_build_and_smoke_on_native_runners(self):
        w = workflow()
        for name in ['image', 'publish']:
            job = w['jobs'][name]
            self.assertEqual(job['runs-on'], '${{ matrix.runner }}')
            self.assertEqual(job['strategy']['fail-fast'], 'false')
            self.assertEqual(job['strategy']['matrix']['include'], [
                {'arch': 'amd64', 'runner': 'ubuntu-24.04'},
                {'arch': 'arm64', 'runner': 'ubuntu-24.04-arm'},
            ])
        self.assertIn('manifest', w['jobs'])
        self.assertEqual(set(w['jobs']['manifest']['needs']), {'publish'})


class StaticChecks(unittest.TestCase):
    def test_hosted_checks_run_the_same_linters(self):
        self.assertIn('scripts/ci/check', steps(workflow()['jobs']['quality']))
        s = (ROOT / 'scripts/ci/check').read_text()
        for tool in ['actionlint', 'hadolint', 'shellcheck', 'sha256sum --check']:
            self.assertIn(tool, s)


class Container(unittest.TestCase):
    def test_release_binary_nonroot_and_platform_native(self):
        s = (ROOT / 'Dockerfile').read_text()
        self.assertIn('cargo build --locked --release -p pinakesd', s)
        self.assertNotRegex(s, r'(?im)^ARG TARGETARCH')
        self.assertNotIn('--platform=$BUILDPLATFORM', s)
        self.assertRegex(s, r'(?m)^USER 10001:10001$')
        self.assertEqual(re.findall(r'(?m)^EXPOSE (.+)$', s), ['8090'])
        self.assertIn('PINAKES__ADMIN_LISTEN=127.0.0.1:9090', s)
        self.assertIn('PINAKES__OSM__DUMP_DIR=/var/lib/pinakes/dumps', s)
        self.assertIn('ENTRYPOINT ["/usr/local/bin/pinakesd"]', s)
        self.assertIn('CMD ["/usr/local/bin/pinakesd", "healthcheck"]', s)
        self.assertNotIn('COPY . ', s)
        context = (ROOT / '.dockerignore').read_text().splitlines()
        self.assertIn('**', context)
        self.assertNotIn('!.env', context)

    def test_build_context_holds_every_cargo_target_and_no_dead_rule(self):
        text = (ROOT / '.dockerignore').read_text()
        rules = dockerignore_rules(text)
        tracked = subprocess.run(['git', 'ls-files'], cwd=ROOT, check=True, capture_output=True,
                                 text=True).stdout.splitlines()
        # Cargo fails to load a manifest whose declared target file is missing (e2e_stack).
        targets = cargo_target_paths()
        self.assertIn('crates/pinakes-core/examples/e2e_stack.rs', targets)
        for path in sorted(targets) + ['data/type-mappings.json', 'crates/pinakes-core/src/agents/guide.md']:
            self.assertTrue(in_build_context(rules, path), path)
        self.assertTrue(any(p.startswith('migrations/') and in_build_context(rules, p) for p in tracked))
        for path in ['.env', 'pinakes.toml', '.git/config', 'target/release/pinakesd',
                     'e2e/node_modules/x.js', 'docs/deployment.md', '.runtime/x', 'Dockerfile']:
            self.assertFalse(in_build_context(rules, path), path)
        # Every rule changes what is sent: a rule hidden by a broader one is not a boundary.
        probe = tracked + ['.env', 'target/release/pinakesd']
        context = {p for p in probe if in_build_context(rules, p)}
        for i, (line, _, _) in enumerate(rules):
            without = rules[:i] + rules[i + 1:]
            changed = {p for p in probe if in_build_context(without, p)} != context
            self.assertTrue(changed, f'.dockerignore rule {line!r} changes nothing')
        # The comment must not promise a narrower context than the rules give.
        self.assertTrue(any(in_build_context(rules, p) for p in tracked if '/tests/' in p))
        self.assertIn('tests', text.split('**')[0])

    def test_smoke_uses_real_database_api_and_dump(self):
        w = workflow()
        job = w['jobs']['image']
        self.assertNotIn('secrets.', str(job))
        self.assertIn('scripts/ci/setup-postgres', steps(job))
        self.assertIn('scripts/ci/container-smoke', steps(job))
        build = next(s for s in job['steps'] if s.get('uses', '').startswith('docker/build-push-action@'))
        self.assertEqual(build['with']['push'], 'false')
        self.assertEqual(build['with']['load'], 'true')
        s = (ROOT / 'scripts/ci/container-smoke').read_text()
        for required in ['/v1/info', '/v1/search', '/v1/dumps/osm_place.ndjson.gz',
                         'CREATE DATABASE', '127.0.0.1:8090:8090', '0100007F:2382',
                         '--config /etc/pinakes/pinakes.toml']:
            self.assertIn(required, s)


    def test_smoke_cleans_up_and_requires_a_clean_exit(self):
        # Recording fakes for docker, curl, psql and sleep: no daemon, database or network.
        (ROOT / '.runtime').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=ROOT / '.runtime', prefix='ci-smoke-') as d:
            p = Path(d)
            (p / 'bin').mkdir()
            fakes = {
                'docker': """#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
a = sys.argv[1:]
with (Path(os.environ['FAKE_DIR']) / 'docker.jsonl').open('a') as f:
    f.write(json.dumps(a) + '\\n')
if a[0] == 'exec' and a[2:] == ['id', '-u']:
    print('10001')
elif a[0] == 'exec' and a[2:] == ['cat', '/proc/net/tcp']:
    print('  sl  local_address rem_address   st')
    print('   0: 0100007F:2382 00000000:0000 0A')
    print('   1: 00000000:1F9A 00000000:0000 0A')
elif a[0] == 'exec' and a[2:] == ['/usr/local/bin/pinakesd', 'healthcheck']:
    if os.environ.get('FAKE_UNHEALTHY'):
        sys.exit(1)
    print('healthy')
elif a[:2] == ['image', 'inspect'] and a[-1] == '{{json .Config.Healthcheck.Test}}':
    print(json.dumps(['CMD', '/usr/local/bin/pinakesd', 'healthcheck'], separators=(',', ':')))
elif a[:2] == ['image', 'inspect'] and a[-1] == '{{json .Config.ExposedPorts}}':
    print(json.dumps({'8090/tcp': {}}, separators=(',', ':')))
elif a[0] == 'inspect':
    print(os.environ['FAKE_EXIT'])
""",
                'curl': """#!/usr/bin/env python3
import gzip, json, os, sys
url = sys.argv[-1]
if os.environ.get('FAKE_DOWN'):
    sys.exit(7)
if url.endswith('/v1/info'):
    print(json.dumps({'version': '0.1.0'}))
elif '/v1/search' in url:
    print(json.dumps({'results': []}))
elif url.endswith('.ndjson.gz'):
    sys.stdout.buffer.write(gzip.compress(json.dumps({'licence': 'ODbL-1.0'}).encode() + b'\\n'))
""",
                'psql': '#!/bin/sh\nexit 0\n',
                'sleep': '#!/bin/sh\nexit 0\n',
            }
            for name, body in fakes.items():
                (p / 'bin' / name).write_text(body)
                (p / 'bin' / name).chmod(0o755)
            name = 'pinakes-smoke-7-1'
            for exit_code, down, unhealthy, ok in [('0', '', '', True), ('143', '', '', False),
                                                   ('0', '1', '', False), ('0', '', '1', False)]:
                (p / 'docker.jsonl').write_text('')
                env = {**os.environ, 'PATH': f'{p / "bin"}{os.pathsep}{os.environ["PATH"]}',
                       'GITHUB_ACTIONS': 'true', 'RUNNER_TEMP': d, 'FAKE_DIR': d, 'GITHUB_RUN_ID': '7',
                       'GITHUB_RUN_ATTEMPT': '1', 'DATABASE_URL': 'postgresql://ci@/postgres',
                       'FAKE_EXIT': exit_code, 'FAKE_DOWN': down, 'FAKE_UNHEALTHY': unhealthy}
                run = subprocess.run(['scripts/ci/container-smoke', 'pinakes:ci'], cwd=ROOT, env=env,
                                     capture_output=True, text=True)
                calls = [json.loads(line) for line in (p / 'docker.jsonl').read_text().splitlines()]
                self.assertEqual(run.returncode == 0, ok,
                                 (exit_code, down, unhealthy, run.stdout, run.stderr))
                # The container and its volume are removed on success and on failure.
                self.assertEqual(calls[-2:], [['rm', '--force', name], ['volume', 'rm', f'{name}-dumps']])
                if not down and not unhealthy:
                    stop = calls.index(['stop', '--time', '30', name])
                    self.assertEqual(calls[stop + 1], ['inspect', '--format', '{{.State.ExitCode}}', name])

class Release(unittest.TestCase):
    def test_release_waits_for_gates_and_only_tags_get_secrets(self):
        w = workflow()
        job = w['jobs']['publish']
        self.assertEqual(set(job['needs']), {'quality', 'image'})
        self.assertEqual(job['if'], "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')")
        self.assertEqual(job['steps'][1]['run'], 'python3 scripts/ci/release-tag "$RELEASE_TAG" >> "$GITHUB_OUTPUT"')
        shared = next(s for s in job['steps'] if s.get('uses', '').startswith('pubky/ci-workflows/'))
        args = shared['with']
        self.assertEqual(args['registry'], 'registry-1.docker.io/synonymsoft')
        self.assertEqual(args['image'], 'pinakes')
        self.assertEqual(args['push'], 'true')
        self.assertEqual(args['latest'], 'false')
        self.assertEqual(args['tags'], f'type=raw,value={PLATFORM_TAG}')
        self.assertEqual(args['platforms'], 'linux/${{ matrix.arch }}')
        self.assertEqual(args['sha_tag'], 'false')
        manifest = w['jobs']['manifest']
        self.assertEqual(manifest['if'], job['if'])
        self.assertIn('scripts/ci/publish-manifest', steps(manifest))
        self.assertNotIn('env', job)

    def test_publish_smokes_the_exact_pushed_platform_image(self):
        job = workflow()['jobs']['publish']
        pushed = f'registry-1.docker.io/synonymsoft/pinakes:{PLATFORM_TAG}'
        shared = step_index(job, lambda st: st.get('uses', '').startswith('pubky/ci-workflows/'), 'publishing')
        postgres = step_index(job, lambda st: st.get('run') == 'scripts/ci/setup-postgres', 'provisioning PostgreSQL')
        login = step_index(job, lambda st: st.get('uses', '').startswith('docker/login-action@'), 'logging in')
        pull = step_index(job, lambda st: 'docker pull' in st.get('run', ''), 'pulling the image')
        smoke = step_index(job, lambda st: 'scripts/ci/container-smoke' in st.get('run', ''), 'smoking the image')
        # Root package installs run before the registry credentials are written to disk.
        self.assertLess(postgres, login)
        self.assertLess(login, shared)
        self.assertLess(shared, pull)
        self.assertLess(pull, smoke)
        for i in [pull, smoke]:
            self.assertEqual(job['steps'][i]['env']['PUBLISHED_IMAGE'], pushed)
            self.assertIn('"$PUBLISHED_IMAGE"', job['steps'][i]['run'])
        self.assertIn('--platform "linux/$PUBLISHED_ARCH"', job['steps'][pull]['run'])
        self.assertEqual(job['steps'][pull]['env']['PUBLISHED_ARCH'], '${{ matrix.arch }}')
        self.assertEqual(job['steps'][smoke]['run'], 'scripts/ci/container-smoke "$PUBLISHED_IMAGE"')
        # manifest promotes only after every platform job, smoke included, succeeded.
        self.assertEqual(set(workflow()['jobs']['manifest']['needs']), {'publish'})

    def test_tags_publish_only_commits_on_main(self):
        w = workflow()
        for name in ['publish', 'manifest']:
            job = w['jobs'][name]
            self.assertNotIn('environment', job)
            # Full history and every branch (origin/main included) from checkout's own fetch.
            self.assertEqual(job['steps'][0]['with']['fetch-depth'], '0')
            check = step_index(job, lambda st: st.get('run') == 'scripts/ci/release-on-main >> "$GITHUB_OUTPUT"',
                               'checking main and resolving the commit')
            self.assertEqual(job['steps'][check]['id'], 'main')
            login = step_index(job, lambda st: st.get('uses', '').startswith('docker/login-action@'), 'logging in')
            self.assertLess(check, login)
            self.assertNotIn('secrets.', str(job['steps'][:login]))
            # One canonical commit: the raw event SHA (expression or variable) never names an image.
            self.assertNotIn('github.sha', str(job).lower())
        manifest = w['jobs']['manifest']
        promote = manifest['steps'][step_index(manifest, lambda st: st.get('run') == 'scripts/ci/publish-manifest',
                                               'promoting')]
        self.assertEqual(promote['env']['RELEASE_COMMIT'], '${{ steps.main.outputs.commit }}')
        self.assertTrue(all('environment' not in job for job in w['jobs'].values()))
        (ROOT / '.runtime').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=ROOT / '.runtime', prefix='ci-main-') as d:
            origin, clone = Path(d) / 'origin', Path(d) / 'clone'
            origin.mkdir()
            git(origin, 'init', '--quiet', '--initial-branch=main')
            first = commit(origin, 'a.txt', 'a\n', 'first')
            on_main = commit(origin, 'b.txt', 'b\n', 'second')
            git(origin, 'checkout', '--quiet', '-b', 'feature', first)
            feature = commit(origin, 'c.txt', 'c\n', 'feature only')
            git(origin, 'checkout', '--quiet', 'main')

            def run(repo, sha, head):
                # actions/checkout leaves a detached HEAD at the tagged commit.
                git(repo, 'checkout', '--quiet', '--detach', head)
                env = {**os.environ, 'GITHUB_SHA': sha, 'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1'}
                return subprocess.run([str(ROOT / 'scripts/ci/release-on-main')], cwd=repo, env=env,
                                      capture_output=True, text=True)

            def accepted(repo, sha, head, expected):
                p = run(repo, sha, head)
                self.assertEqual(p.returncode, 0, p.stderr)
                # stdout is the step output and nothing else.
                self.assertEqual(p.stdout, f'commit={expected}\n')
            git(origin, 'checkout', '--quiet', '-b', 'merged', on_main)
            inside = commit(origin, 'd.txt', 'd\n', 'reviewed only as part of the merge')
            git(origin, 'checkout', '--quiet', 'main')
            git(origin, 'merge', '--quiet', '--no-ff', '--no-gpg-sign', '-m', 'merge', 'merged')
            merge = git(origin, 'rev-parse', 'HEAD')
            # Annotated tags (the documented `git tag -s` makes one): a tag object, a tag of a
            # tag, a feature-branch tag and a tag of a tree.
            git(origin, 'tag', '--no-sign', '-a', '-m', 'release', 'v1.0.0', merge)
            git(origin, 'tag', '--no-sign', '-a', '-m', 'nested', 'v1.0.1', 'v1.0.0')
            git(origin, 'tag', '--no-sign', '-a', '-m', 'feature', 'v0.9.0', feature)
            git(origin, 'tag', '--no-sign', '-a', '-m', 'tree', 'v0.0.1', f'{merge}^{{tree}}')
            git(d, 'clone', '--quiet', '--no-local', str(origin), str(clone))
            tag_object = git(clone, 'rev-parse', 'refs/tags/v1.0.0')
            nested = git(clone, 'rev-parse', 'refs/tags/v1.0.1')
            self.assertEqual(git(clone, 'cat-file', '-t', tag_object), 'tag')
            self.assertNotEqual(tag_object, merge)
            accepted(clone, merge, merge, merge)
            # GitHub documents GITHUB_SHA as the commit; if it ever held the tag object, the
            # release still names and checks the commit, never the tag object.
            accepted(clone, merge, 'refs/tags/v1.0.0', merge)
            accepted(clone, tag_object, 'refs/tags/v1.0.0', merge)
            accepted(clone, nested, 'refs/tags/v1.0.1', merge)
            accepted(clone, on_main, on_main, on_main)
            accepted(clone, first, first, first)
            self.assertNotEqual(run(clone, inside, inside).returncode, 0)
            self.assertNotEqual(run(clone, feature, feature).returncode, 0)
            self.assertNotEqual(run(clone, git(clone, 'rev-parse', 'refs/tags/v0.9.0'), 'refs/tags/v0.9.0').returncode, 0)
            self.assertNotEqual(run(clone, git(clone, 'rev-parse', 'refs/tags/v0.0.1'), merge).returncode, 0)
            self.assertNotEqual(run(clone, 'f' * 40, merge).returncode, 0)
            self.assertNotEqual(run(clone, 'main', merge).returncode, 0)
            # The checked-out tree (the image's build context) must be the commit published.
            self.assertNotEqual(run(clone, on_main, merge).returncode, 0)
            self.assertNotEqual(run(clone, tag_object, on_main).returncode, 0)
            # Fail closed without main or with shallow history; a refusal writes no output.
            git(clone, 'update-ref', '-d', 'refs/remotes/origin/main')
            refused = run(clone, on_main, on_main)
            self.assertNotEqual(refused.returncode, 0)
            self.assertEqual(refused.stdout, '')
            shallow = Path(d) / 'shallow'
            git(d, 'clone', '--quiet', '--depth=1', '--no-single-branch', f'file://{origin}', str(shallow))
            self.assertNotEqual(run(shallow, merge, merge).returncode, 0)

    def test_manifest_tags_and_failure_before_promotion(self):
        # A recording Docker CLI tests the actual shell command boundary without
        # a Docker socket, credentials or registry writes.
        (ROOT / '.runtime').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=ROOT / '.runtime', prefix='ci-contract-') as d:
            p = Path(d)
            docker = p / 'docker'
            docker.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
p = Path(os.environ['RUNNER_TEMP'])
with (p / 'calls.jsonl').open('a') as f:
    f.write(json.dumps(sys.argv[1:]) + '\\n')
if '--raw' in sys.argv or '--dry-run' in sys.argv:
    arches = ['amd64'] if os.environ.get('MISSING_ARM') else ['amd64', 'arm64']
    print(json.dumps({'manifests': [{'platform': {'os': 'linux', 'architecture': a}} for a in arches]}))
""")
            docker.chmod(0o755)
            image = 'registry-1.docker.io/synonymsoft/pinakes:'
            commit_sha, tag_object = 'a' * 40, 'b' * 40
            cases = [('true', False, {}), ('false', False, {}), ('true', True, {}),
                     # Fail closed before any registry call on a bad commit or run ID.
                     ('true', None, {'RELEASE_COMMIT': ''}), ('true', None, {'RELEASE_COMMIT': 'main'}),
                     ('true', None, {'GITHUB_RUN_ID': ''}), ('true', None, {'GITHUB_RUN_ID': '1-arm64'})]
            for latest, missing, override in cases:
                calls = p / 'calls.jsonl'
                calls.write_text('')
                # GITHUB_SHA stands for an annotated tag's object: it must name nothing.
                env = {**os.environ, 'PATH': d + os.pathsep + os.environ['PATH'],
                       'RUNNER_TEMP': d, 'GITHUB_ACTIONS': 'true', 'GITHUB_EVENT_NAME': 'push',
                       'GITHUB_REF_TYPE': 'tag', 'GITHUB_SHA': tag_object, 'RELEASE_COMMIT': commit_sha,
                       'GITHUB_RUN_ID': '4242',
                       'RELEASE_IMAGE_TAG': 'v1.2.3' if latest == 'true' else 'v1.2.3-rc.1',
                       'RELEASE_LATEST': latest, 'MISSING_ARM': '1' if missing else '', **override}
                run = subprocess.run(['scripts/ci/publish-manifest'], cwd=ROOT, env=env, capture_output=True, text=True)
                recorded = [json.loads(line) for line in calls.read_text().splitlines()]
                mutations = [call for call in recorded if call[:3] == ['buildx', 'imagetools', 'create'] and '--dry-run' not in call]
                if missing is None:
                    self.assertNotEqual(run.returncode, 0, override)
                    self.assertEqual(recorded, [], override)
                    continue
                if missing:
                    self.assertNotEqual(run.returncode, 0)
                    self.assertEqual(mutations, [])
                    continue
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(len(mutations), 1)
                args = mutations[0]
                self.assertEqual(any(v.endswith(':latest') for v in args), latest == 'true')
                self.assertIn(image + env['RELEASE_IMAGE_TAG'], args)
                self.assertIn(image + commit_sha, args)
                # Exactly this run's smoke-tested platform images are combined.
                sources = [image + f'{commit_sha}-4242-{arch}' for arch in ['amd64', 'arm64']]
                self.assertEqual(args[-2:], sources)
                for call in recorded:
                    if '--dry-run' in call:
                        self.assertEqual(call[-2:], sources)
                    self.assertFalse(any(tag_object in v for v in call), call)

    def test_docs_match_the_release_path_and_name_required_checks(self):
        docs = ' '.join((ROOT / 'docs/deployment.md').read_text().split())
        # Run-scoped platform tags, under the commit an annotated tag points to.
        self.assertIn('`<full-commit-SHA>-<run-ID>-amd64`', docs)
        self.assertIn('`<full-commit-SHA>-<run-ID>-arm64`', docs)
        self.assertNotRegex(docs, r'<full-commit-SHA>-(amd64|arm64|<arch>)')
        self.assertIn('git rev-parse "$GITHUB_SHA^{commit}"', docs)
        self.assertIn('annotated tag, such as the signed one below, the commit-named image tags still name '
                      'the commit and never the tag object', docs)
        # main's branch protection must require every PR job, or "all PR hosted jobs must
        # pass before merge" is a convention, not a rule.
        prerequisites = re.search(r'Owner prerequisites, not set by this repository\'s files:(.*?)Provision repository',
                                  docs).group(1)
        jobs = workflow()['jobs']
        names = [jobs['quality']['name']] + [jobs['image']['name'].replace('${{ matrix.arch }}', arch)
                                             for arch in ['amd64', 'arm64']]
        self.assertEqual(names, ['Full test gate and mock e2e', 'Build and smoke image (amd64)',
                                 'Build and smoke image (arm64)'])
        self.assertIn('required status checks, without bypass', prerequisites)
        for name in names:
            self.assertIn(f'`{name}`', prerequisites)

    def test_semver_classification_and_rejection(self):
        cases = {'v1.2.3': 'true', 'v0.0.0': 'true', 'v1.2.3-rc.1': 'false',
                 'v1.2.3-beta.0+build.9': 'false', 'v1.2.3+build.9': 'true'}
        for tag, latest in cases.items():
            p = subprocess.run(['python3', 'scripts/ci/release-tag', tag], cwd=ROOT, capture_output=True, text=True)
            self.assertEqual(p.returncode, 0, p.stderr)
            image_tag = tag.replace('+', '_')
            self.assertEqual(p.stdout, f'latest={latest}\ntag={image_tag}\n')
        for tag in ['main', 'v1.2', 'v01.2.3', 'v1.2.3-01', 'v1.2.3-', 'v1.2.3\nx=1', 'v1.2.3-rc..1', 'v1.2.3-' + 'a' * 128]:
            p = subprocess.run(['python3', 'scripts/ci/release-tag', tag], cwd=ROOT, capture_output=True, text=True)
            self.assertNotEqual(p.returncode, 0, tag)
            self.assertEqual(p.stdout, '')


if __name__ == '__main__':
    unittest.main()
