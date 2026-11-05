#!/usr/bin/env python3
"""Encrypted off-host backups and original-identity recovery for one VPS.

Only operator-supplied configuration may select containers, paths or commands.
Bundle contents never supply commands to execute. No credentials are printed.
"""
import argparse
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.parse
import urllib.request
import uuid

FORMAT = 'backendrelay-recovery-v3'
SCHEMA_VERSION = 4
LEGACY_SCHEMA_VERSION = 3
IMAGE_ID = re.compile(r'^sha256:[a-f0-9]{64}$')
IMAGE_REF = re.compile(r'^[a-z0-9._:/-]+@sha256:[a-f0-9]{64}$')
FILES = {'primary.dump', 'server-secrets.tar',
         'configuration.tar', 'manifest.json'}
# Only schema-3 bundles carried a plaintext image archive; images are now saved
# once per deploy by snapshot-images and restored with --images.
OPTIONAL_FILES = {'images.tar'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def now():
    return datetime.now(timezone.utc).isoformat()


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def run(argv, *, stdin=None, stdout=None, timeout=3600, env=None):
    require(isinstance(argv, list) and argv and all(isinstance(a, str) and a for a in argv), 'invalid operator command')
    result = subprocess.run(argv, stdin=stdin, stdout=stdout or subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout, env=env)
    require(result.returncode == 0, 'operator command failed; output withheld to protect credentials')
    return result.stdout.decode().strip() if stdout is None else None


def inspect(name):
    return json.loads(run(['docker', 'inspect', name]))[0]


def inspect_image(name):
    return json.loads(run(['docker', 'image', 'inspect', name]))[0]


def verify_loaded_images(images):
    for image in images:
        require(IMAGE_ID.fullmatch(image['id']), 'invalid restored Docker image ID')
        require(IMAGE_ID.fullmatch(image['reference']) or IMAGE_REF.fullmatch(image['reference']), 'restored image reference must be immutable')
        require(inspect_image(image['id'])['Id'] == image['id'], 'restored Docker image identity mismatch')


def missing_images(images):
    missing = []
    for image in images:
        require(IMAGE_ID.fullmatch(image['id']), 'invalid restored Docker image ID')
        require(IMAGE_ID.fullmatch(image['reference']) or IMAGE_REF.fullmatch(image['reference']), 'restored image reference must be immutable')
        try:
            missing_image = inspect_image(image['id'])['Id'] != image['id']
        except (ValueError, OSError, subprocess.SubprocessError):
            missing_image = True
        if missing_image:
            missing.append(image['id'])
    return missing


def parse_dsn(entry):
    """Read one operator-owned DSN file without carrying the password in argv."""
    require(isinstance(entry, dict) and set(entry) == {'dsnFile'}, 'database must be a container name or {"dsnFile": "/absolute/path"}')
    path = Path(entry['dsnFile'])
    require(path.is_absolute() and path.is_file() and not path.is_symlink(), 'database DSN file must be an absolute regular file')
    require(stat.S_IMODE(path.stat().st_mode) == 0o600, 'database DSN file must have mode 0600')
    text = path.read_text().strip()
    require('\n' not in text and text.startswith('postgresql://'), 'database DSN file must contain one postgresql:// URL')
    parts = urllib.parse.urlsplit(text)
    require(parts.scheme in ('postgresql', 'postgres') and parts.hostname, 'invalid database DSN')
    password = None
    netloc = parts.netloc
    if '@' in netloc:
        userinfo, host = netloc.rsplit('@', 1)
        username, separator, secret = userinfo.partition(':')
        password = urllib.parse.unquote(secret) if separator else None
        netloc = (username + '@' if username else '') + host
    url = urllib.parse.urlunsplit((parts.scheme, netloc, parts.path, parts.query, parts.fragment))
    return {'kind': 'dsn', 'url': url, 'password': password}


def database_source(entry):
    if isinstance(entry, str):
        require(entry and len(entry) <= 128, 'invalid database container name')
        return {'kind': 'container', 'name': entry}
    return parse_dsn(entry)


def database_environment(database):
    if database['kind'] == 'container' or database['password'] is None:
        return None
    environment = os.environ.copy()
    environment['PGPASSWORD'] = database['password']
    return environment


def query(database, sql):
    if isinstance(database, str):
        database = {'kind': 'container', 'name': database}
    if database['kind'] == 'container':
        return run(['docker', 'exec', database['name'], 'sh', '-ec',
                    'exec psql -X -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -At -c "$1"', 'sh', sql], timeout=60)
    return run(['psql', '-X', '-v', 'ON_ERROR_STOP=1', '-At', '--dbname=' + database['url'], '-c', sql],
               timeout=60, env=database_environment(database))


def require_local_pg_dump():
    version = run(['pg_dump', '--version'])
    match = re.search(r'(\d+)', version)
    require(match and int(match.group(1)) == 17, 'local pg_dump major version 17 required for a remote database')


def dump_database(database, target):
    if database['kind'] == 'container':
        with target.open('xb') as out:
            run(['docker', 'exec', database['name'], 'sh', '-ec', 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom --compress=6 --no-owner --no-acl'], stdout=out)
        with target.open('rb') as source:
            run(['docker', 'exec', '-i', database['name'], 'pg_restore', '--list'], stdin=source)
    else:
        require_local_pg_dump()
        with target.open('xb') as out:
            run(['pg_dump', '--dbname=' + database['url'], '--format=custom', '--compress=6', '--no-owner', '--no-acl'],
                stdout=out, env=database_environment(database))
        with target.open('rb') as source:
            run(['pg_restore', '--list'], stdin=source)


def mount(info, destination):
    values = [m['Source'] for m in info['Mounts'] if m['Destination'] == destination]
    require(len(values) == 1, 'expected one configured data mount')
    return Path(values[0])


def headroom(path, reserve):
    require(type(reserve) is int and reserve >= 5 * 1024**3, 'reserve must be at least 5 GiB')
    require(shutil.disk_usage(path).free >= reserve, 'insufficient backup/restore disk headroom')


def safe_members(archive, maximum_bytes=1024**4):
    seen, total = set(), 0
    members = archive.getmembers()
    for item in members:
        path = Path(item.name)
        require(item.name and not path.is_absolute() and '..' not in path.parts,
                'unsafe archive path')
        require(item.isfile() or item.isdir(), 'archive links/devices are forbidden')
        key = str(path)
        require(key not in seen, 'duplicate archive path')
        seen.add(key)
        total += item.size
        require(total <= maximum_bytes, 'archive exceeds restore size budget')
    return members


def archive_directory(source, target):
    require(source.is_dir() and not source.is_symlink(), 'source directory must be real')
    with tarfile.open(target, 'w') as archive:
        for path in sorted(source.rglob('*')):
            require(not path.is_symlink() and (path.is_file() or path.is_dir()), 'backup source contains links/devices')
            archive.add(path, arcname=str(path.relative_to(source)), recursive=False)


def extract_archive(source, target, maximum_bytes=1024**4, owner=None):
    require(target.is_dir() and not target.is_symlink() and not any(target.iterdir()), 'restore directory must be empty')
    with tarfile.open(source, 'r:') as archive:
        members = safe_members(archive, maximum_bytes)
        uid, gid = owner or (os.getuid(), os.getgid())
        require(type(uid) is int and type(gid) is int and uid >= 0 and gid >= 0, 'invalid restore owner')
        for item in members:
            item.uid = uid
            item.gid = gid
            item.uname = item.gname = ''
            item.mode = 0o700 if item.isdir() else 0o600
        archive.extractall(target, members=members, filter='data')
        # Python's data filter discards archive ownership; apply the explicit
        # trusted target ownership, including the root of each mounted volume.
        for path in [target, *target.rglob('*')]:
            os.chown(path, uid, gid)


def restart_writers(containers):
    failed = []
    for container in list(containers):
        try:
            run(['docker', 'start', container], timeout=90)
            containers.remove(container)
        except (ValueError, OSError, subprocess.SubprocessError):
            failed.append(container)
    require(not failed, 'one or more writer containers could not restart')


def save_journal(root, entries):
    temporary = root / '.writers-stopped.tmp'
    with temporary.open('w') as file:
        json.dump({'format': FORMAT, 'writers': entries}, file)
        file.flush()
        os.fsync(file.fileno())
    os.replace(temporary, root / 'writers-stopped.json')
    descriptor = os.open(root, os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def resume_journal(root, services):
    path = root / 'writers-stopped.json'
    if not path.exists():
        return
    require(path.is_file() and not path.is_symlink(), 'invalid writer recovery journal')
    journal = json.loads(path.read_text())
    require(journal.get('format') == FORMAT and isinstance(journal.get('writers'), list), 'invalid writer recovery journal')
    allowed = {services['server']}
    names = []
    for entry in journal['writers']:
        require(entry.get('name') in allowed and re.fullmatch('[a-f0-9]{64}', entry.get('id', '')), 'journal has unexpected writer identity')
        require(inspect(entry['name'])['Id'] == entry['id'], 'writer was replaced since backup; manual recovery required')
        names.append(entry['name'])
    restart_writers(names)
    path.unlink()


def write_checksums(directory):
    data = {p.name: digest(p) for p in sorted(directory.iterdir()) if p.is_file()}
    (directory / 'checksums.json').write_text(json.dumps(data, indent=2) + '\n')


def validate_bundle_manifest(manifest):
    require(isinstance(manifest, dict) and manifest.get('format') == FORMAT
            and manifest.get('schemaVersion') in (LEGACY_SCHEMA_VERSION, SCHEMA_VERSION),
            'unsupported backup format; use the original tool for legacy bundles')
    schema_version = manifest['schemaVersion']
    if schema_version == SCHEMA_VERSION:
        require(manifest.get('consistency') == 'mvcc-snapshot', 'MVCC snapshot database backup required')
    else:
        require(manifest.get('consistency') == 'writers-stopped', 'quiesced database backup required')
    source = manifest.get('source', {})
    require(isinstance(source, dict) and re.fullmatch('[a-f0-9]{40}', source.get('revision', ''))
            and re.fullmatch('[a-f0-9]{64}', source.get('sourceSha256', ''))
            and type(source.get('dirty')) is bool, 'invalid backup source identity')
    services = manifest.get('services', {})
    candidates = manifest.get('candidateImages', {})
    require(set(services) == set(candidates) == {'server', 'dashboard'}, 'complete backup service roles required')
    require(all(isinstance(image, str) and IMAGE_ID.fullmatch(image) for image in services.values())
            and all(isinstance(image, str) and IMAGE_REF.fullmatch(image) for image in candidates.values()), 'immutable backup service images required')
    databases = manifest.get('databases', {})
    require(set(databases) == {'primary'}, 'primary database required')
    for database in databases.values():
        require(isinstance(database, dict) and database.get('postgresMajor') == 17
                and all(isinstance(database.get(key), str) and 0 < len(database[key]) <= 128 for key in ('database', 'user')), 'invalid backup database identity')
        if schema_version == LEGACY_SCHEMA_VERSION or 'imageId' in database:
            require(isinstance(database.get('imageId'), str) and IMAGE_ID.fullmatch(database['imageId']), 'invalid backup database identity')
    images_bundle = manifest.get('imagesBundle')
    if images_bundle is not None:
        require(isinstance(images_bundle, str) and Path(images_bundle).name == images_bundle
                and images_bundle.endswith('.tar.age'), 'invalid image snapshot name')
    images = manifest.get('images')
    require(isinstance(images, list) and images, 'backup image inventory required')
    references, identifiers = set(), set()
    for image in images:
        require(isinstance(image, dict) and isinstance(image.get('id'), str)
                and IMAGE_ID.fullmatch(image['id']), 'invalid backup image ID')
        reference = image.get('reference')
        require(isinstance(reference, str) and (IMAGE_ID.fullmatch(reference) or IMAGE_REF.fullmatch(reference)), 'invalid backup image reference')
        require(reference not in references, 'duplicate backup image reference')
        require(not IMAGE_ID.fullmatch(reference) or reference == image['id'], 'backup image reference/ID mismatch')
        references.add(reference)
        identifiers.add(image['id'])
    database_images = {database['imageId'] for database in databases.values() if 'imageId' in database}
    require(set(services.values()) | database_images <= identifiers,
            'backup service image missing from archive inventory')


def verify_directory(directory):
    sums = json.loads((directory / 'checksums.json').read_text())
    require(FILES <= set(sums) <= FILES | OPTIONAL_FILES, 'backup artifact set is incomplete or unexpected')
    require({p.name for p in directory.iterdir()} == set(sums) | {'checksums.json'}, 'unexpected bundle entries')
    for name, expected in sums.items():
        path = directory / name
        require(path.is_file() and not path.is_symlink() and re.fullmatch('[a-f0-9]{64}', expected)
                and digest(path) == expected, 'backup artifact checksum mismatch')
    manifest = json.loads((directory / 'manifest.json').read_text())
    validate_bundle_manifest(manifest)
    require('images.tar' not in sums or manifest['schemaVersion'] == LEGACY_SCHEMA_VERSION,
            'image archive is only supported in legacy schema-3 bundles')
    return manifest


def image_snapshot_name(identifiers):
    return hashlib.sha256('\n'.join(sorted(set(identifiers))).encode()).hexdigest() + '.tar.age'


def image_snapshot_path(root, identifiers):
    return Path(root) / 'images' / image_snapshot_name(identifiers)


def encrypt_file(config, plain, encrypted):
    args = ['age', '--encrypt']
    for recipient in config['ageRecipients']:
        args += ['--recipient', recipient]
    run([*args, '--output', str(encrypted), str(plain)])


def upload(config, encrypted):
    local_hash = digest(encrypted)
    replacements = {'{bundle}': str(encrypted), '{name}': encrypted.name, '{sha256}': local_hash}

    def command(key):
        return [replacements.get(arg, arg) for arg in config[key]]

    run(command('uploadCommand'))
    verified = run(command('verifyOffhostCommand'))
    require(verified.split()[0] == local_hash, 'off-host checksum verification failed')
    return local_hash


def snapshot_images(config):
    root = Path(config['backupRoot'])
    require(root.is_absolute() and not root.is_symlink(), 'backup root must be absolute and real')
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    root.chmod(0o700)
    headroom(root, config.get('minimumFreeBytes', 30 * 1024**3))
    require(config.get('ageRecipients') and config.get('uploadCommand') and config.get('verifyOffhostCommand'), 'encryption and verified off-host upload required')
    require(set(config.get('candidateImages', {})) == {'server', 'dashboard'}
            and all(IMAGE_REF.fullmatch(ref) for ref in config['candidateImages'].values()), 'two exact candidate image references required')
    require(all(isinstance(ref, str) and IMAGE_REF.fullmatch(ref) for ref in config.get('rollbackImages', [])), 'rollback images must be digest-pinned')
    require(set(config['databases']) == {'primary'}, 'primary database required')
    database = database_source(config['databases']['primary'])
    references = set(config['candidateImages'].values()) | set(config.get('rollbackImages', []))
    if database['kind'] == 'container':
        info = inspect(database['name'])
        require(info['State']['Running'], 'backup database must be running')
        references.add(info['Image'])
    identifiers = set()
    for reference in references:
        info = inspect_image(reference)
        require(IMAGE_ID.fullmatch(info['Id']), 'invalid backup image')
        identifiers.add(info['Id'])
    directory = root / 'images'
    directory.mkdir(mode=0o700, exist_ok=True)
    directory.chmod(0o700)
    target = directory / image_snapshot_name(identifiers)
    require(not target.is_symlink(), 'image snapshot must be a regular file')
    with (root / '.backup.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if target.is_file():
            return {'schemaVersion': SCHEMA_VERSION, 'status': 'exists',
                    'imagesBundle': target.name, 'imageIds': sorted(identifiers)}
        with tempfile.TemporaryDirectory(prefix='.images-', dir=directory) as temporary:
            plain = Path(temporary) / 'images.tar'
            run(['docker', 'image', 'save', '--output', str(plain), *sorted(identifiers)])
            encrypt_file(config, plain, target)
            upload(config, target)
            headroom(root, config.get('minimumFreeBytes', 30 * 1024**3))
        return {'schemaVersion': SCHEMA_VERSION, 'status': 'created',
                'imagesBundle': target.name, 'imageIds': sorted(identifiers)}


def backup(config):
    root = Path(config['backupRoot'])
    require(root.is_absolute() and not root.is_symlink(), 'backup root must be absolute and real')
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    root.chmod(0o700)
    reserve = config.get('minimumFreeBytes', 30 * 1024**3)
    headroom(root, reserve)
    require(config.get('ageRecipients') and config.get('uploadCommand') and config.get('verifyOffhostCommand'), 'encryption and verified off-host upload required')
    require(type(config.get('keepLocal', 7)) is int and config.get('keepLocal', 7) >= 1, 'keepLocal must be positive')
    source = config.get('source', {})
    require(re.fullmatch('[a-f0-9]{40}', source.get('revision', '')) and re.fullmatch('[a-f0-9]{64}', source.get('sourceSha256', ''))
            and type(source.get('dirty')) is bool, 'deployed source identity required')
    require(set(config.get('candidateImages', {})) == {'server', 'dashboard'}
            and all(IMAGE_REF.fullmatch(ref) for ref in config['candidateImages'].values()), 'two exact candidate image references required')
    require(all(isinstance(ref, str) and IMAGE_REF.fullmatch(ref) for ref in config.get('rollbackImages', [])), 'rollback images must be digest-pinned')
    services = config['services']
    require(set(services) == {'server', 'dashboard'}, 'server/dashboard service identities required')
    require(set(config['databases']) == {'primary'}, 'primary database required')
    database = database_source(config['databases']['primary'])
    require(len(set(services.values())) == len(services), 'container identities must be distinct')
    if database['kind'] == 'container':
        require(len({*services.values(), database['name']}) == 3, 'container identities must be distinct')
    with (root / '.backup.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        started = now()
        infos = {role: inspect(name) for role, name in services.items()}
        for role, image in config['candidateImages'].items():
            require(inspect_image(image)['Id'] == infos[role]['Image'], 'running service differs from candidate image')
        key_root = mount(infos['server'], '/home/instant/config')
        backup_id = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-' + uuid.uuid4().hex[:8]
        encrypted = root / (backup_id + '.tar.age')
        with tempfile.TemporaryDirectory(prefix='.pending-', dir=root) as temporary:
            directory = Path(temporary)
            # pg_dump reads one MVCC snapshot, so the server keeps serving writes
            # throughout the cut and no writer journal is needed.
            manifest = {'schemaVersion': SCHEMA_VERSION, 'format': FORMAT, 'startedAt': started,
                        'consistency': 'mvcc-snapshot', 'databases': {}, 'images': [],
                        'services': {role: info['Image'] for role, info in infos.items()},
                        'source': config['source'], 'candidateImages': config['candidateImages']}
            selected = set()
            version = query(database, "SELECT current_setting('server_version_num')::int / 10000")
            require(version == '17', 'PostgreSQL 17 required')
            dump_database(database, directory / 'primary.dump')
            record = {'database': query(database, 'SELECT current_database()'),
                      'user': query(database, 'SELECT current_user'),
                      'postgresMajor': 17}
            if database['kind'] == 'container':
                info = inspect(database['name'])
                require(info['State']['Running'], 'backup database must be running')
                record['imageId'] = info['Image']
                selected.add(info['Image'])
            manifest['databases']['primary'] = record
            selected |= {info['Image'] for info in infos.values()} | set(config.get('rollbackImages', []))
            for reference in sorted(selected):
                info = inspect_image(reference)
                require(IMAGE_ID.fullmatch(info['Id']), 'invalid backup image')
                manifest['images'].append({'reference': reference, 'id': info['Id']})
            archive_directory(key_root, directory / 'server-secrets.tar')
            archive_directory(Path(config['configurationDirectory']), directory / 'configuration.tar')
            # snapshot-images saves the images once per deploy; record that
            # snapshot here instead of copying multi-gigabyte images into
            # every bundle.
            snapshot = image_snapshot_path(root, [image['id'] for image in manifest['images']])
            if snapshot.is_file() and not snapshot.is_symlink():
                manifest['imagesBundle'] = snapshot.name
            manifest['completedAt'] = now()
            (directory / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
            write_checksums(directory)
            verify_directory(directory)
            plain = directory / 'bundle.tar'
            with tarfile.open(plain, 'w') as archive:
                names = sorted((FILES | OPTIONAL_FILES | {'checksums.json'}) & {p.name for p in directory.iterdir()})
                for name in names:
                    archive.add(directory / name, arcname=name, recursive=False)
            encrypt_file(config, plain, encrypted)
            headroom(root, reserve)
        local_hash = digest(encrypted)
        upload(config, encrypted)
        receipt = {'schemaVersion': SCHEMA_VERSION, 'format': FORMAT, 'bundle': encrypted.name,
                   'sha256': local_hash, 'startedAt': started, 'completedAt': now(), 'offhostVerified': True}
        (root / (backup_id + '.receipt.json')).write_text(json.dumps(receipt, indent=2) + '\n')
        if config.get('metricsFile'):
            target = Path(config['metricsFile'])
            tmp = target.with_suffix('.tmp')
            tmp.write_text(f'backendrelay_backup_offhost_completed_seconds {time.time()}\n'
                           f'backendrelay_backup_snapshot_seconds {datetime.fromisoformat(started).timestamp()}\n')
            os.replace(tmp, target)
        # Retention is explicit; only verified artifacts generated by this tool qualify.
        keep = config.get('keepLocal', 7)
        require(type(keep) is int and keep >= 1, 'keepLocal must be positive')
        receipts = []
        for file in root.glob('*.receipt.json'):
            try:
                item = json.loads(file.read_text())
                if item.get('format') == FORMAT and item.get('offhostVerified') is True and Path(item['bundle']).name == item['bundle']:
                    receipts.append((item['completedAt'], file, item))
            except (ValueError, KeyError, OSError):
                continue
        for _, file, item in sorted(receipts, reverse=True)[keep:]:
            old = root / item['bundle']
            if not old.is_symlink() and old.is_file() and digest(old) == item['sha256']:
                old.unlink()
                file.unlink()
        return receipt


def validate_backup_age(manifest, allow_older_than_days=None):
    require(allow_older_than_days is None or (type(allow_older_than_days) is int and allow_older_than_days >= 1),
            '--allow-older-than must be a positive number of days')
    age = (datetime.now(timezone.utc) - datetime.fromisoformat(manifest['startedAt'])).total_seconds()
    require(age >= 0, 'off-host backup is future dated')
    limit = 86400 if allow_older_than_days is None else allow_older_than_days * 86400
    require(age <= limit, 'off-host backup is older than 24 hours or future dated'
            if allow_older_than_days is None else 'off-host backup is older than the allowed age')
    return age


def ping_healthcheck(config, ok):
    url = config.get('healthcheckUrl')
    if not url:
        return
    try:
        require(isinstance(url, str) and url.startswith(('http://', 'https://')), 'invalid healthcheck URL')
        target = url if ok else url.rstrip('/') + '/fail'
        urllib.request.urlopen(target, timeout=10).close()
    except Exception:
        # A monitoring ping never changes the backup outcome.
        print('healthcheck ping failed; backup result is unchanged', file=sys.stderr)


def restore_images(manifest, data, work, identity_file, image_snapshot):
    """Load the bundle's exact image IDs before any database or volume mutation."""
    required = manifest['images']
    snapshot_name = manifest.get('imagesBundle')
    if image_snapshot is not None:
        if snapshot_name is not None:
            require(image_snapshot.name == snapshot_name,
                    f'--images must be the bundle image snapshot {snapshot_name}')
        plain = work / 'images.tar'
        run(['age', '--decrypt', '--identity', str(identity_file), '--output', str(plain), str(image_snapshot)])
        run(['docker', 'image', 'load', '--input', str(plain)])
        verify_loaded_images(required)
        return
    legacy = data / 'images.tar'
    if legacy.is_file():
        run(['docker', 'image', 'load', '--input', str(legacy)])
        verify_loaded_images(required)
        return
    missing = missing_images(required)
    message = 'bundle has no image snapshot; '
    message += (f'pass --images with the snapshot {snapshot_name}, ' if snapshot_name is not None
                else 'pass --images with the matching snapshot, ')
    message += 'or load these Docker image IDs locally: ' + ', '.join(missing)
    require(not missing, message)
    verify_loaded_images(required)


def restore(bundle, identity_file, target, allow_older_than_days=None, image_snapshot=None):
    started = time.monotonic()
    require(allow_older_than_days is None or (type(allow_older_than_days) is int and allow_older_than_days >= 1),
            '--allow-older-than must be a positive number of days')
    require(target.get('disposable') is True, 'restore target must be explicitly disposable')
    require(target.get('startCommand') and target.get('verifyCommand'), 'explicit service start and complete verification commands required')
    require(set(target['databases']) == {'primary'}, 'primary restore database target required')
    require(set(target['directories']) == {'server-secrets', 'configuration'}, 'all restore directories required')
    root = Path(target['scratchDirectory'])
    require(root.is_absolute() and root.is_dir() and not root.is_symlink(), 'absolute restore scratch directory required')
    headroom(root, target.get('minimumFreeBytes', 30 * 1024**3))
    require(digest(bundle) == target['bundleSha256'], 'encrypted backup checksum mismatch')
    with tempfile.TemporaryDirectory(prefix='restore-', dir=root) as temporary:
        work = Path(temporary)
        plain = work / 'bundle.tar'
        run(['age', '--decrypt', '--identity', str(identity_file), '--output', str(plain), str(bundle)])
        data = work / 'data'
        data.mkdir(mode=0o700)
        extract_archive(plain, data, target.get('maximumArchiveBytes', 1024**4))
        manifest = verify_directory(data)
        age = validate_backup_age(manifest, allow_older_than_days)
        require(set(target.get('composeServices', {})) == {'server', 'dashboard', 'primary'}, 'complete compose service mapping required')
        require(len(set(target['composeServices'].values())) == 3 and all(re.fullmatch('[a-zA-Z0-9][a-zA-Z0-9_.-]*', name) for name in target['composeServices'].values()), 'distinct compose service names required')
        config_dir = Path(target['directories']['configuration']['path'])
        override_path = config_dir / 'backendrelay-restored-images.yml'
        require(str(override_path) in target['startCommand'], 'startCommand must include the generated exact-image override')
        require(not override_path.exists(), 'restore image override already exists')
        paths = [Path(location['path']).resolve() for location in target['directories'].values()]
        require(all(a != b and not a.is_relative_to(b) and not b.is_relative_to(a) for i, a in enumerate(paths) for b in paths[i+1:]), 'restore directories must be distinct and non-overlapping')
        # Preflight every destination before writing any restored state.
        for name in target['databases'].values():
            info = inspect(name)
            require(info['Config'].get('Labels', {}).get('backendrelay.restore.disposable') == 'true', 'database lacks disposable restore label')
            require(query(name, "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'") == '0', 'restore database is not empty')
            require(query(name, "SELECT current_setting('server_version_num')::int / 10000") == '17', 'restore database must use PostgreSQL 17')
        for location in target['directories'].values():
            directory = Path(location['path'])
            require(all(type(location.get(key)) is int and location[key] >= 0 for key in ('uid', 'gid')), 'explicit restore uid/gid required')
            require(directory.is_absolute() and directory.is_dir() and not directory.is_symlink() and not any(directory.iterdir()), 'restore directories must be empty absolute paths')
        restore_images(manifest, data, work, identity_file, image_snapshot)
        for role, name in target['databases'].items():
            with (data / (role + '.dump')).open('rb') as source:
                run(['docker', 'exec', '-i', name, 'sh', '-ec', 'exec pg_restore --exit-on-error --no-owner --no-acl -U "$POSTGRES_USER" -d "$POSTGRES_DB"'], stdin=source)
        for role, location in target['directories'].items():
            extract_archive(data / (role + '.tar'), Path(location['path']), target.get('maximumArchiveBytes', 1024**4), (location['uid'], location['gid']))
        image_override = {'services': {}}
        for role, service in target['composeServices'].items():
            require(role in {'server', 'dashboard', 'primary'}, 'unknown restore service role')
            if role in manifest['services']:
                image_id = manifest['services'][role]
            elif 'imageId' in manifest['databases'].get(role, {}):
                image_id = manifest['databases'][role]['imageId']
            else:
                continue
            image_override['services'][service] = {'image': image_id, 'pull_policy': 'never'}
        # JSON is valid YAML. Explicit loaded IDs avoid relying on RepoDigests
        # surviving docker save/load; startCommand must include this override.
        override_path.write_text(json.dumps(image_override, indent=2) + '\n')
        # Database records and encryption keys are restored without recreating app identities.
        run(target['startCommand'])
        run(target['verifyCommand'])
        elapsed = time.monotonic() - started
        require(elapsed <= 3600, 'complete-service restore exceeded 60 minutes')
        return {'schemaVersion': SCHEMA_VERSION, 'status': 'passed', 'bundleSha256': digest(bundle),
                'restoreSeconds': elapsed, 'backupAgeSeconds': age, 'completedAt': now(),
                'restoredSource': manifest['source'], 'restoredImages': manifest['candidateImages'],
                'scope': 'Operator verification command passed; review full database and service receipts before release.'}


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='mode', required=True)
    create = sub.add_parser('backup')
    create.add_argument('--config', type=Path, required=True)
    snapshot = sub.add_parser('snapshot-images')
    snapshot.add_argument('--config', type=Path, required=True)
    recover = sub.add_parser('restore')
    recover.add_argument('--bundle', type=Path, required=True)
    recover.add_argument('--identity-file', type=Path, required=True)
    recover.add_argument('--target-config', type=Path, required=True)
    recover.add_argument('--images', type=Path,
                         help='encrypted image snapshot from snapshot-images (defaults to a legacy in-bundle images.tar)')
    recover.add_argument('--allow-older-than', type=int, metavar='DAYS',
                         help='accept a bundle older than the default 24-hour drill guard')
    resume = sub.add_parser('resume-writers', help='restart writers stopped by an older backup tool version')
    resume.add_argument('--config', type=Path, required=True)
    args = parser.parse_args()
    allow_older_than = getattr(args, 'allow_older_than', None)
    if allow_older_than is not None and allow_older_than < 1:
        parser.error('--allow-older-than must be a positive number of days')
    try:
        if args.mode == 'resume-writers':
            config = json.loads(args.config.read_text())
            root = Path(config['backupRoot'])
            with (root / '.backup.lock').open('a') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                resume_journal(root, config['services'])
            result = {'status': 'resumed', 'scope': 'original backup writer identities only'}
        elif args.mode == 'backup':
            config = json.loads(args.config.read_text())
            try:
                result = backup(config)
            except Exception:
                ping_healthcheck(config, False)
                raise
            ping_healthcheck(config, True)
        elif args.mode == 'snapshot-images':
            result = snapshot_images(json.loads(args.config.read_text()))
        else:
            if args.allow_older_than is not None:
                print(f'warning: accepting backups up to {args.allow_older_than} days old', file=sys.stderr)
            result = restore(args.bundle, args.identity_file, json.loads(args.target_config.read_text()),
                             args.allow_older_than, args.images)
        print(json.dumps(result))
    except (OSError, ValueError, KeyError, TypeError, IndexError, subprocess.SubprocessError):
        parser.exit(1, 'Recovery command failed; inspect configuration, artifact integrity, disk headroom and service state. No successful receipt recorded.\n')


if __name__ == '__main__':
    main()
