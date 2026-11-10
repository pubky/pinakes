"""Regression contracts for the CI trust boundary and release packaging."""
from pathlib import Path
import json
import os
import tempfile
import re
import subprocess
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


def workflow():
    return yaml.load((ROOT / '.github/workflows/ci.yml').read_text(), Loader=yaml.BaseLoader)


def steps(job):
    return '\n'.join(step.get('run', '') for step in job['steps'])


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
        for required in ['postgresql-16-postgis-3', 'postgresql-16-h3=4.2.3-',
                         'pg_trgm', 'h3_latlng_to_cell', '4.2.3', 'ON_ERROR_STOP=1']:
            self.assertIn(required, s)
        self.assertIn('pinakes-ci', s)


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
        self.assertNotIn('COPY . ', s)
        context = (ROOT / '.dockerignore').read_text().splitlines()
        self.assertIn('**', context)
        self.assertIn('!crates/*/build.rs', context)
        self.assertNotIn('!.env', context)

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
        self.assertEqual(args['tags'], 'type=raw,value=${{ github.sha }}-${{ matrix.arch }}')
        self.assertEqual(args['platforms'], 'linux/${{ matrix.arch }}')
        self.assertEqual(args['sha_tag'], 'false')
        manifest = w['jobs']['manifest']
        self.assertEqual(manifest['if'], job['if'])
        self.assertIn('scripts/ci/publish-manifest', steps(manifest))
        self.assertNotIn('secrets.', str(job.get('env', {})))

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
            for latest, missing in [('true', False), ('false', False), ('true', True)]:
                calls = p / 'calls.jsonl'
                calls.write_text('')
                env = {**os.environ, 'PATH': d + os.pathsep + os.environ['PATH'],
                       'RUNNER_TEMP': d, 'GITHUB_ACTIONS': 'true', 'GITHUB_EVENT_NAME': 'push',
                       'GITHUB_REF_TYPE': 'tag', 'GITHUB_SHA': 'a' * 40,
                       'RELEASE_IMAGE_TAG': 'v1.2.3' if latest == 'true' else 'v1.2.3-rc.1',
                       'RELEASE_LATEST': latest, 'MISSING_ARM': '1' if missing else ''}
                run = subprocess.run(['scripts/ci/publish-manifest'], cwd=ROOT, env=env, capture_output=True, text=True)
                recorded = [json.loads(line) for line in calls.read_text().splitlines()]
                mutations = [call for call in recorded if call[:3] == ['buildx', 'imagetools', 'create'] and '--dry-run' not in call]
                if missing:
                    self.assertNotEqual(run.returncode, 0)
                    self.assertEqual(mutations, [])
                    continue
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(len(mutations), 1)
                args = mutations[0]
                self.assertEqual(any(v.endswith(':latest') for v in args), latest == 'true')
                self.assertIn('registry-1.docker.io/synonymsoft/pinakes:' + env['RELEASE_IMAGE_TAG'], args)
                self.assertIn('registry-1.docker.io/synonymsoft/pinakes:' + 'a' * 40, args)
                for arch in ['amd64', 'arm64']:
                    self.assertIn('registry-1.docker.io/synonymsoft/pinakes:' + 'a' * 40 + '-' + arch, args)

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
