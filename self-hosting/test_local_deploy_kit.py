from pathlib import Path
import ipaddress
import re
import subprocess
import unittest

import yaml

ROOT = Path(__file__).parent
DEPLOY = ROOT / 'deploy'


def load(name):
    return yaml.safe_load((DEPLOY / name).read_text())


class DeployComposeTests(unittest.TestCase):
    def test_required_services_and_digest_only_images(self):
        doc = load('docker-compose.yml')
        services = doc['services']
        for name in ('postgres', 'aux-postgres', 'server', 'dashboard', 'caddy', 'smoke'):
            self.assertIn(name, services, name)
        for name, service in services.items():
            image = service['image']
            self.assertIn(':?', image, name)
            self.assertTrue(image.startswith('${') and image.endswith('}'), name)
        self.assertEqual(services['smoke']['profiles'], ['smoke'])
        self.assertIn('8080', services['dashboard']['expose'])
        self.assertEqual(services['dashboard']['environment']['BR_API_URL'],
                         '${INSTANT_BACKEND_URL:?set INSTANT_BACKEND_URL}')
        self.assertEqual(services['dashboard']['image'],
                         '${INSTANT_DASHBOARD_IMAGE:?pass the signed dashboard digest to deploy.sh}')

    def test_hardening_and_log_limits_cover_every_service(self):
        services = load('docker-compose.yml')['services']
        for name, service in services.items():
            logging = service.get('logging', {})
            self.assertEqual(logging.get('driver'), 'local', name)
            self.assertEqual(logging.get('options', {}).get('max-size'), '50m', name)
            self.assertEqual(logging.get('options', {}).get('max-file'), '5', name)
            self.assertIn('no-new-privileges:true', service.get('security_opt', []), name)
            self.assertIn('pids_limit', service, name)
            if name != 'caddy':
                self.assertNotIn('ports', service, name)

        for name in ('server', 'dashboard', 'caddy', 'smoke'):
            self.assertEqual(services[name]['cap_drop'], ['ALL'], name)
            self.assertTrue(services[name]['read_only'], name)
        self.assertEqual(services['caddy']['cap_add'], ['NET_BIND_SERVICE'])
        self.assertEqual(services['server']['stop_grace_period'], '45s')
        self.assertIn('/tmp', services['server']['tmpfs'])
        self.assertEqual(services['server']['healthcheck']['test'],
                         ['CMD', '/usr/local/bin/instant-server', 'healthcheck'])
        self.assertEqual(services['dashboard']['healthcheck']['test'],
                         ['CMD', 'wget', '-qO-', 'http://127.0.0.1:8080/healthz'])

    def test_fixed_subnet_and_trusted_proxy_match(self):
        doc = load('docker-compose.yml')
        network = doc['networks']['default']
        self.assertEqual(network['name'], '${DEPLOY_NETWORK_NAME:-backendrelay}')
        self.assertEqual(network['ipam']['config'],
                         [{'subnet': '${DEPLOY_SUBNET:?set the deploy network subnet, '
                                     'for example 172.30.0.0/24}'}])
        server = doc['services']['server']
        self.assertEqual(server['environment']['INSTANT_TRUSTED_PROXY_CIDRS'],
                         '${DEPLOY_SUBNET:?set the deploy network subnet, '
                         'for example 172.30.0.0/24}')
        for name, address in (('caddy', '${CADDY_IP:-172.30.0.2}'),
                              ('server', '${SERVER_IP:-172.30.0.3}'),
                              ('dashboard', '${DASHBOARD_IP:-172.30.0.4}'),
                              ('aux-postgres', '${AUX_POSTGRES_IP:-172.30.0.5}'),
                              ('postgres', '${POSTGRES_IP:-172.30.0.6}')):
            self.assertEqual(doc['services'][name]['networks']['default']['ipv4_address'], address)

    def test_staging_database_never_mounts_the_production_volume(self):
        services = load('docker-compose.yml')['services']
        self.assertIn('backend-db:/var/lib/postgresql/data', services['postgres']['volumes'])
        self.assertIn('aux-db:/var/lib/postgresql/data', services['aux-postgres']['volumes'])
        self.assertNotIn('backend-db:/var/lib/postgresql/data',
                         services['aux-postgres']['volumes'])

    def test_dry_run_overlay_keeps_the_stack_local(self):
        doc = load('docker-compose.dry-run.yml')
        self.assertIn('minio', doc['services'])
        self.assertIn('minio-init', doc['services'])
        caddy_volumes = doc['services']['caddy']['volumes']
        self.assertIn('./deploy/Caddyfile.local:/etc/caddy/Caddyfile:ro', caddy_volumes)
        self.assertEqual(doc['services']['caddy']['volumes'].count(
            './deploy/Caddyfile.local:/etc/caddy/Caddyfile:ro'), 1)
        self.assertEqual(doc['volumes'], {'minio-data': None})


class DeployCaddyTests(unittest.TestCase):
    def test_production_edges_use_cloudflare_trust_and_client_ip(self):
        for name in ('Caddyfile', 'Caddyfile.letsencrypt-dns', 'Caddyfile.local'):
            text = (DEPLOY / name).read_text()
            self.assertIn('timeouts {', text, name)
            self.assertIn('read_header 10s', text, name)
            self.assertIn('idle 2m', text, name)
            self.assertIn('health_uri /health/system', text, name)
            self.assertIn('flush_interval -1', text, name)
            self.assertIn('header_up X-Forwarded-For {client_ip}', text, name)
            self.assertIn('reverse_proxy {$SERVER_UPSTREAMS}', text, name)
            self.assertIn('reverse_proxy dashboard:8080', text, name)
            self.assertIn('request_body', text, name)

        for name in ('Caddyfile', 'Caddyfile.letsencrypt-dns'):
            text = (DEPLOY / name).read_text()
            self.assertIn('trusted_proxies static', text, name)
            self.assertIn('client_ip_headers CF-Connecting-IP', text, name)
            self.assertIn('tls ', text, name)

        self.assertIn('http://{$BACKEND_DOMAIN:api.localhost}', (DEPLOY / 'Caddyfile.local').read_text())
        self.assertNotIn('tls ', (DEPLOY / 'Caddyfile.local').read_text())

    def test_body_limits_exceed_the_server_caps(self):
        text = (DEPLOY / 'Caddyfile').read_text()
        restore = int(re.search(r'CADDY_MAX_RESTORE_BODY:(\d+)MB', text).group(1))
        upload = int(re.search(r'CADDY_MAX_UPLOAD_BODY:(\d+)MB', text).group(1))
        management = int(re.search(r'CADDY_MAX_MANAGEMENT_BODY:(\d+)MB', text).group(1))
        self.assertGreater(restore * 1_000_000, 1024 * 1024 * 1024)
        self.assertGreater(upload * 1_000_000, 100 * 1024 * 1024)
        self.assertGreaterEqual(management * 1_000_000, 1024 * 1024)
        self.assertIn('not path /dash/restores/zip', text)


class DeployScriptTests(unittest.TestCase):
    def test_shell_and_node_syntax(self):
        for name in ('deploy.sh', 'rollback.sh', 'lib.sh', 'staging-backup.sh'):
            subprocess.run(['bash', '-n', str(DEPLOY / name)], check=True)
        subprocess.run(['node', '--check', str(DEPLOY / 'smoke.mjs')], check=True)

    def test_scripts_declare_their_interpreters(self):
        self.assertTrue((DEPLOY / 'deploy.sh').read_text().startswith('#!/usr/bin/env bash\n'))
        self.assertTrue((DEPLOY / 'rollback.sh').read_text().startswith('#!/usr/bin/env bash\n'))
        self.assertTrue((DEPLOY / 'staging-backup.sh').read_text().startswith('#!/usr/bin/env bash\n'))
        self.assertTrue((DEPLOY / 'smoke.mjs').read_text().startswith('#!/usr/bin/env node\n'))

    def test_deploy_order_verification_backup_migrate_health_smoke_audit(self):
        text = (DEPLOY / 'deploy.sh').read_text()
        anchors = [
            'verify-release-images.sh',
            'recovery.py" backup --config',
            'server migrate up',
            'force-recreate "$service"',
            'force-recreate dashboard',
            '/health/system',
            'instant-server audit',
            'write_state "$current_state"',
        ]
        positions = [text.index(anchor) for anchor in anchors]
        self.assertEqual(positions, sorted(positions))
        self.assertIn('trap on_error ERR', text)
        self.assertIn('rollback.sh', text)
        self.assertIn('mutated=1', text)
        self.assertLess(text.index('mutated=1'), text.index('server migrate up'))
        self.assertIn('the pre-deploy backup failed; aborting', text)

    def test_rollback_uses_previous_state_and_the_additive_floor(self):
        deploy = (DEPLOY / 'deploy.sh').read_text()
        rollback = (DEPLOY / 'rollback.sh').read_text()
        lib = (DEPLOY / 'lib.sh').read_text()
        self.assertIn('previous.env', rollback)
        self.assertIn('previous.env', deploy)
        self.assertIn('migrate down', lib)
        self.assertIn('database\\ schema\\ version', lib)
        self.assertIn('ROLLBACK_MIGRATE_DOWN', rollback)
        self.assertIn('current.env', rollback)

    def test_client_ip_smoke_rejects_private_and_edge_addresses(self):
        lib = (DEPLOY / 'lib.sh').read_text()
        self.assertIn('client_ip', lib)
        self.assertIn('remote_ip', lib)
        self.assertIn('is_global', lib)
        self.assertIn('per-IP magic-code quota is inactive', lib)
        self.assertIn('client_ip_headers', (DEPLOY / 'Caddyfile').read_text())

    def test_smoke_query_matches_the_runtime_protocol(self):
        text = (DEPLOY / 'smoke.mjs').read_text()
        self.assertIn("op: 'init'", text)
        self.assertIn("__admin-token", text)
        self.assertIn("op: 'add-query'", text)
        self.assertIn("'add-query-ok'", text)
        self.assertIn('/runtime/session', text)
        self.assertNotIn('console.log(adminToken', text)


class DeployEnvAndUnitsTests(unittest.TestCase):
    def test_env_example_lists_every_compose_variable(self):
        compose_keys = set()
        for name in ('docker-compose.yml', 'docker-compose.dry-run.yml'):
            text = (DEPLOY / name).read_text()
            compose_keys |= set(re.findall(r'\$\{([A-Z_][A-Z0-9_]*)', text))
        env_text = (DEPLOY / '.env.example').read_text()
        configured = set(re.findall(r'^([A-Z_][A-Z0-9_]*)=', env_text, re.MULTILINE))
        self.assertEqual(sorted(compose_keys - configured), [])
        self.assertIn('DEPLOY_SUBNET=', env_text)
        self.assertIn('SERVER_UPSTREAMS=', env_text)
        self.assertIn('DEPLOY_EXPECT_CLOUDFLARE=', env_text)

    def test_env_example_contains_no_filled_secrets(self):
        values = dict(re.findall(r'^([A-Z_][A-Z0-9_]*)=(.*)$', (DEPLOY / '.env.example').read_text(),
                                 re.MULTILINE))
        for name in ('POSTGRES_PASSWORD', 'INSTANT_DB_PASSWORD', 'AUX_POSTGRES_PASSWORD',
                     'AUX_DB_PASSWORD', 'S3_SECRET_ACCESS_KEY', 'POSTMARK_TOKEN',
                     'SENDGRID_TOKEN', 'RESEND_TOKEN', 'CLOUDFLARE_EMAIL_API_TOKEN',
                     'INSTANT_DASHBOARD_GOOGLE_OAUTH_CLIENT_SECRET'):
            self.assertEqual(values.get(name, ''), '', name)

    def test_env_file_is_git_ignored(self):
        text = (ROOT.parent / '.gitignore').read_text()
        self.assertIn('self-hosting/deploy/.env', text)

    def test_systemd_units_cover_backup_canary_and_weekly_audit(self):
        backup = (DEPLOY / 'systemd/backendrelay-deploy-backup.service').read_text()
        backup_timer = (DEPLOY / 'systemd/backendrelay-deploy-backup.timer').read_text()
        audit = (DEPLOY / 'systemd/backendrelay-deploy-audit.service').read_text()
        audit_timer = (DEPLOY / 'systemd/backendrelay-deploy-audit.timer').read_text()
        canary = (DEPLOY / 'systemd/backendrelay-deploy-canary.service').read_text()
        self.assertIn('recovery.py backup --config /etc/backendrelay/backup.json', backup)
        self.assertIn('Unit=backendrelay-deploy-backup.service', backup_timer)
        self.assertIn('instant-server audit', audit)
        self.assertIn('Unit=backendrelay-deploy-audit.service', audit_timer)
        self.assertIn('OnCalendar=Mon', audit_timer)
        self.assertIn('--profile canary up -d --no-deps canary', canary)
        self.assertIn('docker-compose.monitoring.yml', canary)

    def test_units_do_not_collide_with_the_monitoring_schedule(self):
        deploy_units = {path.name for path in (DEPLOY / 'systemd').iterdir()}
        monitoring_units = {path.name for path in (ROOT / 'monitoring').iterdir()
                           if path.suffix in ('.service', '.timer')}
        self.assertEqual(deploy_units & monitoring_units, set())


class DeployStagingTests(unittest.TestCase):
    """P7.5: the staging project shares the compose file but nothing else."""

    def staging_values(self):
        return dict(re.findall(r'^([A-Z_][A-Z0-9_]*)=(.*)$',
                               (DEPLOY / '.env.staging.example').read_text(), re.MULTILINE))

    def production_values(self):
        return dict(re.findall(r'^([A-Z_][A-Z0-9_]*)=(.*)$',
                               (DEPLOY / '.env.example').read_text(), re.MULTILINE))

    def test_staging_env_example_fills_every_required_compose_variable(self):
        required = set(re.findall(r'\$\{([A-Z_][A-Z0-9_]*):\?',
                                  (DEPLOY / 'docker-compose.yml').read_text()))
        self.assertEqual(sorted(required - set(self.staging_values())), [])

    def test_staging_env_file_is_git_ignored_and_the_example_is_not(self):
        text = (ROOT.parent / '.gitignore').read_text()
        self.assertIn('self-hosting/deploy/.env.staging\n', text)
        self.assertNotIn('self-hosting/deploy/.env.staging.example', text)

    def test_staging_project_differs_from_production_where_it_must(self):
        staging, production = self.staging_values(), self.production_values()
        for key in ('DEPLOY_COMPOSE_PROJECT', 'DEPLOY_NETWORK_NAME', 'DEPLOY_SUBNET',
                    'DEPLOY_STATE_DIR', 'CADDY_HTTP_PORT', 'CADDY_HTTPS_PORT',
                    'BACKEND_DOMAIN', 'DASHBOARD_DOMAIN',
                    'S3_FILES_BUCKET', 'S3_BACKUPS_BUCKET'):
            self.assertNotEqual(staging[key], production[key], key)
        self.assertEqual(staging['DEPLOY_STACK'], 'staging')
        self.assertEqual(staging['DEPLOY_DB_SERVICE'], 'aux-postgres')
        self.assertEqual(production['DEPLOY_STACK'], 'production')
        self.assertEqual(production['DEPLOY_DB_SERVICE'], 'postgres')
        self.assertEqual(staging['DEPLOY_MONITORING'], '0')
        self.assertEqual(staging['CADDY_HTTPS_PORT'], '8443')
        self.assertIn('staging-api.', staging['BACKEND_DOMAIN'])
        self.assertIn('staging-dash.', staging['DASHBOARD_DOMAIN'])
        self.assertIn('staging', staging['S3_FILES_BUCKET'])
        self.assertIn('staging', staging['S3_BACKUPS_BUCKET'])

    def test_staging_addresses_sit_inside_its_own_subnet(self):
        staging = self.staging_values()
        subnet = ipaddress.ip_network(staging['DEPLOY_SUBNET'])
        self.assertNotEqual(str(subnet), self.production_values()['DEPLOY_SUBNET'])
        addresses = [staging[key] for key in
                     ('CADDY_IP', 'SERVER_IP', 'DASHBOARD_IP', 'AUX_POSTGRES_IP', 'POSTGRES_IP')]
        self.assertEqual(len(addresses), len(set(addresses)))
        for key, address in zip(('CADDY_IP', 'SERVER_IP', 'DASHBOARD_IP',
                                 'AUX_POSTGRES_IP', 'POSTGRES_IP'), addresses):
            self.assertIn(ipaddress.ip_address(address), subnet, key)

    def test_staging_database_credentials_match_the_staging_role(self):
        staging = self.staging_values()
        self.assertEqual(staging['POSTGRES_DB'], staging['AUX_POSTGRES_DB'])
        self.assertEqual(staging['INSTANT_DB_USER'], staging['AUX_DB_USER'])
        for name in ('POSTGRES_PASSWORD', 'INSTANT_DB_PASSWORD',
                     'AUX_POSTGRES_PASSWORD', 'AUX_DB_PASSWORD',
                     'S3_ACCESS_KEY_ID', 'S3_SECRET_ACCESS_KEY',
                     'CLOUDFLARE_EMAIL_API_TOKEN'):
            self.assertEqual(staging.get(name, ''), '', name)

    def test_one_compose_file_serves_both_projects(self):
        services = load('docker-compose.yml')['services']
        self.assertEqual(services['aux-postgres']['profiles'], ['staging'])
        # Production keeps its documented manual commands: postgres has no
        # profile, so no extra flag is needed during an incident.
        self.assertNotIn('profiles', services['postgres'])
        self.assertIn('@${DEPLOY_DB_SERVICE:-postgres}:5432/',
                      services['server']['environment']['DATABASE_URL'])
        self.assertEqual(services['caddy']['ports'],
                         ['${CADDY_HTTP_PORT:-80}:80', '${CADDY_HTTPS_PORT:-443}:443'])

    def test_deploy_and_rollback_resolve_the_stack(self):
        for name in ('deploy.sh', 'rollback.sh'):
            text = (DEPLOY / name).read_text()
            self.assertIn('configure_stack "$project" "$state_dir"', text)
            self.assertIn('"${DEPLOY_COMPOSE_PROFILE_FLAGS[@]}"', text)
        deploy = (DEPLOY / 'deploy.sh').read_text()
        self.assertIn('up -d "$DEPLOY_DB_SERVICE"', deploy)
        self.assertNotIn('up -d postgres', deploy)
        self.assertIn('DEPLOY_STACK="$DEPLOY_STACK_NAME"', deploy)
        lib = (DEPLOY / 'lib.sh').read_text()
        self.assertIn('DEPLOY_COMPOSE_PROFILE_FLAGS=(--profile staging)', lib)
        self.assertIn('DEPLOY_DB_SERVICE=aux-postgres', lib)
        self.assertIn('export DEPLOY_DB_SERVICE', lib)
        self.assertIn('backendrelay is the production project', lib)
        self.assertIn('backendrelay is the production network', lib)
        self.assertIn('holds the production rollback state', lib)

    def test_staging_backup_only_dumps_the_staging_database(self):
        text = (DEPLOY / 'staging-backup.sh').read_text()
        self.assertIn('DEPLOY_STACK_NAME" = staging', text)
        self.assertIn('pg_dumpall', text)
        self.assertIn('"$DEPLOY_DB_SERVICE"', text)
        self.assertIn('.env.staging', text)
        self.assertNotIn('recovery.py', text)
        self.assertIn('STAGING_BACKUP_DIR', text)
        self.assertIn('staging-backup.sh', self.staging_values()['DEPLOY_BACKUP_COMMAND'])


if __name__ == '__main__':
    unittest.main()
