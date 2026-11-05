import json
from pathlib import Path
import re
import subprocess
import unittest
import yaml
import importlib.util

ROOT = Path(__file__).parent
spec = importlib.util.spec_from_file_location('slot_queries', ROOT / 'monitoring/slot_queries.py')
slots = importlib.util.module_from_spec(spec)
spec.loader.exec_module(slots)


class MonitoringTests(unittest.TestCase):
    def test_slot_render_rejects_invalid_expected_replica_counts(self):
        for value in (0, -1, 1001, True, 1.5, None, '1'):
            with self.assertRaises(ValueError):
                slots.render(value)

    def test_slot_queries_match_slot_names_by_prefix(self):
        default = yaml.safe_load((ROOT / 'monitoring/postgres-queries.yml').read_text())
        rendered = slots.render(3)
        for doc in (default, rendered):
            detail = doc['backendrelay_slot']['query']
            self.assertIn("LIKE 'invalidator\\_%' ESCAPE '\\'", detail)
            self.assertIn("slot_name = 'aggregator'", detail)
            labels = [name for row in doc['backendrelay_slot']['metrics']
                      for name, value in row.items() if value['usage'] == 'LABEL']
            self.assertEqual(labels, ['slot', 'role'])
            gauges = [name for row in doc['backendrelay_slot']['metrics']
                      for name, value in row.items() if value['usage'] == 'GAUGE']
            for gauge in ('retained_bytes', 'wal_status_reserved', 'wal_status_extended',
                          'wal_status_unreserved', 'wal_status_lost'):
                self.assertIn(gauge, gauges)
            self.assertIn("LIKE 'invalidator\\_%' ESCAPE '\\'",
                          doc['backendrelay_invalidator_slots']['query'])
            self.assertIn("slot_name = 'aggregator'", doc['backendrelay_aggregator_slot']['query'])
            self.assertNotIn('backendrelay_slots', doc)
        self.assertEqual(default['backendrelay_slot']['query'], rendered['backendrelay_slot']['query'])
        self.assertEqual(default['backendrelay_slot_expected']['query'], 'SELECT 1::float8 AS replicas')
        self.assertEqual(rendered['backendrelay_slot_expected']['query'], 'SELECT 3::float8 AS replicas')

    def test_slot_alerts_follow_names_by_prefix(self):
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}
        self.assertNotIn('SlotMonitoringUnconfigured', rules)
        missing = rules['InvalidatorSlotsMissing']
        self.assertEqual(missing['expr'],
                         'backendrelay_invalidator_slots_active < backendrelay_slot_expected_replicas')
        self.assertEqual(missing['for'], '2m')
        self.assertEqual(missing['labels']['severity'], 'critical')
        self.assertEqual(rules['ReplicationSlotInactive']['expr'], 'backendrelay_slot_active == 0')
        self.assertEqual(rules['ReplicationSlotInactive']['for'], '10m')
        self.assertEqual(rules['ReplicationSlotUnhealthy']['expr'],
                         'backendrelay_slot_wal_status_reserved == 0')
        self.assertEqual(rules['WalRetentionHigh']['expr'],
                         'backendrelay_wal_retained_bytes > 1073741824')
        self.assertEqual(rules['AggregatorSlotMissing']['expr'],
                         'backendrelay_aggregator_slot_present == 0')

    def test_every_alert_has_a_runbook_playbook(self):
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        names = [rule['alert'] for group in alerts['groups'] for rule in group['rules']]
        operations = (ROOT.parent / 'docs/operations.md').read_text()
        headings = set(re.findall(r'^### (\S+)\s*$', operations, re.MULTILINE))
        self.assertEqual(sorted(set(names) - headings), [],
                         'alerts without a ### playbook heading in docs/operations.md')

    def test_monitoring_has_bounded_storage_resources_and_private_ports(self):
        doc = yaml.safe_load((ROOT / 'docker-compose.monitoring.yml').read_text())
        services = doc['services']
        self.assertEqual(services['server']['environment']['INSTANT_METRICS_ADDR'],
                         '0.0.0.0:9091')
        self.assertEqual(
            services['server']['environment']['INSTANT_METRICS_ALLOW_NONLOOPBACK'], 'true')
        self.assertNotIn('network_mode', services['prometheus'])
        self.assertIn('--storage.tsdb.retention.time=15d', services['prometheus']['command'])
        self.assertIn('--storage.tsdb.retention.size=5GB', services['prometheus']['command'])
        for name, service in services.items():
            if name == 'server':
                continue
            self.assertIn('mem_limit', service)
            self.assertIn('cpus', service)
            self.assertIn('pids_limit', service)
            self.assertNotIn('labels', service)
            for port in service.get('ports', []):
                self.assertTrue(port.startswith('127.0.0.1:'), name)

    def test_every_compose_service_bounds_container_logs(self):
        for name in ('docker-compose.yml', 'docker-compose.with-caddy.yml',
                     'docker-compose.local.yml', 'docker-compose.monitoring.yml'):
            doc = yaml.safe_load((ROOT / name).read_text())
            for service_name, service in doc['services'].items():
                logging = service.get('logging', {})
                self.assertEqual(logging.get('driver'), 'local', f'{name}: {service_name}')
                self.assertEqual(logging.get('options', {}).get('max-size'), '50m',
                                 f'{name}: {service_name}')
                self.assertEqual(logging.get('options', {}).get('max-file'), '5',
                                 f'{name}: {service_name}')

    def test_loki_receives_container_logs_through_a_socket_proxy(self):
        doc = yaml.safe_load((ROOT / 'docker-compose.monitoring.yml').read_text())
        services = doc['services']
        for name in ('prometheus', 'alertmanager', 'grafana', 'node-exporter',
                     'postgres-exporter', 'loki', 'alloy', 'docker-socket-proxy'):
            logging = services[name].get('logging', {})
            self.assertEqual(logging.get('driver'), 'local', name)
            self.assertEqual(logging.get('options', {}).get('max-size'), '50m', name)
            self.assertEqual(logging.get('options', {}).get('max-file'), '5', name)

        loki = services['loki']
        self.assertTrue(loki['image'].startswith('${LOKI_IMAGE:?'))
        self.assertNotIn('ports', loki)
        self.assertIn('loki_data:/loki', loki['volumes'])
        loki_config = yaml.safe_load((ROOT / 'monitoring/loki.yml').read_text())
        self.assertEqual(loki_config['limits_config']['retention_period'], '336h')
        self.assertTrue(loki_config['compactor']['retention_enabled'])
        self.assertEqual(loki_config['compactor']['delete_request_store'], 'filesystem')
        self.assertEqual(
            loki_config['common']['storage']['filesystem']['chunks_directory'],
            '/loki/chunks')

        proxy = services['docker-socket-proxy']
        self.assertTrue(proxy['image'].startswith('${DOCKER_SOCKET_PROXY_IMAGE:?'))
        self.assertEqual(proxy['environment']['CONTAINERS'], '1')
        for key, value in proxy['environment'].items():
            if key != 'CONTAINERS':
                self.assertEqual(value, '0', key)
        self.assertEqual(proxy['volumes'],
                         ['/var/run/docker.sock:/var/run/docker.sock:ro'])
        self.assertNotIn('ports', proxy)
        self.assertEqual(proxy['networks'], ['logs'])
        self.assertTrue(doc['networks']['logs']['internal'])

        alloy = services['alloy']
        self.assertTrue(alloy['image'].startswith('${ALLOY_IMAGE:?'))
        alloy_config = (ROOT / 'monitoring/alloy.alloy').read_text()
        self.assertIn('tcp://docker-socket-proxy:2375', alloy_config)
        self.assertIn('loki.source.docker', alloy_config)
        self.assertIn('http://loki:3100/loki/api/v1/push', alloy_config)
        for name, service in services.items():
            if name == 'docker-socket-proxy':
                continue
            self.assertNotIn('/var/run/docker.sock:/var/run/docker.sock', ' '.join(
                service.get('volumes', [])), 'only the socket proxy may mount the Docker socket')

        datasource = yaml.safe_load(
            (ROOT / 'monitoring/grafana/provisioning/datasources/loki.yml').read_text())
        loki_source = datasource['datasources'][0]
        self.assertEqual(loki_source['type'], 'loki')
        self.assertEqual(loki_source['uid'], 'backendrelay-loki')
        self.assertEqual(loki_source['url'], 'http://loki:3100')
        self.assertIn('default', services['grafana']['networks'])
        self.assertIn('logs', services['grafana']['networks'])

    def test_canary_is_wired_to_compose_prometheus_and_alerts(self):
        compose = yaml.safe_load((ROOT / 'docker-compose.monitoring.yml').read_text())
        canary = compose['services']['canary']
        self.assertEqual(canary['profiles'], ['canary'])
        self.assertTrue(canary['image'].startswith('${CANARY_IMAGE:-backendrelay/canary:local}'))
        self.assertEqual(canary['build']['context'], './canary')
        self.assertTrue(canary['read_only'])
        self.assertEqual(canary['environment']['CANARY_ADMIN_TOKEN_FILE'],
                         '/run/secrets/canary-admin-token')
        self.assertTrue(canary['environment']['CANARY_API_URL'].startswith(
            '${CANARY_API_URL:-}'))
        self.assertTrue(canary['environment']['CANARY_APP_ID'].startswith(
            '${CANARY_APP_ID:-}'))
        self.assertEqual(canary['environment']['CANARY_HEALTHCHECK_URL'],
                         '${CANARY_HEALTHCHECK_URL:-}')
        self.assertEqual(canary['environment']['CANARY_INTERVAL_SECONDS'],
                         '${CANARY_INTERVAL_SECONDS:-60}')
        volumes = '\n'.join(canary['volumes'])
        self.assertIn('${CANARY_ADMIN_TOKEN_FILE:-/dev/null}', volumes)
        self.assertIn('/run/secrets/canary-admin-token:ro', volumes)
        self.assertNotIn('ports', canary)
        self.assertNotIn('CANARY_ADMIN_TOKEN', canary['environment'])

        config = yaml.safe_load((ROOT / 'monitoring/prometheus.yml').read_text())
        job = next(job for job in config['scrape_configs'] if job['job_name'] == 'canary')
        self.assertEqual(job['static_configs'][0]['targets'], ['canary:9465'])

        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}
        failing = rules['CanaryFailing']
        self.assertIn('time() - backendrelay_canary_last_success_timestamp_seconds > 300',
                      failing['expr'])
        self.assertIn('backendrelay_canary_last_success_timestamp_seconds > 0',
                      failing['expr'])
        self.assertIn('backendrelay_canary_last_success_timestamp_seconds == 0',
                      failing['expr'])
        self.assertIn('backendrelay_canary_started_timestamp_seconds', failing['expr'])
        self.assertEqual(failing['for'], '1m')
        self.assertEqual(failing['labels']['severity'], 'critical')
        slow = rules['CanaryPropagationSlow']
        self.assertIn('histogram_quantile(0.95', slow['expr'])
        self.assertIn('backendrelay_canary_propagation_seconds_bucket', slow['expr'])
        self.assertIn('> 5', slow['expr'])
        self.assertEqual(slow['for'], '10m')
        self.assertEqual(slow['labels']['severity'], 'warning')

    def test_canary_image_is_digest_pinned_and_runs_unprivileged(self):
        dockerfile = (ROOT / 'canary/Dockerfile').read_text()
        self.assertIn(
            'node:22.22.2-slim@sha256:9f6d5975c7dca860947d3915877f85607946403fc55349f39b4bc3688448bb6e',
            dockerfile)
        self.assertIn('USER 10001:10001', dockerfile)
        self.assertIn('CMD ["node", "src/index.mjs"]', dockerfile)
        self.assertIn('EXPOSE 9465', dockerfile)

    def test_scrapes_and_alerts_include_failure_absence_and_external_deadman(self):
        config = yaml.safe_load((ROOT / 'monitoring/prometheus.yml').read_text())
        self.assertEqual(config['global']['scrape_interval'], '15s')
        backendrelay = next(job for job in config['scrape_configs']
                            if job['job_name'] == 'backendrelay')
        self.assertEqual(backendrelay['static_configs'][0]['targets'], ['server:9091'])
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}
        self.assertIn('absent(', rules['BackupStale']['expr'])
        self.assertNotIn('DurableAuthPolicyUnsafe', rules)
        self.assertIn('pg_up == 0', rules['DatabaseUnavailable']['expr'])
        self.assertIn('pg_exporter_last_scrape_error', rules['DatabaseMetricsFailed']['expr'])
        self.assertIn('absent(backendrelay_wal_retained_bytes)', rules['DatabaseMetricsFailed']['expr'])
        task_stale = rules['TaskStale']
        self.assertIn('time() - backendrelay_task_last_success_timestamp_seconds', task_stale['expr'])
        self.assertIn('backendrelay_task_last_success_timestamp_seconds > 0', task_stale['expr'])
        self.assertEqual(task_stale['labels']['severity'], 'warning')
        task_failing = rules['TaskFailing']
        self.assertIn('outcome=~"failure|timeout|panic"', task_failing['expr'])
        self.assertIn('unless', task_failing['expr'])
        memory = rules['ServerMemoryHigh']
        self.assertIn('backendrelay_process_resident_memory_bytes', memory['expr'])
        self.assertIn('backendrelay_process_memory_limit_bytes', memory['expr'])
        self.assertEqual(memory['expr'], 'backendrelay_process_resident_memory_bytes / clamp_min(backendrelay_process_memory_limit_bytes, 1) > 0.8')
        self.assertEqual(memory['for'], '10m')
        self.assertEqual(memory['labels']['severity'], 'warning')
        flapping = rules['InvalidationStreamFlapping']
        self.assertEqual(
            flapping['expr'],
            'increase(backendrelay_invalidation_stream_restarts_total[10m]) > 3',
        )
        self.assertEqual(flapping['for'], '1m')
        self.assertEqual(flapping['labels']['severity'], 'warning')
        self.assertNotIn('ExecutionBacklog', rules)
        self.assertNotIn('FailedJobs', rules)
        receivers = yaml.safe_load((ROOT / 'monitoring/alertmanager.yml').read_text())
        self.assertEqual(
            {r['name'] for r in receivers['receivers']},
            {'pushover-critical', 'pushover-warning', 'external-deadman'})
        self.assertEqual(rules['Watchdog']['expr'], 'vector(1)')

    def test_delivery_email_and_storage_failures_reach_alerts(self):
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}

        webhook = rules['WebhookDeliveryFailing']
        self.assertIn(
            'backendrelay_webhook_deliveries_total{outcome="failed"}',
            webhook['expr'],
        )
        self.assertIn(
            'backendrelay_webhook_deliveries_total{outcome="delivered"}',
            webhook['expr'],
        )
        self.assertIn('unless', webhook['expr'])
        self.assertEqual(webhook['for'], '15m')
        self.assertEqual(webhook['labels']['severity'], 'warning')

        email = rules['EmailFailing']
        self.assertIn('backendrelay_emails_total{outcome="failed"}', email['expr'])
        self.assertIn('backendrelay_emails_total{outcome="sent"}', email['expr'])
        self.assertIn('unless', email['expr'])
        self.assertEqual(email['for'], '15m')
        self.assertEqual(email['labels']['severity'], 'warning')

        storage = rules['StorageErrors']
        self.assertIn('backendrelay_storage_ops_total{outcome="error"}', storage['expr'])
        self.assertIn('backendrelay_storage_ops_total{outcome=~"ok|error"}', storage['expr'])
        self.assertIn('clamp_min', storage['expr'])
        self.assertIn('> 0.05', storage['expr'])
        self.assertEqual(storage['for'], '10m')
        self.assertEqual(storage['labels']['severity'], 'warning')

    def test_prometheus_sample_limit_covers_per_app_series(self):
        config = yaml.safe_load((ROOT / 'monitoring/prometheus.yml').read_text())
        backendrelay = next(job for job in config['scrape_configs'] if job['job_name'] == 'backendrelay')
        self.assertEqual(backendrelay['sample_limit'], 3000)

    def test_postgres_exporter_queries_expose_per_app_write_storage_and_webhook_metrics(self):
        doc = yaml.safe_load((ROOT / 'monitoring/postgres-queries.yml').read_text())
        for name, table in (
            ('backendrelay_app_writes', 'FROM transactions'),
            ('backendrelay_app_triples', 'FROM triples_size_aggregate'),
            ('backendrelay_app_webhook_events', 'FROM webhook_events'),
        ):
            query = doc[name]
            self.assertEqual(query['cache_seconds'], 60)
            self.assertIn(table, query['query'])
            labels = [
                label
                for row in query['metrics']
                for label, value in row.items()
                if value['usage'] == 'LABEL'
            ]
            self.assertEqual(labels, ['app'])
        exported = {
            f'{name}_{column}'
            for name, query in doc.items()
            for row in query['metrics']
            for column, value in row.items()
            if value['usage'] != 'LABEL'
        }
        for metric in ('backendrelay_app_writes_per_minute',
                       'backendrelay_app_triples_bytes',
                       'backendrelay_app_webhook_events_pending',
                       'backendrelay_app_webhook_events_failed'):
            self.assertIn(metric, exported)

    def test_pgbackrest_units_and_archive_alert_are_operator_safe(self):
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}
        archive = rules['WalArchiveFailing']
        self.assertIn('pg_stat_archiver_failed_count', archive['expr'])
        self.assertIn('pg_stat_archiver_last_archive_age', archive['expr'])
        self.assertIn('pg_stat_archiver_archived_count', archive['expr'])
        self.assertEqual(archive['labels']['severity'], 'critical')

        for kind, calendar in (('full', 'Sun *-*-* 02:30:00 UTC'),
                               ('diff', 'Mon..Sat *-*-* 02:30:00 UTC')):
            service = (ROOT / f'monitoring/backendrelay-pgbackrest-{kind}.service').read_text()
            timer = (ROOT / f'monitoring/backendrelay-pgbackrest-{kind}.timer').read_text()
            self.assertIn(f'--type={kind} backup', service)
            self.assertIn('--stanza=backendrelay', service)
            self.assertIn('exec -T -u postgres postgres pgbackrest', service)
            self.assertIn('EnvironmentFile=-/etc/backendrelay/pgbackrest.env', service)
            self.assertIn('PGBACKREST_HEALTHCHECK_URL', service)
            self.assertIn('curl -fsS -m 10', service)
            self.assertIn('-/bin/sh -c', service)
            self.assertIn(f'OnCalendar={calendar}', timer)
            self.assertIn(f'Unit=backendrelay-pgbackrest-{kind}.service', timer)

    def test_realtime_and_server_health_alerts_cover_exported_metrics(self):
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}
        for alert, metric, severity in (
            ('RealtimeRefreshBacklog', 'backendrelay_refresh_waiting', 'warning'),
            ('RealtimeRefreshRejected', 'backendrelay_refresh_admission_rejections_total', 'warning'),
            ('RealtimeReceiveWait', 'backendrelay_receive_oldest_wait_ms', 'warning'),
            ('WebsocketExitsHigh', 'backendrelay_websocket_exits_total', 'warning'),
            ('CacheBudgetPressure', 'backendrelay_result_cache_max_bytes', 'warning'),
            ('ProcessPanics', 'backendrelay_panics_total', 'critical'),
        ):
            self.assertIn(metric, rules[alert]['expr'])
            self.assertEqual(rules[alert]['labels']['severity'], severity)
        self.assertEqual(rules['RealtimeRefreshBacklog']['expr'],
                         'max by (consumer) (backendrelay_refresh_waiting) > 50')
        self.assertEqual(rules['WebsocketExitsHigh']['expr'],
                         'sum by (reason) (rate(backendrelay_websocket_exits_total{reason!~"peer-close|inbound-eof"}[5m])) > 0.5')
        self.assertIn('backendrelay_result_cache_bytes', rules['CacheBudgetPressure']['expr'])
        self.assertIn('increase(', rules['ProcessPanics']['expr'])
        for alert in ('TaskFailing', 'TaskStale', 'EmailFailing'):
            self.assertIn(alert, rules)

    def test_postgres_capacity_alerts_and_exporter_queries(self):
        alerts = yaml.safe_load((ROOT / 'monitoring/alerts.yml').read_text())
        rules = {r['alert']: r for g in alerts['groups'] for r in g['rules']}
        for alert, metric, severity in (
            ('PostgresConnectionsHigh', 'backendrelay_postgres_connections_used', 'warning'),
            ('PostgresLongTransaction', 'backendrelay_postgres_longest_transaction_seconds', 'warning'),
            ('PostgresIdleInTransaction', 'backendrelay_postgres_idle_in_transaction_seconds', 'warning'),
            ('PostgresDeadlocks', 'pg_stat_database_deadlocks', 'warning'),
            ('TriplesDeadTuplesHigh', 'pg_stat_user_tables_n_dead_tup', 'warning'),
            ('DatabaseSizeGrowth', 'pg_database_size_bytes', 'warning'),
        ):
            self.assertIn(metric, rules[alert]['expr'])
            self.assertEqual(rules[alert]['labels']['severity'], severity)
        self.assertIn('backendrelay_postgres_connections_max',
                      rules['PostgresConnectionsHigh']['expr'])
        self.assertIn('relname="triples"', rules['TriplesDeadTuplesHigh']['expr'])
        self.assertIn('offset 1d', rules['DatabaseSizeGrowth']['expr'])

        queries = yaml.safe_load((ROOT / 'monitoring/postgres-queries.yml').read_text())
        for name, fragment in (
            ('backendrelay_postgres_connections', 'max_connections'),
            ('backendrelay_postgres_longest_transaction', 'xact_start'),
            ('backendrelay_postgres_idle_in_transaction', "state = 'idle in transaction'"),
            ('backendrelay_pg_statements', 'FROM pg_stat_statements'),
            ('backendrelay_pg_statements_top', 'ORDER BY total_exec_time'),
        ):
            self.assertIn(fragment, queries[name]['query'])
        self.assertEqual(queries['backendrelay_pg_statements']['cache_seconds'], 60)
        self.assertEqual(queries['backendrelay_pg_statements_top']['cache_seconds'], 60)
        exported = {
            f'{name}_{column}'
            for name, query in queries.items()
            for row in query['metrics']
            for column, value in row.items()
            if value['usage'] != 'LABEL'
        }
        for metric in ('backendrelay_postgres_connections_used',
                       'backendrelay_postgres_connections_max',
                       'backendrelay_postgres_longest_transaction_seconds',
                       'backendrelay_postgres_idle_in_transaction_seconds',
                       'backendrelay_pg_statements_calls'):
            self.assertIn(metric, exported)

    def test_alertmanager_uses_pushover_priorities_and_file_secrets(self):
        config = yaml.safe_load((ROOT / 'monitoring/alertmanager.yml').read_text())
        route = config['route']
        self.assertEqual(route['receiver'], 'pushover-warning')
        self.assertEqual(route['routes'][0]['receiver'], 'external-deadman')
        self.assertIn('alertname="Watchdog"', route['routes'][0]['matchers'])
        self.assertEqual(route['routes'][1]['receiver'], 'pushover-critical')
        self.assertIn('severity="critical"', route['routes'][1]['matchers'])

        receivers = {r['name']: r for r in config['receivers']}
        critical = receivers['pushover-critical']['pushover_configs'][0]
        warning = receivers['pushover-warning']['pushover_configs'][0]
        self.assertEqual(critical['priority'], 2)
        self.assertEqual(critical['retry'], '60s')
        self.assertEqual(critical['expire'], '1h')
        self.assertEqual(warning['priority'], 0)
        for config_entry in (critical, warning):
            self.assertNotIn('user_key', config_entry)
            self.assertNotIn('token', config_entry)
            self.assertEqual(config_entry['user_key_file'], '/run/secrets/pushover-user-key')
            self.assertEqual(config_entry['token_file'], '/run/secrets/pushover-token')

        compose = yaml.safe_load((ROOT / 'docker-compose.monitoring.yml').read_text())
        volumes = compose['services']['alertmanager']['volumes']
        joined = '\n'.join(volumes)
        self.assertIn('${PUSHOVER_USER_KEY_FILE:?', joined)
        self.assertIn('${PUSHOVER_TOKEN_FILE:?', joined)
        self.assertIn('/run/secrets/pushover-user-key:ro', joined)
        self.assertIn('/run/secrets/pushover-token:ro', joined)
        self.assertNotIn('operator-webhook', joined)
        self.assertIn('/run/secrets/deadman-webhook:ro', joined)

    def test_dashboard_queries_are_valid_fixed_contract_names(self):
        dashboard = json.loads((ROOT / 'monitoring/grafana/dashboards/backendrelay.json').read_text())
        panels = dashboard['panels']
        self.assertGreaterEqual(len(panels), 25)
        rows = {panel['title'] for panel in panels if panel['type'] == 'row'}
        self.assertEqual(rows, {'Realtime', 'Per app'})
        for panel in panels:
            if panel['type'] == 'row':
                self.assertNotIn('datasource', panel)
                continue
            self.assertEqual(panel['datasource']['uid'], 'backendrelay-prometheus')
            for target in panel['targets']:
                self.assertNotIn('app_id', target['expr'])
                self.assertNotIn('token', target['expr'])
        expressions = {target['expr'] for panel in panels for target in panel.get('targets', [])}
        for expression in (
            'max by (consumer) (backendrelay_refresh_waiting)',
            'sum by (reason) (rate(backendrelay_websocket_exits_total[5m]))',
            'backendrelay_result_cache_max_bytes',
            'backendrelay_app_sessions',
            'sum by (app) (rate(backendrelay_app_ops_total[5m]))',
            'backendrelay_app_writes_per_minute',
            'backendrelay_app_triples_bytes',
        ):
            self.assertIn(expression, expressions)

    def test_live_check_covers_every_p77_dependency(self):
        script_path = ROOT / 'monitoring/live-check.sh'
        script = script_path.read_text()
        subprocess.run(['bash', '-n', str(script_path)], check=True)
        self.assertTrue(script.startswith('#!/usr/bin/env bash\n'))
        self.assertIn('promtool query instant', script)
        for fragment in (
            'pg_stat_archiver_archived_count',
            'pg_stat_archiver_failed_count',
            'pg_stat_archiver_last_archive_age',
            'increase(pg_stat_archiver_failed_count[15m])',
            'pg_up == 1',
            'ALERTS{alertname="Watchdog",alertstate="firing"}',
            'ALERTS{alertname="WalArchiveFailing",alertstate="firing"}',
            'backendrelay_canary_last_success_timestamp_seconds',
            'backendrelay_backup_offhost_completed_seconds',
            'SELECT failed_count FROM pg_stat_archiver',
        ):
            self.assertIn(fragment, script)
        for service in ('prometheus', 'alertmanager', 'postgres-exporter',
                        'loki', 'alloy', 'canary'):
            self.assertIn(service, script)
        self.assertNotIn('CANARY_ADMIN_TOKEN', script)
        self.assertNotIn('PUSHOVER_TOKEN', script)

    def test_wal_alert_helper_only_injects_the_failure_counter(self):
        script_path = ROOT / 'monitoring/fire-wal-alert.sh'
        script = script_path.read_text()
        subprocess.run(['bash', '-n', str(script_path)], check=True)
        self.assertTrue(script.startswith('#!/usr/bin/env bash\n'))
        self.assertIn(
            'MONITORING_TEXTFILE_DIR:-/var/lib/node-exporter/textfile', script)
        self.assertIn('pg_stat_archiver_failed_count', script)
        # Only the failure counter is synthetic: faking archived_count or the
        # archive age would exercise a different branch of the rule.
        self.assertNotIn('pg_stat_archiver_archived_count', script)
        self.assertNotIn('pg_stat_archiver_last_archive_age', script)
        self.assertIn('trap cleanup EXIT', script)
        self.assertIn('alertname=WalArchiveFailing', script)
        self.assertIn('amtool', script)
        # The default must match the node-exporter mount in deploy/.env.example.
        env_example = (ROOT / 'deploy/.env.example').read_text()
        self.assertIn('MONITORING_TEXTFILE_DIR=/var/lib/node-exporter/textfile',
                      env_example)

    def test_monitoring_go_live_runbook_documents_the_alert_tests(self):
        operations = (ROOT.parent / 'docs/operations.md').read_text()
        start = operations.index('## Monitoring go-live and alert tests (P7.7)')
        end = operations.index('\n## ', start + 1)
        section = operations[start:end]
        for fragment in (
            'live-check.sh',
            'fire-wal-alert.sh',
            'pg_stat_archiver_failed_count',
            'pg_stat_archiver_archived_count',
            'pg_stat_archiver_last_archive_age',
            'dc stop alertmanager',
            'healthchecks.io',
            'alert add alertname=OperationalTestAlert',
            'pgbackrest --stanza=backendrelay check',
            '--type=full backup',
            'posix',
        ):
            self.assertIn(fragment, section)
        self.assertIn('### Monitoring checklist', section)
        # The P7.6 checklist must link the new section, not the reference one.
        self.assertIn('[P7.7](#monitoring-go-live-and-alert-tests-p77)', operations)
        self.assertNotIn('[P7.7](#external-monitoring-and-canary)', operations)
