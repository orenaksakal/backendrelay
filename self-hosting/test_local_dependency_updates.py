import os
from pathlib import Path
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / '.github/dependabot.yml'
SEMVER_ECOSYSTEMS = {'cargo', 'npm', 'github-actions'}
IGNORED_DIRECTORIES = {'.git', '.plan', '.turbo', 'node_modules', 'target'}


def directories(entry):
    """Return the configured directories, accepting both spellings Dependabot allows."""
    values = entry['directories'] if 'directories' in entry else [entry['directory']]
    return {'/' if not value.strip('/') else '/' + value.strip('/') for value in values}


def dockerfile_directories():
    found = set()
    for directory, subdirectories, filenames in os.walk(ROOT):
        subdirectories[:] = [name for name in subdirectories if name not in IGNORED_DIRECTORIES]
        if any(name.startswith('Dockerfile') for name in filenames):
            relative = Path(directory).relative_to(ROOT).as_posix()
            found.add('/' if relative == '.' else '/' + relative)
    return found


class DependencyUpdateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.config = yaml.safe_load(CONFIG.read_text())
        cls.entries = cls.config['updates']

    def entries_for(self, ecosystem):
        return [entry for entry in self.entries if entry['package-ecosystem'] == ecosystem]

    def test_weekly_grouped_prs_with_a_bounded_queue(self):
        self.assertEqual(self.config['version'], 2)
        self.assertEqual({entry['package-ecosystem'] for entry in self.entries},
                         SEMVER_ECOSYSTEMS | {'docker', 'docker-compose'})
        for entry in self.entries:
            ecosystem = entry['package-ecosystem']
            self.assertEqual(entry['schedule'], {'interval': 'weekly', 'day': 'monday'}, ecosystem)
            self.assertEqual(entry['open-pull-requests-limit'], 5, ecosystem)
            groups = entry['groups']
            self.assertIn('all', groups, ecosystem)
            self.assertEqual(groups['all']['patterns'], ['*'], ecosystem)
            if ecosystem in SEMVER_ECOSYSTEMS:
                # Dependabot assigns a dependency to the first matching group.
                self.assertEqual(list(groups), ['major', 'all'], ecosystem)
                self.assertEqual(groups['major']['update-types'], ['major'], ecosystem)
                self.assertEqual(groups['all']['update-types'], ['minor', 'patch'], ecosystem)
            else:
                self.assertEqual(list(groups), ['all'], ecosystem)
                self.assertNotIn('update-types', groups['all'], ecosystem)

    def test_no_duplicate_ecosystem_and_directory_pairs(self):
        pairs = [(entry['package-ecosystem'], directory)
                 for entry in self.entries for directory in directories(entry)]
        self.assertEqual(len(pairs), len(set(pairs)))

    def test_rust_and_client_roots_are_covered(self):
        self.assertEqual([directories(entry) for entry in self.entries_for('cargo')],
                         [{'/server-rs'}])
        self.assertTrue((ROOT / 'server-rs/Cargo.toml').is_file())
        # cel is patched to the vendored copy, so Dependabot must not raise it.
        self.assertEqual([item['dependency-name'] for item in self.entries_for('cargo')[0]['ignore']],
                         ['cel'])
        self.assertIn('cel = { path = "vendor/cel" }', (ROOT / 'server-rs/Cargo.toml').read_text())

        self.assertEqual([directories(entry) for entry in self.entries_for('npm')],
                         [{'/client'}])
        self.assertTrue((ROOT / 'client/pnpm-lock.yaml').is_file())

    def test_actions_and_compose_stacks_are_covered(self):
        self.assertEqual([directories(entry) for entry in self.entries_for('github-actions')], [{'/'}])
        self.assertEqual([directories(entry) for entry in self.entries_for('docker-compose')],
                         [{'/self-hosting'}])
        self.assertTrue(list((ROOT / 'self-hosting').glob('docker-compose*.yml')))

    def test_every_dockerfile_directory_is_covered(self):
        configured = set().union(*(directories(entry) for entry in self.entries_for('docker')))
        self.assertEqual(configured, dockerfile_directories())

    def test_configured_directories_exist(self):
        for entry in self.entries:
            for directory in directories(entry):
                path = ROOT if directory == '/' else ROOT / directory.lstrip('/')
                self.assertTrue(path.is_dir(), f"{entry['package-ecosystem']}: {directory}")
