import io
import copy
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))

import recovery


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.secrets = self.root / 'secrets'
        self.secrets.mkdir()
        (self.secrets / 'secrets.env').write_text('fixture')
        self.configuration = self.root / 'configuration'
        self.configuration.mkdir()
        (self.configuration / 'compose.yml').write_text('fixture')
        self.bundle = self.root / 'bundle'
        self.bundle.mkdir()

    def test_archive_restores_original_configuration_files(self):
        source = self.root / 'source'
        source.mkdir()
        (source / 'config').mkdir()
        (source / 'config/manifest.json').write_text('{"app":"original"}')
        (source / 'settings').write_bytes(b'original')
        archive = self.root / 'configuration.tar'
        recovery.archive_directory(source, archive)
        target = self.root / 'target'
        target.mkdir()
        recovery.extract_archive(archive, target)
        self.assertEqual((target / 'config/manifest.json').read_bytes(), (source / 'config/manifest.json').read_bytes())
        self.assertEqual((target / 'settings').read_bytes(), b'original')

    def test_archive_rejects_traversal_links_devices_and_duplicate_paths(self):
        for name, kind in [('../outside', tarfile.REGTYPE), ('/outside', tarfile.REGTYPE),
                           ('link', tarfile.SYMTYPE), ('device', tarfile.CHRTYPE)]:
            buffer = io.BytesIO()
            with tarfile.open(fileobj=buffer, mode='w') as archive:
                member = tarfile.TarInfo(name)
                member.type = kind
                archive.addfile(member)
            buffer.seek(0)
            with tarfile.open(fileobj=buffer) as archive, self.assertRaises(ValueError):
                recovery.safe_members(archive)

    def test_integrity_requires_every_artifact_and_mvcc_cut(self):
        for name in recovery.FILES:
            (self.bundle / name).write_text('fixture')
        manifest = self.manifest()
        (self.bundle / 'manifest.json').write_text(json.dumps(manifest))
        recovery.write_checksums(self.bundle)
        self.assertEqual(recovery.verify_directory(self.bundle), manifest)
        (self.bundle / 'primary.dump').write_text('tampered')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            recovery.verify_directory(self.bundle)

    def test_legacy_plaintext_image_archive_is_accepted_but_encrypted_snapshots_are_not(self):
        for name in recovery.FILES:
            (self.bundle / name).write_text('fixture')
        (self.bundle / 'images.tar').write_bytes(b'images')
        manifest = self.manifest()
        manifest.update(schemaVersion=3, consistency='writers-stopped')
        (self.bundle / 'manifest.json').write_text(json.dumps(manifest))
        recovery.write_checksums(self.bundle)
        self.assertEqual(recovery.verify_directory(self.bundle), manifest)

        current = self.manifest()
        (self.bundle / 'checksums.json').unlink()
        (self.bundle / 'manifest.json').write_text(json.dumps(current))
        recovery.write_checksums(self.bundle)
        with self.assertRaisesRegex(ValueError, 'legacy schema-3'):
            recovery.verify_directory(self.bundle)

        (self.bundle / 'checksums.json').unlink()
        (self.bundle / 'images.tar').unlink()
        (self.bundle / 'images.tar.age').write_bytes(b'encrypted images')
        recovery.write_checksums(self.bundle)
        with self.assertRaisesRegex(ValueError, 'unexpected'):
            recovery.verify_directory(self.bundle)

    def manifest(self):
        image = 'sha256:' + 'a' * 64
        return {'schemaVersion': 4, 'format': recovery.FORMAT, 'consistency': 'mvcc-snapshot',
                'source': {'revision': 'b' * 40, 'sourceSha256': 'c' * 64, 'dirty': False},
                'services': {role: image for role in ('server', 'dashboard')},
                'candidateImages': {role: 'example/' + role + '@sha256:' + 'd' * 64 for role in ('server', 'dashboard')},
                'databases': {'primary': {'database': 'primary', 'user': 'fixture', 'postgresMajor': 17, 'imageId': image}},
                'images': [{'reference': image, 'id': image}]}

    def test_schema_three_manifests_still_validate(self):
        legacy = copy.deepcopy(self.manifest())
        legacy.update(schemaVersion=3, consistency='writers-stopped')
        recovery.validate_bundle_manifest(legacy)
        mismatched = copy.deepcopy(self.manifest())
        mismatched.update(consistency='writers-stopped')
        with self.assertRaisesRegex(ValueError, 'MVCC'):
            recovery.validate_bundle_manifest(mismatched)

    def test_manifest_rejects_missing_or_unpinned_restoration_inputs_before_mutation(self):
        valid = self.manifest()
        recovery.validate_bundle_manifest(valid)
        mutations = [
            lambda value: value['services'].pop('dashboard'),
            lambda value: value['source'].update(revision='unreviewed'),
            lambda value: value['candidateImages'].update(server='example/server:latest'),
            lambda value: value['services'].update(server='sha256:' + 'f' * 64),
            lambda value: value['images'][0].update(id='not-an-image'),
            lambda value: value['databases']['primary'].update(postgresMajor=16),
            lambda value: value.update(schemaVersion=2, format='backendrelay-recovery-v2'),
            lambda value: value.update(imagesBundle='../../outside.tar.age'),
        ]
        for mutate in mutations:
            manifest = copy.deepcopy(valid)
            mutate(manifest)
            with self.assertRaises(ValueError):
                recovery.validate_bundle_manifest(manifest)

    def test_existing_restore_data_is_never_overwritten(self):
        (self.root / 'keep').write_text('existing')
        with self.assertRaisesRegex(ValueError, 'empty'):
            recovery.extract_archive(self.root / 'absent.tar', self.root)
        self.assertEqual((self.root / 'keep').read_text(), 'existing')

    def test_restore_requires_disposable_opt_in_before_any_command(self):
        with patch.object(recovery, 'run') as command, self.assertRaisesRegex(ValueError, 'disposable'):
            recovery.restore(Path('none'), Path('none'), {'disposable': False})
        command.assert_not_called()

    def run_restore(self, manifest, extra_files=(), image_snapshot=None, image_ids=()):
        manifest = dict(manifest, startedAt=datetime.now(timezone.utc).isoformat())
        staging = self.root / 'bundle-data'
        staging.mkdir()
        (staging / 'primary.dump').write_text('fixture')
        recovery.archive_directory(self.secrets, staging / 'server-secrets.tar')
        recovery.archive_directory(self.configuration, staging / 'configuration.tar')
        for name in extra_files:
            shutil.copyfile(self.root / name, staging / name)
        (staging / 'manifest.json').write_text(json.dumps(manifest))
        recovery.write_checksums(staging)
        inner = self.root / 'bundle-inner.tar'
        with tarfile.open(inner, 'w') as archive:
            for path in sorted(staging.iterdir()):
                archive.add(path, arcname=path.name, recursive=False)
        bundle = self.root / 'bundle.tar.age'
        bundle.write_bytes(b'encrypted bundle')
        scratch = self.root / 'scratch'
        scratch.mkdir()
        secrets_target = self.root / 'restored-secrets'
        secrets_target.mkdir()
        configuration_target = self.root / 'restored-configuration'
        configuration_target.mkdir()
        target = {'disposable': True,
                  'startCommand': ['start', str(configuration_target / 'backendrelay-restored-images.yml')],
                  'verifyCommand': ['verify'],
                  'databases': {'primary': 'restored'},
                  'directories': {'server-secrets': {'path': str(secrets_target),
                                                     'uid': os.getuid(), 'gid': os.getgid()},
                                  'configuration': {'path': str(configuration_target),
                                                    'uid': os.getuid(), 'gid': os.getgid()}},
                  'composeServices': {'server': 'server', 'dashboard': 'dashboard', 'primary': 'primary'},
                  'scratchDirectory': str(scratch),
                  'bundleSha256': recovery.digest(bundle)}
        calls = []

        def command(args, **kwargs):
            calls.append(args)
            if args[0] == 'age' and args[6] == str(bundle):
                shutil.copyfile(inner, args[5])
            elif args[0] == 'age':
                Path(args[5]).write_bytes(b'images')
            return None

        def inspect_image(name):
            if name in image_ids:
                return {'Id': name}
            raise ValueError('image is not present')

        with patch.object(recovery, 'headroom'), \
                patch.object(recovery, 'inspect', return_value={'Config': {'Labels': {'backendrelay.restore.disposable': 'true'}}}), \
                patch.object(recovery, 'query', side_effect=lambda name, sql: '17' if 'server_version_num' in sql else '0'), \
                patch.object(recovery, 'inspect_image', side_effect=inspect_image), \
                patch.object(recovery, 'run', side_effect=command):
            result = recovery.restore(bundle, Path('identity'), target, image_snapshot=image_snapshot)
        return result, calls

    def test_restore_with_images_decrypts_the_named_snapshot(self):
        manifest = self.manifest()
        image_id = manifest['images'][0]['id']
        snapshot = self.root / recovery.image_snapshot_name([image_id])
        snapshot.write_bytes(b'encrypted images')
        manifest['imagesBundle'] = snapshot.name
        result, calls = self.run_restore(manifest, image_snapshot=snapshot, image_ids=[image_id])
        self.assertEqual(result['status'], 'passed')
        decrypts = [args for args in calls if args[0] == 'age' and args[6] == str(snapshot)]
        self.assertEqual(len(decrypts), 1)
        self.assertEqual([args for args in calls if args[:3] == ['docker', 'image', 'load']],
                         [['docker', 'image', 'load', '--input', decrypts[0][5]]])

    def test_restore_with_images_refuses_a_different_snapshot(self):
        manifest = self.manifest()
        manifest['imagesBundle'] = recovery.image_snapshot_name(['sha256:' + 'e' * 64])
        other = self.root / 'other-images.tar.age'
        other.write_bytes(b'encrypted images')
        with self.assertRaises(ValueError) as raised:
            self.run_restore(manifest, image_snapshot=other, image_ids=[manifest['images'][0]['id']])
        self.assertIn(manifest['imagesBundle'], str(raised.exception))

    def test_restore_without_images_names_missing_ids_and_snapshot(self):
        manifest = self.manifest()
        manifest['imagesBundle'] = recovery.image_snapshot_name([manifest['images'][0]['id']])
        with self.assertRaises(ValueError) as raised:
            self.run_restore(manifest)
        message = str(raised.exception)
        self.assertIn(manifest['images'][0]['id'], message)
        self.assertIn(manifest['imagesBundle'], message)
        self.assertIn('--images', message)

    def test_restore_loads_legacy_plaintext_images_archive(self):
        manifest = self.manifest()
        manifest.update(schemaVersion=3, consistency='writers-stopped')
        (self.root / 'images.tar').write_bytes(b'images')
        result, calls = self.run_restore(manifest, extra_files=['images.tar'],
                                         image_ids=[manifest['images'][0]['id']])
        self.assertEqual(result['status'], 'passed')
        loads = [args for args in calls if args[:3] == ['docker', 'image', 'load']]
        self.assertEqual(len(loads), 1)
        self.assertTrue(loads[0][4].endswith('images.tar'))

    def test_saved_registry_digest_does_not_need_repo_digest_after_load(self):
        image_id = 'sha256:' + 'a' * 64
        with patch.object(recovery, 'inspect_image', return_value={'Id': image_id}) as inspect:
            recovery.verify_loaded_images([{'reference': 'example/server@sha256:' + 'b' * 64, 'id': image_id}])
        inspect.assert_called_once_with(image_id)

    def test_mutable_image_tags_are_rejected(self):
        image_id = 'sha256:' + 'a' * 64
        with patch.object(recovery, 'inspect_image') as inspect:
            with self.assertRaisesRegex(ValueError, 'immutable'):
                recovery.verify_loaded_images([{'reference': 'example/server:latest', 'id': image_id}])
        inspect.assert_not_called()

    def test_restart_attempts_every_writer_when_one_fails(self):
        containers = ['first', 'second']
        calls = []
        def run(args, **kwargs):
            calls.append(args[-1])
            if args[-1] == 'first':
                raise ValueError('start failed')
        with patch.object(recovery, 'run', side_effect=run), self.assertRaises(ValueError):
            recovery.restart_writers(containers)
        self.assertEqual(calls, ['first', 'second'])
        self.assertEqual(containers, ['first'])

    def test_resume_journal_rejects_replaced_container_and_keeps_receipt(self):
        recovery.save_journal(self.root, [{'name': 'server', 'id': 'a' * 64}])
        with patch.object(recovery, 'inspect', return_value={'Id': 'b' * 64}), patch.object(recovery, 'run') as command:
            with self.assertRaisesRegex(ValueError, 'replaced'):
                recovery.resume_journal(self.root, {'server': 'server'})
        command.assert_not_called()
        self.assertTrue((self.root / 'writers-stopped.json').exists())

    def test_resume_journal_restarts_only_originally_running_writers(self):
        recovery.save_journal(self.root, [{'name': 'server', 'id': 'a' * 64}])
        with patch.object(recovery, 'inspect', return_value={'Id': 'a' * 64}), patch.object(recovery, 'run') as command:
            recovery.resume_journal(self.root, {'server': 'server'})
        command.assert_called_once_with(['docker', 'start', 'server'], timeout=90)
        self.assertFalse((self.root / 'writers-stopped.json').exists())

    def backup_config(self):
        return {'backupRoot': str(self.root / 'backups'), 'ageRecipients': ['public'],
                'uploadCommand': ['unused'], 'verifyOffhostCommand': ['unused'],
                'services': {'server': 's', 'dashboard': 'd'},
                'databases': {'primary': 'p'},
                'configurationDirectory': str(self.configuration),
                'source': {'revision': 'a' * 40, 'sourceSha256': 'b' * 64, 'dirty': False},
                'candidateImages': {role: 'example/' + role + '@sha256:' + 'a' * 64 for role in ('server', 'dashboard')}}

    def fake_encrypt(self, config, plain, encrypted):
        encrypted.write_bytes(b'encrypted bundle')

    def test_backup_takes_mvcc_snapshot_without_stopping_writers(self):
        config = self.backup_config()
        calls = []
        captured = {}

        def info(name):
            return {'Id': 'a' * 64, 'Image': 'sha256:' + 'a' * 64, 'State': {'Running': True},
                    'Mounts': [{'Destination': '/home/instant/config', 'Source': str(self.secrets)}]}

        def command(args, **kwargs):
            calls.append(args)
            if args[0] == 'docker' and args[1:4] == ['exec', 'p', 'sh']:
                return {'SELECT current_database()': 'primary',
                        'SELECT current_user': 'fixture'}.get(args[-1], '17')
            return None

        def verify(directory):
            captured.update(json.loads((directory / 'manifest.json').read_text()))
            return captured

        with patch.object(recovery, 'inspect', side_effect=info), \
                patch.object(recovery, 'inspect_image', return_value={'Id': 'sha256:' + 'a' * 64}), \
                patch.object(recovery, 'headroom'), \
                patch.object(recovery, 'mount', return_value=self.secrets), \
                patch.object(recovery, 'run', side_effect=command), \
                patch.object(recovery, 'encrypt_file', side_effect=self.fake_encrypt), \
                patch.object(recovery, 'upload', return_value='f' * 64), \
                patch.object(recovery, 'verify_directory', side_effect=verify):
            recovery.backup(config)

        self.assertFalse(any(command_args[:2] == ['docker', 'stop'] for command_args in calls))
        self.assertFalse(any(command_args[:3] == ['docker', 'image', 'save'] for command_args in calls))
        self.assertEqual(captured['consistency'], 'mvcc-snapshot')
        self.assertEqual(captured['schemaVersion'], 4)
        self.assertNotIn('imagesBundle', captured)
        self.assertFalse((self.root / 'backups/writers-stopped.json').exists())

    def test_backup_records_image_snapshot_without_copying_it(self):
        config = self.backup_config()
        image_id = 'sha256:' + 'a' * 64
        snapshot = recovery.image_snapshot_path(config['backupRoot'], [image_id])
        snapshot.parent.mkdir(parents=True)
        snapshot.write_bytes(b'already encrypted')
        captured, bundle_names, calls = {}, set(), []

        def info(name):
            return {'Id': 'a' * 64, 'Image': image_id, 'State': {'Running': True},
                    'Mounts': [{'Destination': '/home/instant/config', 'Source': str(self.secrets)}]}

        def command(args, **kwargs):
            calls.append(args)
            if args[0] == 'docker' and args[1:4] == ['exec', 'p', 'sh']:
                return {'SELECT current_database()': 'primary',
                        'SELECT current_user': 'fixture'}.get(args[-1], '17')
            return None

        def verify(directory):
            captured.update(json.loads((directory / 'manifest.json').read_text()))
            return captured

        def encrypt(config, plain, encrypted):
            with tarfile.open(plain) as archive:
                bundle_names.update(archive.getnames())
            encrypted.write_bytes(b'encrypted bundle')

        with patch.object(recovery, 'inspect', side_effect=info), \
                patch.object(recovery, 'inspect_image', return_value={'Id': image_id}), \
                patch.object(recovery, 'headroom'), \
                patch.object(recovery, 'mount', return_value=self.secrets), \
                patch.object(recovery, 'run', side_effect=command), \
                patch.object(recovery, 'encrypt_file', side_effect=encrypt), \
                patch.object(recovery, 'upload', return_value='f' * 64), \
                patch.object(recovery, 'verify_directory', side_effect=verify):
            recovery.backup(config)

        self.assertEqual(captured['imagesBundle'], snapshot.name)
        self.assertFalse(bundle_names & {'images.tar', 'images.tar.age'})
        self.assertEqual(snapshot.read_bytes(), b'already encrypted')
        self.assertFalse(any(command_args[:3] == ['docker', 'image', 'save'] for command_args in calls))

    def test_remote_dsn_never_places_the_password_in_argv_or_errors(self):
        dsn = self.root / 'primary.dsn'
        dsn.write_text('postgresql://backup:s3cret-password@db.example.test:5432/instant?sslmode=require\n')
        dsn.chmod(0o600)
        config = self.backup_config()
        config['databases'] = {'primary': {'dsnFile': str(dsn)}}
        captured = {}
        calls = []

        def command(args, **kwargs):
            calls.append((args, kwargs.get('env')))
            if args == ['pg_dump', '--version']:
                return 'pg_dump (PostgreSQL) 17.2'
            if args[0] == 'psql':
                return {'SELECT current_database()': 'primary',
                        'SELECT current_user': 'fixture'}.get(args[-1], '17')
            return None

        def verify(directory):
            captured.update(json.loads((directory / 'manifest.json').read_text()))
            return captured

        with patch.object(recovery, 'inspect', return_value={'Id': 'a' * 64, 'Image': 'sha256:' + 'a' * 64,
                                                             'State': {'Running': True}}), \
                patch.object(recovery, 'inspect_image', return_value={'Id': 'sha256:' + 'a' * 64}), \
                patch.object(recovery, 'headroom'), \
                patch.object(recovery, 'mount', return_value=self.secrets), \
                patch.object(recovery, 'run', side_effect=command), \
                patch.object(recovery, 'encrypt_file', side_effect=self.fake_encrypt), \
                patch.object(recovery, 'upload', return_value='f' * 64), \
                patch.object(recovery, 'verify_directory', side_effect=verify):
            recovery.backup(config)

        self.assertNotIn('s3cret-password', ' '.join(part for args, _ in calls for part in args))
        dump_environments = [environment for args, environment in calls
                             if args[0] in ('pg_dump', 'psql') and environment is not None]
        self.assertTrue(dump_environments)
        self.assertTrue(all(environment.get('PGPASSWORD') == 's3cret-password' for environment in dump_environments))
        self.assertFalse(any('s3cret-password' in part for args, _ in calls for part in args))
        self.assertEqual(captured['databases']['primary']['database'], 'primary')
        self.assertNotIn('imageId', captured['databases']['primary'])

        dsn.write_text('postgresql://backup:s3cret-password@db.example.test/one\npostgresql://leak/\n')
        with self.assertRaises(ValueError) as raised:
            recovery.database_source({'dsnFile': str(dsn)})
        self.assertNotIn('s3cret-password', str(raised.exception))

    def test_restore_age_guard_and_allow_older_than_boundaries(self):
        fresh = {'startedAt': (datetime.now(timezone.utc) - timedelta(hours=23)).isoformat()}
        self.assertGreater(recovery.validate_backup_age(fresh), 0)
        old = {'startedAt': (datetime.now(timezone.utc) - timedelta(hours=25)).isoformat()}
        with self.assertRaisesRegex(ValueError, '24 hours'):
            recovery.validate_backup_age(old)
        self.assertGreater(recovery.validate_backup_age(old, 2), 24 * 3600)
        with self.assertRaisesRegex(ValueError, 'allowed age'):
            recovery.validate_backup_age(old, 1)
        with self.assertRaisesRegex(ValueError, 'positive'):
            recovery.validate_backup_age(old, 0)
        future = {'startedAt': (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()}
        with self.assertRaisesRegex(ValueError, 'future'):
            recovery.validate_backup_age(future, 30)

    def test_snapshot_images_skips_an_existing_snapshot(self):
        config = self.backup_config()
        image_id = 'sha256:' + 'a' * 64
        snapshot = recovery.image_snapshot_path(config['backupRoot'], [image_id])
        snapshot.parent.mkdir(parents=True)
        snapshot.write_bytes(b'already encrypted')
        with patch.object(recovery, 'inspect', return_value={'Id': 'a' * 64, 'Image': image_id,
                                                             'State': {'Running': True}}), \
                patch.object(recovery, 'inspect_image', return_value={'Id': image_id}), \
                patch.object(recovery, 'headroom'), \
                patch.object(recovery, 'run') as command:
            result = recovery.snapshot_images(config)
        command.assert_not_called()
        self.assertEqual(result['status'], 'exists')
        self.assertEqual(result['imagesBundle'], snapshot.name)
