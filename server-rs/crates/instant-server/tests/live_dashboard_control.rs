use std::future::{Future, ready};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use instant_http::{ApiError, ApiErrorType, DashboardRequest, DashboardResponse};
use instant_instaql::{AttributeValueType, Cardinality, system_attr_id, system_attribute_catalog};
use instant_permissions::InMemoryRateLimiter;
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardControlService,
    DashboardRateLimitHooks, DashboardSubsystem, INSTANT_CONFIG_APP_ID, NoopMagicCodeRateLimiter,
    OAuthSecretCipher, PostgresPool, ProductionDashboardTerminalHooks, ServerConfig,
    StaticWebhookLimit, production_dashboard_adapters,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Clone, Default)]
struct Hooks {
    calls: Arc<Mutex<Vec<&'static str>>>,
    special_creator: Arc<Mutex<Option<Uuid>>>,
    verification_code: Arc<Mutex<Option<String>>>,
}

impl DashboardControlHooks for Hooks {
    #[allow(clippy::too_many_lines)]
    fn execute(
        &self,
        operation: DashboardControlHookOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        let (name, result) = match operation {
            DashboardControlHookOperation::AfterCreate { .. } => ("create", Value::Null),
            DashboardControlHookOperation::ClearApp { .. } => ("clear", Value::Null),
            DashboardControlHookOperation::EnrichApp { mut app, .. } => {
                app["enriched"] = json!(true);
                ("get", app)
            }
            DashboardControlHookOperation::SoftDeletedAttributes { .. } => {
                ("soft-delete", json!([{"id": Uuid::nil()}]))
            }
            DashboardControlHookOperation::OrgDetails { org, .. } => (
                "org",
                json!({"org": org, "apps": [], "members": [], "invites": []}),
            ),
            DashboardControlHookOperation::AfterTransfer { .. } => (
                "transfer",
                json!({"credit": null, "app_member_changes": {"removed": []}}),
            ),
            DashboardControlHookOperation::DeliverInvite { .. } => ("invite", Value::Null),
            DashboardControlHookOperation::AuthorizeClaim { .. } => ("claim", Value::Bool(true)),
            DashboardControlHookOperation::EncryptOAuthSecret { plaintext, .. } => {
                ("encrypt", Value::String(format!("encrypted:{plaintext}")))
            }
            DashboardControlHookOperation::ValidateOAuthDiscovery { .. } => {
                ("discovery", Value::Bool(true))
            }
            DashboardControlHookOperation::AuthorizeSharedOAuthCredentials { .. } => {
                ("shared", Value::Bool(true))
            }
            DashboardControlHookOperation::DashboardAuthDefaultSender { .. } => (
                "sender",
                Value::String("auth@self-hosted.example".to_owned()),
            ),
            DashboardControlHookOperation::SpecialAppConfiguration { .. } => (
                "special-config",
                json!({
                    "creator_id": self
                        .special_creator
                        .lock()
                        .unwrap()
                        .unwrap_or_else(Uuid::nil),
                    "enabled": true
                }),
            ),
            DashboardControlHookOperation::EnqueueIndexingJob { .. } => ("index", Value::Null),
            DashboardControlHookOperation::ValidateWebhookSink { .. } => {
                ("webhook-validate", Value::Bool(true))
            }
            DashboardControlHookOperation::EnableWebhookLogs { .. } => {
                ("webhook-logs", Value::Null)
            }
            DashboardControlHookOperation::NotifyWebhookEvent { .. } => {
                ("webhook-notify", Value::Null)
            }
            DashboardControlHookOperation::AuthorizeBackup { .. } => {
                ("backup-authorize", Value::Bool(true))
            }
            DashboardControlHookOperation::BackupRateLimitConfiguration => (
                "backup-rate-limits",
                json!({
                    "app": {"capacity": 1, "windowMinutes": 5},
                    "ip": {"capacity": 2, "windowMinutes": 5}
                }),
            ),
            DashboardControlHookOperation::EnqueueBackupJob { .. } => {
                ("backup-enqueue", Value::Null)
            }
            DashboardControlHookOperation::ListBackupFiles { .. } => (
                "backup-files",
                json!([{"name": "config.json", "size": 100}]),
            ),
            DashboardControlHookOperation::PresignBackupFile { .. } => (
                "backup-url",
                Value::String("https://backups.example.com/config.json".to_owned()),
            ),
            DashboardControlHookOperation::StreamBackupStorageFiles { .. } => (
                "backup-storage",
                Value::String("{\"done\":true}\n".to_owned()),
            ),
            DashboardControlHookOperation::UploadDashboardFile { body, .. } => {
                ("storage-upload", json!({"size": body.len()}))
            }
            DashboardControlHookOperation::DeleteDashboardFiles { paths, .. } => {
                ("storage-delete", json!({"deleted": paths}))
            }
            DashboardControlHookOperation::SessionCounts => {
                ("session-counts", json!({"connected": 7}))
            }
            DashboardControlHookOperation::AppSessionStats { .. } => (
                "app-stats",
                json!({"count": 3, "origins": {"https://app.example.com": 3}}),
            ),
            DashboardControlHookOperation::AppSessionCounts => ("app-session-counts", json!({})),
            DashboardControlHookOperation::ActiveSessionStats => (
                "active-session-stats",
                json!({"total-count": 7, "total-queries": 11}),
            ),
            DashboardControlHookOperation::DefaultEmailTemplate => (
                "email-default",
                json!({
                    "sender-email": "hello@example.com",
                    "subject": "{code} for {app_title}",
                    "body": "Use {code} within {expiration}.",
                    "verification-code-expiry-minutes": 1440
                }),
            ),
            DashboardControlHookOperation::SyncEmailSender { .. } => ("email-sync", json!(42)),
            DashboardControlHookOperation::VerifyEmailDomain { .. } => {
                ("email-domain", Value::Null)
            }
            DashboardControlHookOperation::GetEmailSender { .. } => (
                "email-sender",
                json!({
                    "ID": 42,
                    "EmailAddress": "custom@example.com",
                    "Confirmed": true,
                    "DKIMHost": "dkim.example.com"
                }),
            ),
            DashboardControlHookOperation::AuthorizeEmailSend { .. } => {
                ("email-authorize", Value::Bool(true))
            }
            DashboardControlHookOperation::DeliverDashboardEmail { kind, subject, .. } => {
                if kind == "sender-verification" {
                    *self.verification_code.lock().unwrap() =
                        subject.split_whitespace().next().map(str::to_owned);
                }
                ("email-deliver", Value::Bool(true))
            }
            DashboardControlHookOperation::AuthorizeDashboardAdmin { .. } => {
                ("restore-authorize", Value::Bool(true))
            }
            DashboardControlHookOperation::PersistRestoreZip { .. } => (
                "restore-persist",
                Value::String("/tmp/instant-test-restore.zip".to_owned()),
            ),
            DashboardControlHookOperation::StageBackupRestoreZip { .. } => (
                "restore-stage",
                Value::String("/tmp/instant-test-restore.zip".to_owned()),
            ),
            DashboardControlHookOperation::DiscardRestoreZip { .. } => {
                ("restore-discard", Value::Null)
            }
            DashboardControlHookOperation::EnqueueRestoreJob { .. } => {
                ("restore-enqueue", Value::Null)
            }
            DashboardControlHookOperation::DashboardAdminRecentUsers => ("admin-users", json!([])),
            DashboardControlHookOperation::DashboardAdminTopUsers { .. } => {
                ("admin-top", json!([]))
            }
            DashboardControlHookOperation::DashboardAdminStorageMetrics => {
                ("admin-storage", json!([]))
            }
            DashboardControlHookOperation::DashboardAdminOverviewDaily => {
                ("admin-daily", json!({"charts": {}}))
            }
            DashboardControlHookOperation::DashboardAdminOverviewMinute => (
                "admin-minute",
                json!({"session-reports": {}, "proxied-connections": null}),
            ),
            DashboardControlHookOperation::DashboardOAuthConfiguration { .. } => (
                "dashboard-oauth-config",
                json!({
                    "authorization_endpoint": "https://accounts.example.com/oauth",
                    "client_id": "dashboard-client",
                    "callback_url": "https://api.example.com/dash/oauth/callback",
                    "dashboard_origin": "https://dashboard.example.com",
                    "dev_dashboard_origin": "http://localhost:3000",
                    "secure_cookie": true
                }),
            ),
            DashboardControlHookOperation::DashboardOAuthExchangeCode { .. } => (
                "dashboard-oauth-exchange",
                json!({
                    "email": "operator@example.com",
                    "subject": "google-subject",
                    "email_verified": true
                }),
            ),
            DashboardControlHookOperation::AuthorizeDashboardOAuthSignup { .. } => {
                ("dashboard-oauth-signup", Value::Bool(true))
            }
        };
        self.calls.lock().unwrap().push(name);
        ready(Ok(result))
    }
}

#[allow(clippy::needless_pass_by_value)]
fn request(method: Method, path: &str, token: &str, value: Value) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    DashboardRequest {
        method,
        path: path.to_owned(),
        query: None,
        headers,
        body: Bytes::from(serde_json::to_vec(&value).unwrap()),
        staged_body_path: None,
        remote_addr: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn body(response: DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

async fn insert_system_attrs(client: &tokio_postgres::Client, app_id: Uuid) {
    for attribute in system_attribute_catalog().unwrap().attributes {
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        client
            .execute(
                "INSERT INTO attrs
                   (id, app_id, value_type, cardinality, is_unique, is_indexed,
                    is_required, forward_ident, etype, label)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT DO NOTHING",
                &[
                    &attribute.id,
                    &app_id,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &attribute.required,
                    &Uuid::new_v4(),
                    &attribute.etype,
                    &attribute.label,
                ],
            )
            .await
            .unwrap();
    }
}

#[test]
fn dashboard_app_and_org_control_plane_enforces_roles_and_persists_changes() {
    std::thread::Builder::new()
        .name("live-dashboard-control".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_dashboard_app_and_org_control_plane());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[allow(clippy::too_many_lines)]
async fn run_dashboard_app_and_org_control_plane() {
    let Some(url) =
        instant_test_support::test_database_url("run_dashboard_app_and_org_control_plane")
    else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TYPE pg_temp.attr_on_delete AS ENUM ('cascade');
             CREATE TYPE pg_temp.webhook_action AS ENUM ('create', 'update', 'delete');
             CREATE TYPE pg_temp.webhook_status AS ENUM ('active', 'disabled');
             CREATE TYPE pg_temp.webhook_event_status AS ENUM ('pending', 'processing', 'success', 'error', 'failed');
             CREATE TYPE pg_temp.isn AS (slot_num int, lsn pg_lsn);
             CREATE TYPE pg_temp.webhook_attempt AS (
               attempt_at timestamp with time zone,
               duration_ms int,
               success boolean,
               status_code int,
               response_text text,
               error_type text,
               error_message text
             );
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE attrs (
               id uuid NOT NULL,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL DEFAULT false,
               is_indexed boolean NOT NULL DEFAULT false,
               is_required boolean NOT NULL DEFAULT false,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               inferred_types bit(32),
               on_delete attr_on_delete,
               on_delete_reverse attr_on_delete,
               checked_data_type checked_data_type,
               checking_data_type boolean,
               indexing boolean,
               setting_unique boolean,
               metadata jsonb,
               deletion_marked_at timestamp with time zone,
               PRIMARY KEY (app_id, id)
             );
             CREATE UNIQUE INDEX dashboard_control_attrs_id ON attrs(id);
             CREATE TEMP TABLE idents (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               UNIQUE (app_id, etype, label)
             );
             CREATE TEMP TABLE triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL,
               value_md5 text NOT NULL,
               ea boolean NOT NULL,
               eav boolean NOT NULL,
               av boolean NOT NULL,
               ave boolean NOT NULL,
               vae boolean NOT NULL,
               checked_data_type checked_data_type,
               created_at bigint NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::bigint),
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE UNIQUE INDEX dashboard_control_ea
               ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX dashboard_control_av
               ON triples(app_id, attr_id, public.json_null_to_null(value)) WHERE av;
             CREATE OR REPLACE FUNCTION public.jsonb_deep_merge(obj1 jsonb, obj2 jsonb)
             RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $$
             DECLARE key text; item jsonb;
             BEGIN
               IF obj1 IS NULL OR obj2 IS NULL OR jsonb_typeof(obj1) != 'object' OR jsonb_typeof(obj2) != 'object'
               THEN RETURN obj2; END IF;
               FOR key, item IN SELECT * FROM jsonb_each(obj2) LOOP
                 obj1 := CASE
                   WHEN obj1 ? key AND jsonb_typeof(obj1->key) = 'object' AND jsonb_typeof(item) = 'object'
                     THEN jsonb_set(obj1, ARRAY[key], public.jsonb_deep_merge(obj1->key, item))
                   WHEN jsonb_typeof(item) = 'null' THEN obj1 - key
                   ELSE jsonb_set(obj1, ARRAY[key], item)
                 END;
               END LOOP;
               RETURN obj1;
             END $$;
             CREATE TEMP TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE
             );
             CREATE TEMP TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               last_used_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_personal_access_tokens (
               id uuid PRIMARY KEY,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               name text NOT NULL,
               lookup_key bytea NOT NULL UNIQUE,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_user_oauth_access_tokens (
               id uuid PRIMARY KEY,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               lookup_key bytea NOT NULL UNIQUE,
               refresh_token_lookup_key bytea,
               client_id uuid,
               scopes text[] NOT NULL,
               expires_at timestamp with time zone NOT NULL
             );
             CREATE TEMP TABLE orgs (
               id uuid PRIMARY KEY,
               title text NOT NULL CHECK (length(title) <= 140),
               billing_email text,
               subscription_id uuid,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               updated_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE org_members (
               id uuid PRIMARY KEY,
               org_id uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               role text NOT NULL,
               UNIQUE (org_id, user_id)
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid REFERENCES instant_users(id),
               org_id uuid REFERENCES orgs(id),
               title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'active',
               magic_code_expiry_minutes integer,
               deletion_marked_at timestamp,
               subscription_id uuid
             );
             CREATE TEMP TABLE instant_subscription_types (
               id smallint PRIMARY KEY,
               name text NOT NULL
             );
             INSERT INTO instant_subscription_types (id, name)
             VALUES (1, 'Free'), (2, 'Pro'), (3, 'Startup');
             CREATE TEMP TABLE instant_subscriptions (
               id uuid PRIMARY KEY,
               user_id uuid,
               app_id uuid,
               org_id uuid,
               subscription_type_id smallint NOT NULL,
               stripe_customer_id text,
               stripe_subscription_id text,
               stripe_event_id text,
               source text NOT NULL DEFAULT 'stripe',
               created_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_oauth_apps (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               app_name text NOT NULL,
               granted_scopes text[] NOT NULL,
               is_public boolean NOT NULL,
               support_email text,
               app_home_page text,
               app_privacy_policy_link text,
               app_tos_link text,
               app_logo bytea,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               updated_at timestamp with time zone NOT NULL DEFAULT now(),
               UNIQUE (app_id, app_name)
             );
             CREATE TEMP TABLE instant_oauth_app_clients (
               client_id uuid PRIMARY KEY,
               oauth_app_id uuid NOT NULL,
               client_name text NOT NULL,
               authorized_redirect_urls text[],
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               updated_at timestamp with time zone NOT NULL DEFAULT now(),
               UNIQUE (oauth_app_id, client_name)
             );
             CREATE TEMP TABLE instant_oauth_app_client_secrets (
               id uuid PRIMARY KEY,
               client_id uuid NOT NULL,
               hashed_secret bytea NOT NULL,
               first_four text NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_user_oauth_refresh_tokens (
               lookup_key bytea PRIMARY KEY,
               client_id uuid NOT NULL,
               user_id uuid NOT NULL,
               scopes text[] NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE triples_size_aggregate (
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               pg_size bigint NOT NULL,
               files_size bigint,
               PRIMARY KEY (app_id, attr_id)
             );
             CREATE TEMP TABLE app_email_senders (
               id uuid PRIMARY KEY,
               postmark_id integer NOT NULL,
               email text NOT NULL UNIQUE,
               name text NOT NULL,
               email_provider text NOT NULL DEFAULT 'postmark'
             );
             CREATE TEMP TABLE app_sender_domains (
               app_id uuid NOT NULL,
               domain text NOT NULL,
               cloudflare_zone_id text,
               verified_at timestamp with time zone,
               ownership_verified_at timestamp with time zone,
               last_checked_at timestamp with time zone NOT NULL DEFAULT now(),
               last_error text,
               PRIMARY KEY (app_id, domain)
             );
             CREATE TEMP TABLE app_email_templates (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               email_type text NOT NULL,
               sender_id uuid,
               body text NOT NULL,
               subject text NOT NULL,
               name text,
               UNIQUE (app_id, email_type)
             );
             CREATE TEMP TABLE app_email_verifications (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               sender_id uuid NOT NULL,
               verified boolean NOT NULL DEFAULT false,
               UNIQUE (app_id, sender_id)
             );
             CREATE TEMP TABLE app_email_verification_codes (
               id uuid PRIMARY KEY,
               code text NOT NULL,
               app_id uuid NOT NULL,
               verification_id uuid NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE app_admin_tokens (
               token uuid PRIMARY KEY,
               app_id uuid NOT NULL UNIQUE REFERENCES apps(id) ON DELETE CASCADE,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE app_test_users (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               email text NOT NULL,
               code text NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               UNIQUE (app_id, email)
             );
             CREATE TEMP TABLE rules (
               app_id uuid PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
               code jsonb NOT NULL,
               version integer NOT NULL DEFAULT 0
             );
             CREATE TEMP TABLE rule_versions (
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               version integer NOT NULL,
               edits jsonb NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               PRIMARY KEY (app_id, version)
             );
             CREATE FUNCTION pg_temp.dashboard_rule_versions_trigger()
             RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF OLD.version IS DISTINCT FROM NEW.version THEN
                 INSERT INTO rule_versions (app_id, version, edits)
                 VALUES (NEW.app_id, NEW.version, jsonb_build_array(jsonb_build_object('op', 'replace')));
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER dashboard_rule_versions
               AFTER UPDATE ON rules FOR EACH ROW
               EXECUTE FUNCTION pg_temp.dashboard_rule_versions_trigger();
             CREATE TEMP TABLE indexing_jobs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               attr_id uuid NOT NULL,
               group_id uuid,
               job_serial_key text NOT NULL,
               job_type text NOT NULL,
               job_status text NOT NULL,
               job_dependency uuid REFERENCES indexing_jobs(id) ON DELETE CASCADE,
               job_stage text,
               checked_data_type checked_data_type,
               error text,
               work_estimate integer,
               work_completed integer,
               worker_id text,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               done_at timestamp with time zone,
               updated_at timestamp with time zone NOT NULL DEFAULT now(),
               invalid_unique_value jsonb,
               invalid_entity_id uuid,
               error_detail text,
               error_data jsonb,
               context jsonb
             );
             CREATE TEMP TABLE attr_sketches (
               app_id uuid NOT NULL,
               total bigint NOT NULL
             );
             CREATE TEMP TABLE app_backups (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               isn isn,
               backup_at timestamptz NOT NULL,
               storage_prefix text NOT NULL,
               files_size bigint,
               db_size bigint,
               uncompressed_size bigint,
               description text,
               expires_at timestamptz,
               deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_backup_schedules (
               app_id uuid PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
               cron text NOT NULL, enabled boolean NOT NULL,
               next_run_at timestamptz NOT NULL, last_enqueued_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE app_backup_jobs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               app_backup_id uuid,
               worker_id text,
               job_status text NOT NULL DEFAULT 'waiting',
               description text,
               error text,
               work_estimate bigint,
               work_completed bigint,
               created_at timestamptz NOT NULL DEFAULT now(),
               done_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE UNIQUE INDEX dashboard_backup_one_in_flight
               ON app_backup_jobs(app_id)
               WHERE job_status IN ('waiting', 'processing');
             CREATE TEMP TABLE webhooks (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               topics bigint NOT NULL,
               id_attr_ids uuid[],
               actions webhook_action[] NOT NULL,
               status webhook_status NOT NULL DEFAULT 'active',
               disabled_reason text,
               sink jsonb NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               updated_at timestamp with time zone NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE webhook_events (
               webhook_id uuid NOT NULL,
               isn isn NOT NULL,
               app_id uuid NOT NULL,
               status webhook_event_status NOT NULL,
               machine_id uuid,
               attempts webhook_attempt[],
               partition_bucket int NOT NULL DEFAULT 0,
               next_attempt_after timestamp with time zone,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               updated_at timestamp with time zone NOT NULL DEFAULT now(),
               PRIMARY KEY (webhook_id, isn, partition_bucket)
             );
             CREATE TEMP TABLE app_members (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               member_role text NOT NULL,
               UNIQUE (app_id, user_id)
             );
             CREATE TEMP TABLE app_member_invites (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               inviter_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               invitee_role text NOT NULL,
               invitee_email text NOT NULL,
               status text NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               sent_at timestamp DEFAULT now(),
               UNIQUE (app_id, invitee_email)
             );
             CREATE TEMP TABLE org_member_invites (
               id uuid PRIMARY KEY,
               org_id uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
               inviter_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               invitee_role text NOT NULL,
               invitee_email text NOT NULL,
               status text NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               sent_at timestamp with time zone DEFAULT now(),
               UNIQUE (org_id, invitee_email)
             );
             CREATE TEMP TABLE app_authorized_redirect_origins (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               service text NOT NULL,
               params text[] NOT NULL,
               created_at timestamp with time zone NOT NULL DEFAULT now(),
               UNIQUE (app_id, service, params)
             );",
        )
        .await
        .unwrap();

    let user_id = Uuid::new_v4();
    let other_user_id = Uuid::new_v4();
    let admin_user_id = Uuid::new_v4();
    let org_admin_user_id = Uuid::new_v4();
    let refresh = Uuid::new_v4();
    let other_refresh = Uuid::new_v4();
    let admin_refresh = Uuid::new_v4();
    let org_admin_refresh = Uuid::new_v4();
    let personal = "per_live_dashboard_control";
    let personal_digest = Sha256::digest(personal.as_bytes()).to_vec();
    let data_platform = "pat_live_dashboard_data";
    let data_platform_digest = Sha256::digest(data_platform.as_bytes()).to_vec();
    let collaborator_write_token = "pat_live_dashboard_collaborator_write";
    let collaborator_read_token = "pat_live_dashboard_collaborator_read";
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'owner@example.com'), ($2, 'admin@example.com'),
               ($3, 'app-admin@example.com'), ($4, 'org-admin@example.com')",
            &[&user_id, &other_user_id, &admin_user_id, &org_admin_user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id)
             VALUES ($1, $2, $3), ($4, $5, $6), ($7, $8, $9), ($10, $11, $12)",
            &[
                &refresh,
                &Sha256::digest(refresh.to_string().as_bytes()).to_vec(),
                &user_id,
                &other_refresh,
                &Sha256::digest(other_refresh.to_string().as_bytes()).to_vec(),
                &other_user_id,
                &admin_refresh,
                &Sha256::digest(admin_refresh.to_string().as_bytes()).to_vec(),
                &admin_user_id,
                &org_admin_refresh,
                &Sha256::digest(org_admin_refresh.to_string().as_bytes()).to_vec(),
                &org_admin_user_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_personal_access_tokens (id, user_id, name, lookup_key)
             VALUES ($1, $2, 'automation', $3)",
            &[&Uuid::new_v4(), &user_id, &personal_digest],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_oauth_access_tokens
               (id, user_id, lookup_key, scopes, expires_at)
             VALUES ($1, $2, $3, ARRAY['data-read', 'data-write'], now() + interval '1 hour'),
                    ($4, $5, $6, ARRAY['apps-write'], now() + interval '1 hour'),
                    ($7, $5, $8, ARRAY['apps-read'], now() + interval '1 hour')",
            &[
                &Uuid::new_v4(),
                &user_id,
                &data_platform_digest,
                &Uuid::new_v4(),
                &other_user_id,
                &Sha256::digest(collaborator_write_token.as_bytes()).to_vec(),
                &Uuid::new_v4(),
                &Sha256::digest(collaborator_read_token.as_bytes()).to_vec(),
            ],
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let authorized_oauth_app_id = Uuid::new_v4();
    let authorized_oauth_client_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_oauth_apps
               (id, app_id, app_name, granted_scopes, is_public)
             VALUES ($1, $2, 'Authorized Tool', ARRAY['apps-read'], true)",
            &[&authorized_oauth_app_id, &app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_oauth_app_clients
               (client_id, oauth_app_id, client_name, authorized_redirect_urls)
             VALUES ($1, $2, 'Web', ARRAY['https://tool.example.com/callback'])",
            &[&authorized_oauth_client_id, &authorized_oauth_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_oauth_refresh_tokens
               (lookup_key, client_id, user_id, scopes)
             VALUES ($1, $2, $3, ARRAY['apps-read'])",
            &[
                &Sha256::digest(b"authorized-refresh").to_vec(),
                &authorized_oauth_client_id,
                &user_id,
            ],
        )
        .await
        .unwrap();
    let title_attr = Uuid::new_v4();
    let goals_id_attr = Uuid::new_v4();
    let title_ident = Uuid::new_v4();
    let goals_id_ident = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (
               id, app_id, value_type, cardinality, is_indexed, is_required,
               forward_ident, etype, label, checked_data_type
             ) VALUES
               ($1, $2, 'blob', 'one', true, true, $3, 'goals', 'title', 'string'),
               ($4, $2, 'blob', 'one', true, true, $5, 'goals', 'id', NULL)",
            &[
                &title_attr,
                &app_id,
                &title_ident,
                &goals_id_attr,
                &goals_id_ident,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label) VALUES
               ($1, $2, $3, 'goals', 'title'),
               ($4, $2, $5, 'goals', 'id')",
            &[
                &title_ident,
                &app_id,
                &title_attr,
                &goals_id_ident,
                &goals_id_attr,
            ],
        )
        .await
        .unwrap();
    let files_size_attr = system_attr_id("$files", "size").unwrap();
    client
        .execute(
            "INSERT INTO triples_size_aggregate (app_id, attr_id, pg_size, files_size)
             VALUES ($1, $2, 128, NULL), ($1, $3, 64, 4096)",
            &[&app_id, &title_attr, &files_size_attr],
        )
        .await
        .unwrap();
    let backup_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attr_sketches (app_id, total) VALUES ($1, 2)",
            &[&app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_backups
               (id, app_id, isn, backup_at, storage_prefix, files_size,
                db_size, uncompressed_size, description, expires_at)
             VALUES ($2, $1, '(0,0/9)', now(), 'app-prefix/backup', 10, 20, 30,
                     'Initial backup', now() + interval '1 day')",
            &[&app_id, &backup_id],
        )
        .await
        .unwrap();
    let event_app_id = Uuid::new_v4();
    let event_webhook_id = Uuid::new_v4();
    let event_isn = "0/0/1";
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'Event app')",
            &[&event_app_id, &user_id],
        )
        .await
        .unwrap();
    let app_member_org_id = Uuid::new_v4();
    let app_member_app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO orgs (id, title) VALUES ($1, 'Shared org')",
            &[&app_member_org_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO org_members (id, org_id, user_id, role) VALUES
               ($1, $2, $3, 'owner'), ($4, $2, $5, 'collaborator'),
               ($6, $2, $7, 'admin')",
            &[
                &Uuid::new_v4(),
                &app_member_org_id,
                &user_id,
                &Uuid::new_v4(),
                &admin_user_id,
                &Uuid::new_v4(),
                &org_admin_user_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO apps (id, org_id, title) VALUES ($1, $2, 'Shared app')",
            &[&app_member_app_id, &app_member_org_id],
        )
        .await
        .unwrap();
    let shared_admin_token = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&shared_admin_token, &app_member_app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_members (id, app_id, user_id, member_role)
             VALUES ($1, $2, $3, 'collaborator'), ($4, $2, $5, 'admin'),
                    ($6, $2, $7, 'collaborator')",
            &[
                &Uuid::new_v4(),
                &app_member_app_id,
                &other_user_id,
                &Uuid::new_v4(),
                &admin_user_id,
                &Uuid::new_v4(),
                &org_admin_user_id,
            ],
        )
        .await
        .unwrap();
    let event_sink = tokio_postgres::types::Json(json!({"url": "https://events.example.com"}));
    client
        .execute(
            "INSERT INTO webhooks
               (id, app_id, topics, id_attr_ids, actions, sink)
             VALUES ($1, $2, 0, '{}', ARRAY['create']::text[]::webhook_action[], $3)",
            &[&event_webhook_id, &event_app_id, &event_sink],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO webhook_events (webhook_id, isn, app_id, status)
             VALUES ($1, $2::text::isn, $3, 'error'::webhook_event_status)",
            &[
                &event_webhook_id,
                &event_isn
                    .parse::<instant_wal::Isn>()
                    .unwrap()
                    .to_postgres_composite(),
                &event_app_id,
            ],
        )
        .await
        .unwrap();

    let hooks = Hooks::default();
    *hooks.special_creator.lock().unwrap() = Some(user_id);
    let calls = Arc::clone(&hooks.calls);
    let verification_code = Arc::clone(&hooks.verification_code);
    insert_system_attrs(&client, app_id).await;
    let service = DashboardControlService::new(client, hooks)
        .with_webhook_limit_policy(Arc::new(StaticWebhookLimit(1)));
    let reserved = service
        .handle(request(
            Method::POST,
            "/dash/apps",
            &refresh.to_string(),
            json!({
                "id": INSTANT_CONFIG_APP_ID,
                "title": "Squatted config",
                "admin_token": Uuid::new_v4()
            }),
        ))
        .await
        .unwrap_err();
    assert_eq!(reserved.body().kind, ApiErrorType::ValidationFailed);
    let refresh_token = refresh.to_string();
    let app_member_org = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/orgs/{app_member_org_id}"),
                &other_refresh.to_string(),
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(app_member_org["org"]["id"], app_member_org_id.to_string());

    let event_path =
        format!("/dash/apps/{event_app_id}/webhooks/{event_webhook_id}/events/{event_isn}");
    let events = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{event_app_id}/webhooks/{event_webhook_id}/events"),
                data_platform,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["events"][0]["isn"], event_isn);
    assert!(events["pageInfo"]["startCursor"].as_str().is_some());
    let event = body(
        service
            .handle(request(Method::GET, &event_path, data_platform, json!({})))
            .await
            .unwrap(),
    );
    assert_eq!(event["event"]["status"], "error");
    let resent = body(
        service
            .handle(request(Method::POST, &event_path, data_platform, json!({})))
            .await
            .unwrap(),
    );
    assert_eq!(resent["event"]["status"], "pending");

    let ephemeral = body(
        service
            .handle(request(
                Method::POST,
                "/dash/apps/ephemeral",
                "",
                json!({"title": "Sandbox"}),
            ))
            .await
            .unwrap(),
    );
    let ephemeral_id = Uuid::parse_str(ephemeral["app"]["id"].as_str().unwrap()).unwrap();
    let ephemeral_admin = ephemeral["app"]["admin-token"].as_str().unwrap();
    assert!(ephemeral["expires_ms"].as_i64().is_some());
    let ephemeral_fetched = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/ephemeral/{ephemeral_id}"),
                "",
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert!(ephemeral_fetched["app"].get("admin_token").is_none());
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/ephemeral/{ephemeral_id}/status"),
            "",
            json!({"admin-token": ephemeral_admin, "status": "read-only"}),
        ))
        .await
        .unwrap();

    let get_a_db = body(
        service
            .handle(request(
                Method::POST,
                "/dash/apps/get_a_db",
                personal,
                json!({"title": "Get a DB"}),
            ))
            .await
            .unwrap(),
    );
    let get_a_db_id = Uuid::parse_str(get_a_db["app"]["id"].as_str().unwrap()).unwrap();
    assert!(get_a_db["app"]["admin-token"].as_str().is_some());
    let get_a_db_fetched = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/get_a_db/{get_a_db_id}"),
                "",
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert!(get_a_db_fetched["app"].get("admin_token").is_none());

    let admin_token = Uuid::new_v4();
    let refresh_string = refresh.to_string();
    let created = body(
        service
            .handle(request(
                Method::POST,
                "/dash/apps",
                &refresh_string,
                json!({"id": app_id, "title": " Personal app ", "admin_token": admin_token}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(created["app"]["title"], "Personal app");
    assert_eq!(created["app"]["admin_token"], admin_token.to_string());

    let app_stats = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/stats"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(app_stats["count"], 3);
    assert_eq!(app_stats["origins"]["https://app.example.com"], 3);
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/track-import"),
            "",
            json!({}),
        ))
        .await
        .unwrap();
    let session_counts = body(
        service
            .handle(request(Method::GET, "/dash/session_counts", "", json!({})))
            .await
            .unwrap(),
    );
    assert_eq!(session_counts["connected"], 7);
    let active_session_stats = body(
        service
            .handle(request(
                Method::GET,
                "/dash/stats/active_sessions",
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(active_session_stats["total-count"], 7);
    assert_eq!(active_session_stats["total-queries"], 11);
    let default_email = body(
        service
            .handle(request(
                Method::GET,
                "/dash/default-email-template",
                "",
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(default_email["email-type"], "magic-code");
    assert_eq!(default_email["sender-email"], "hello@example.com");
    let email_syncs_before = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| **call == "email-sync")
        .count();
    let default_sender_template = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/email_templates"),
                personal,
                json!({
                    "email-type": "magic-code",
                    "subject": "Default code {code} for {app_title}",
                    "body": "Hello {user_email}, use {code} within {expiration}.",
                    "sender-email": "",
                    "sender-name": "Instant Test"
                }),
            ))
            .await
            .unwrap(),
    );
    assert!(default_sender_template["id"].as_str().is_some());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == "email-sync")
            .count(),
        email_syncs_before,
        "the dashboard's empty default-sender value must not sync a custom sender"
    );
    let email_template = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/email_templates"),
                personal,
                json!({
                    "email-type": "magic-code",
                    "subject": "Code {code} for {app_title}",
                    "body": "Hello {user_email}, use {code} within {expiration}.",
                    "sender-email": "CUSTOM@EXAMPLE.COM",
                    "sender-name": "Instant Test"
                }),
            ))
            .await
            .unwrap(),
    );
    let email_template_id = email_template["id"].as_str().unwrap();
    let email_syncs_after_custom_sender = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| **call == "email-sync")
        .count();
    let edited_email_template = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/email_templates"),
                personal,
                json!({
                    "email-type": "magic-code",
                    "subject": "Edited code {code} for {app_title}",
                    "body": "Edited for {user_email}: use {code} within {expiration}.",
                    "sender-email": "custom@example.com",
                    "sender-name": "Instant Test"
                }),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(edited_email_template["id"], email_template_id);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == "email-sync")
            .count(),
        email_syncs_after_custom_sender,
        "editing a template with its existing sender must not require Postmark account access"
    );
    let email_status = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/email_status"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(email_status["info"]["email"], "custom@example.com");
    assert_eq!(email_status["info"]["verification_verified"], false);
    let sender_verification = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/sender-verification"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(sender_verification["instant"]["verified?"], false);
    assert_eq!(sender_verification["verification"]["ID"], 42);
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/sender-verification/send-magic-code"),
            personal,
            json!({}),
        ))
        .await
        .unwrap();
    let code = verification_code.lock().unwrap().clone().unwrap();
    let verified = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/sender-verification/verify-magic-code"),
                personal,
                json!({"code": code}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(verified["verified"], true);
    let sender_verification = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/sender-verification"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(sender_verification["instant"]["verified?"], true);
    assert!(
        sender_verification["domain"].is_null(),
        "Postmark verification does not require a sender domain record"
    );
    let sent_test = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/send-test-email"),
                personal,
                json!({
                    "to": "owner@example.com",
                    "subject": "Code {code} for {app_title}",
                    "body": "Hello {user_email}, use {code}.",
                    "sender-email": "custom@example.com",
                    "sender-name": "Instant Test"
                }),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(sent_test["sent-to"], "owner@example.com");
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/email_templates/{email_template_id}"),
            personal,
            json!({}),
        ))
        .await
        .unwrap();
    let oauth_app = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth-apps"),
                &refresh_string,
                json!({
                    "app_name": "Developer Tool",
                    "app_logo": "data:image/png;base64,aWNvbg==",
                    "support_email": "support@example.com",
                    "app_home_page": "https://tool.example.com",
                    "app_privacy_policy_link": "https://tool.example.com/privacy",
                    "app_tos_link": "https://tool.example.com/terms"
                }),
            ))
            .await
            .unwrap(),
    );
    let oauth_app_id = oauth_app["app"]["id"].as_str().unwrap();
    assert_eq!(oauth_app["app"]["appName"], "Developer Tool");
    assert_eq!(
        oauth_app["app"]["appLogo"],
        "data:image/png;base64,aWNvbg=="
    );
    let oauth_client = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth-apps/{oauth_app_id}/clients"),
                &refresh_string,
                json!({
                    "client_name": "Local CLI",
                    "authorized_redirect_urls": ["http://localhost:3000/callback"]
                }),
            ))
            .await
            .unwrap(),
    );
    let oauth_client_id = oauth_client["client"]["clientId"].as_str().unwrap();
    let initial_secret_id = oauth_client["clientSecret"]["id"].as_str().unwrap();
    assert_eq!(oauth_client["secretValue"].as_str().unwrap().len(), 68);
    assert_eq!(
        oauth_client["clientSecret"]["firstFour"]
            .as_str()
            .unwrap()
            .len(),
        4
    );
    let rotated_secret = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth-app-clients/{oauth_client_id}/client-secrets"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    let rotated_secret_id = rotated_secret["clientSecret"]["id"].as_str().unwrap();
    assert_eq!(rotated_secret["secretValue"].as_str().unwrap().len(), 68);
    let updated_oauth_client = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth-app-clients/{oauth_client_id}"),
                &refresh_string,
                json!({
                    "client_name": "Local and Web",
                    "add_redirect_url": "https://tool.example.com/callback"
                }),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        updated_oauth_client["client"]["clientName"],
        "Local and Web"
    );
    assert_eq!(
        updated_oauth_client["client"]["authorizedRedirectUrls"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let updated_oauth_app = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth-apps/{oauth_app_id}"),
                &refresh_string,
                json!({"app_name": "Developer Tool 2"}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(updated_oauth_app["app"]["appName"], "Developer Tool 2");
    let oauth_apps = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/oauth-apps"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(oauth_apps["apps"].as_array().unwrap().len(), 2);
    let managed = oauth_apps["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|app| app["id"] == oauth_app_id)
        .unwrap();
    assert_eq!(
        managed["clients"][0]["clientSecrets"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let authorized_apps = body(
        service
            .handle(request(
                Method::GET,
                "/dash/user/oauth_apps",
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(authorized_apps["oauthApps"][0]["name"], "Authorized Tool");
    let revoked_apps = body(
        service
            .handle(request(
                Method::POST,
                "/dash/user/oauth_apps/revoke_access",
                &refresh_string,
                json!({"oauthAppId": authorized_oauth_app_id}),
            ))
            .await
            .unwrap(),
    );
    assert!(revoked_apps["oauthApps"].as_array().unwrap().is_empty());
    for secret_id in [initial_secret_id, rotated_secret_id] {
        service
            .handle(request(
                Method::DELETE,
                &format!("/dash/apps/{app_id}/oauth-app-client-secrets/{secret_id}"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap();
    }
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/oauth-app-clients/{oauth_client_id}"),
            &refresh_string,
            json!({}),
        ))
        .await
        .unwrap();
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/oauth-apps/{oauth_app_id}"),
            &refresh_string,
            json!({}),
        ))
        .await
        .unwrap();

    let pulled_schema = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/schema/pull"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(pulled_schema["app-title"], "Personal app");
    assert_eq!(
        pulled_schema["attrs"].as_array().unwrap().len(),
        system_attribute_catalog().unwrap().attributes.len() + 2
    );
    assert_eq!(
        pulled_schema["schema"]["blobs"]["goals"]["title"]["id"],
        title_attr.to_string()
    );
    let schema_body = json!({
        "schema": {
            "entities": {
                "goals": {"attrs": {
                    "title": {
                        "valueType": "string",
                        "config": {"indexed": true, "unique": false},
                        "required": true
                    },
                    "description": {
                        "valueType": "any",
                        "config": {"indexed": false, "unique": false}
                    }
                }}
            },
            "links": {}
        },
        "check_types": true,
        "supports_background_updates": true
    });
    let schema_plan = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/schema/push/plan"),
                personal,
                schema_body.clone(),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(schema_plan["steps"].as_array().unwrap().len(), 1);
    assert_eq!(schema_plan["steps"][0][0], "add-attr");
    let schema_apply = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/schema/push/apply"),
                personal,
                schema_body,
            ))
            .await
            .unwrap(),
    );
    assert!(schema_apply["transaction"]["id"].as_i64().is_some());
    assert!(schema_apply["indexing-jobs"].is_null());
    let background_apply = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/schema/steps/apply"),
                personal,
                json!({"steps": [["remove-index", {
                    "attr-id": title_attr,
                    "forward-identity": [title_ident, "goals", "title"]
                }]]}),
            ))
            .await
            .unwrap(),
    );
    assert!(background_apply["transaction"].is_null());
    assert!(background_apply["steps"][0][1]["job-id"].as_str().is_some());
    assert_eq!(
        background_apply["indexing-jobs"]["jobs"][0]["job_type"],
        "remove-index"
    );
    let mut upload_request = request(
        Method::PUT,
        &format!("/dash/apps/{app_id}/storage/upload"),
        personal,
        json!({}),
    );
    upload_request
        .headers
        .insert("path", HeaderValue::from_static("notes/hello.txt"));
    upload_request
        .headers
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    upload_request.body = Bytes::from_static(b"hello");
    let uploaded = body(service.handle(upload_request).await.unwrap());
    assert_eq!(uploaded["data"]["size"], 5);
    let deleted_files = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/storage/files/delete"),
                personal,
                json!({"filenames": ["notes/hello.txt"]}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(deleted_files["data"]["deleted"], json!(["notes/hello.txt"]));
    let schedule_path = format!("/dash/apps/{app_id}/backup-schedule");
    assert!(
        body(
            service
                .handle(request(
                    Method::GET,
                    &schedule_path,
                    &refresh_token,
                    json!({})
                ))
                .await
                .unwrap()
        )["schedule"]
            .is_null()
    );
    let saved_schedule = body(
        service
            .handle(request(
                Method::PUT,
                &schedule_path,
                &refresh_token,
                json!({"cron":"0 */6 * * *","enabled":true}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(saved_schedule["schedule"]["cron"], "0 */6 * * *");
    assert_eq!(saved_schedule["schedule"]["enabled"], true);
    assert!(saved_schedule["schedule"]["next_run_at"].is_string());
    let read_schedule = body(
        service
            .handle(request(
                Method::GET,
                &schedule_path,
                &refresh_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(read_schedule, saved_schedule);
    assert!(
        service
            .handle(request(
                Method::PUT,
                &schedule_path,
                &refresh_token,
                json!({"cron":"60 * * * *","enabled":true})
            ))
            .await
            .is_err()
    );
    assert!(
        service
            .handle(request(
                Method::PUT,
                &format!("/dash/apps/{app_member_app_id}/backup-schedule"),
                &other_refresh.to_string(),
                json!({"cron":"0 * * * *","enabled":true})
            ))
            .await
            .is_err()
    );
    let paused_schedule = body(
        service
            .handle(request(
                Method::PUT,
                &schedule_path,
                &refresh_token,
                json!({"cron":"0 */6 * * *","enabled":false}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(paused_schedule["schedule"]["enabled"], false);
    let backups = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/backups"),
                &refresh_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(backups["backups"][0]["id"], backup_id.to_string());
    assert_eq!(backups["backups"][0]["isn"], "0/0/9");
    let backup_files = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/backups/{backup_id}/files"),
                &refresh_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(backup_files["files"][0]["name"], "config.json");
    let backup_url = body(
        service
            .handle(DashboardRequest {
                query: Some("name=config.json".to_owned()),
                ..request(
                    Method::GET,
                    &format!("/dash/apps/{app_id}/backups/{backup_id}/file-url"),
                    &refresh_token,
                    json!({}),
                )
            })
            .await
            .unwrap(),
    );
    assert_eq!(backup_url["url"], "https://backups.example.com/config.json");
    let storage_files = service
        .handle(request(
            Method::GET,
            &format!("/dash/apps/{app_id}/backups/{backup_id}/storage-files"),
            &refresh_token,
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(storage_files.body.buffered().unwrap(), "{\"done\":true}\n");
    let backup_job = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/backups"),
                &refresh_token,
                json!({"description": "Before migration"}),
            ))
            .await
            .unwrap(),
    );
    let backup_job_id = backup_job["job"]["id"].as_str().unwrap();
    assert_eq!(backup_job["job"]["work_estimate"], 2);
    let backup_jobs = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/backup-jobs"),
                &refresh_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(backup_jobs["jobs"].as_array().unwrap().len(), 1);
    let fetched_backup_job = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/backup-jobs/{backup_job_id}"),
                &refresh_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(fetched_backup_job["job"]["job_status"], "waiting");
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/backup-jobs/{backup_job_id}"),
            &refresh_token,
            json!({}),
        ))
        .await
        .unwrap();
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/backups/{backup_id}"),
            &refresh_token,
            json!({}),
        ))
        .await
        .unwrap();
    let indexing_job = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/indexing-jobs"),
                personal,
                json!({"attr-id": title_attr, "job-type": "index"}),
            ))
            .await
            .unwrap(),
    );
    let indexing_job_id = indexing_job["job"]["id"].as_str().unwrap();
    assert_eq!(indexing_job["job"]["attr_name"], "goals.title");
    assert_eq!(indexing_job["job"]["job_status"], "waiting");
    assert_eq!(indexing_job["job"]["job_stage"], "update-attr-start");
    let fetched_indexing_job = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/indexing-jobs/{indexing_job_id}"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(fetched_indexing_job["job"]["id"], indexing_job_id);
    let indexing_group = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/indexing-jobs/group/{}", Uuid::new_v4()),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert!(indexing_group["jobs"].as_array().unwrap().is_empty());

    let webhook = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/webhooks"),
                data_platform,
                json!({
                    "url": "https://hooks.example.com/instant",
                    "namespaces": ["goals"],
                    "actions": ["create", "update"]
                }),
            ))
            .await
            .unwrap(),
    );
    let webhook_id = webhook["webhook"]["id"].as_str().unwrap();
    assert_eq!(webhook["webhook"]["namespaces"], json!(["goals"]));
    let limit_error = service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/webhooks"),
            data_platform,
            json!({
                "url": "https://hooks.example.com/second",
                "namespaces": ["goals"],
                "actions": ["delete"]
            }),
        ))
        .await
        .unwrap_err();
    assert_eq!(limit_error.body().kind, ApiErrorType::ValidationFailed);
    assert_eq!(
        limit_error.body().message,
        "An app may not have more than 1 active webhooks."
    );
    let updated_webhook = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/webhooks/{webhook_id}"),
                data_platform,
                json!({"url": "https://hooks.example.com/replacement"}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        updated_webhook["webhook"]["sink"]["url"],
        "https://hooks.example.com/replacement"
    );
    let disabled_webhook = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/webhooks/{webhook_id}/disable"),
                data_platform,
                json!({"reason": "maintenance"}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(disabled_webhook["webhook"]["status"], "disabled");
    assert_eq!(
        disabled_webhook["webhook"]["disabled_reason"],
        "maintenance"
    );
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/webhooks/{webhook_id}/enable"),
            data_platform,
            json!({}),
        ))
        .await
        .unwrap();
    let listed_webhooks = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/webhooks"),
                data_platform,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(listed_webhooks["webhooks"].as_array().unwrap().len(), 1);
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/webhooks/{webhook_id}"),
            data_platform,
            json!({}),
        ))
        .await
        .unwrap();

    let test_user = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/test_users"),
                &refresh_string,
                json!({"email": "STATIC@EXAMPLE.COM", "code": "123456"}),
            ))
            .await
            .unwrap(),
    );
    let test_user_id = test_user["test-user"]["id"].as_str().unwrap();
    assert_eq!(test_user["test-user"]["email"], "static@example.com");
    let test_users = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/test_users"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(test_users["test-users"].as_array().unwrap().len(), 1);
    let collaborator_test_user = service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_member_app_id}/test_users"),
            &other_refresh.to_string(),
            json!({"email": "collaborator@example.com", "code": "654321"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        collaborator_test_user.status().as_u16(),
        403,
        "a Collaborator must not set a static test-user code"
    );
    assert_eq!(
        collaborator_test_user.body().message,
        "Only an admin may set a static test-user code"
    );
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_member_app_id}/test_users"),
            &admin_refresh.to_string(),
            json!({"email": "admin-static@example.com", "code": "112233"}),
        ))
        .await
        .unwrap();
    assert!(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/test_users"),
                personal,
                json!({}),
            ))
            .await
            .is_err()
    );
    let deleted_test_user = body(
        service
            .handle(request(
                Method::DELETE,
                &format!("/dash/apps/{app_id}/test_users"),
                &refresh_string,
                json!({"id": test_user_id}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(deleted_test_user["test-user"]["code"], "123456");

    let rules_path = format!("/dash/apps/{app_id}/rules");
    let first_rules = body(
        service
            .handle(request(
                Method::POST,
                &rules_path,
                personal,
                json!({"code": {"goals": {"allow": {"view": "false"}}}}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(first_rules["rules"]["version"], 0);
    let second_rules = body(
        service
            .handle(request(
                Method::POST,
                &rules_path,
                personal,
                json!({"code": {"goals": {"allow": {"view": "true"}}}}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(second_rules["rules"]["version"], 1);
    let pulled_perms = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/perms/pull"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(pulled_perms["perms"]["goals"]["allow"]["view"], "true");
    let versions = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/rule-versions"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(versions["versions"].as_array().unwrap().len(), 1);
    assert_eq!(versions["versions"][0]["version"], 1);

    let fetched = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}"),
                &admin_token.to_string(),
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(fetched["app"]["enriched"], true);
    assert_eq!(
        fetched["app"]["role"], "owner",
        "an app admin token is the app owner"
    );

    // Only Owners and Admins receive an app's admin token; the token grants
    // full data-plane access, so collaborators see every other field.
    let owner_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(owner_view["app"]["admin_token"], admin_token.to_string());
    assert_eq!(owner_view["app"]["role"], "owner");
    let admin_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_member_app_id}"),
                &admin_refresh.to_string(),
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        admin_view["app"]["admin_token"],
        shared_admin_token.to_string()
    );
    assert_eq!(
        admin_view["app"]["role"], "admin",
        "the app admin role outranks the org collaborator role"
    );
    let collaborator_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_member_app_id}"),
                &other_refresh.to_string(),
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert!(
        collaborator_view["app"].get("admin_token").is_none(),
        "collaborators must not receive the admin token"
    );
    assert_eq!(collaborator_view["app"]["role"], "collaborator");
    let org_admin_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_member_app_id}"),
                &org_admin_refresh.to_string(),
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        org_admin_view["app"]["role"], "admin",
        "the org admin role outranks the app collaborator role"
    );
    let org_owner_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_member_app_id}"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        org_owner_view["app"]["role"], "owner",
        "an org owner has owner access to every org app"
    );
    let collaborator_write_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_member_app_id}"),
                collaborator_write_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        collaborator_write_view["app"]["admin_token"],
        shared_admin_token.to_string(),
        "an apps-write platform token is an explicit admin grant"
    );
    assert_eq!(collaborator_write_view["app"]["role"], "collaborator");
    let collaborator_read_view = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_member_app_id}"),
                collaborator_read_token,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert!(
        collaborator_read_view["app"].get("admin_token").is_none(),
        "an apps-read platform token must not receive the admin token"
    );
    assert_eq!(collaborator_read_view["app"]["role"], "collaborator");

    // Clearing an app is owner-only, even for app admins and org admins.
    for token in [
        other_refresh.to_string(),
        admin_refresh.to_string(),
        org_admin_refresh.to_string(),
    ] {
        let denied = service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_member_app_id}/clear"),
                &token,
                json!({}),
            ))
            .await
            .unwrap_err();
        assert_eq!(denied.body().kind, ApiErrorType::PermissionDenied);
    }
    let cleared = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_member_app_id}/clear"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(cleared["ok"], true);
    assert!(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/rename"),
                &admin_token.to_string(),
                json!({"title": "not allowed"}),
            ))
            .await
            .is_err()
    );
    let admin_token_template = service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/email_templates"),
            &admin_token.to_string(),
            json!({
                "email-type": "magic-code",
                "subject": "Code {code} for {app_title}",
                "body": "Hello {user_email}, use {code} within {expiration}.",
                "sender-email": "attacker@example.com",
                "sender-name": "Attacker"
            }),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        admin_token_template.status().as_u16(),
        403,
        "an app admin token must not edit email templates"
    );
    let admin_token_invite = service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/invite/send"),
            &admin_token.to_string(),
            json!({"invitee-email": "teammate@example.com", "role": "admin"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        admin_token_invite.status().as_u16(),
        403,
        "an app admin token has no dashboard user to invite as"
    );
    assert_eq!(
        admin_token_invite.body().message,
        "Dashboard user token required"
    );

    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/rename"),
            &refresh_string,
            json!({"title": "Renamed"}),
        ))
        .await
        .unwrap();
    let claimable_app_id = Uuid::new_v4();
    let claimable_admin = Uuid::new_v4();
    service
        .handle(request(
            Method::POST,
            "/dash/apps",
            &refresh_string,
            json!({
                "id": claimable_app_id,
                "title": "Claimable",
                "admin_token": claimable_admin
            }),
        ))
        .await
        .unwrap();
    let provider = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth_service_providers"),
                personal,
                json!({"provider_name": "github"}),
            ))
            .await
            .unwrap(),
    );
    let provider_id = provider["provider"]["id"].as_str().unwrap();
    let oauth_client = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth_clients"),
                personal,
                json!({
                    "provider_id": provider_id,
                    "client_name": "github-web",
                    "client_id": "external-id",
                    "client_secret": "secret",
                    "extra_audiences": [
                        "com.example.ios",
                        " com.example.ios ",
                        "android-client-id"
                    ],
                    "redirect_to": "https://app.example.com/callback",
                    "meta": {"teamId": "TEAM", "keyId": "KEY"}
                }),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        oauth_client["client"]["extra_audiences"],
        json!(["com.example.ios", "android-client-id"])
    );
    let oauth_client_id = oauth_client["client"]["id"].as_str().unwrap();
    let updated_client = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/oauth_clients/{oauth_client_id}"),
                personal,
                json!({
                    "client_id": "replacement-id",
                    "client_secret": null,
                    "extra_audiences": [],
                    "meta": null
                }),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(updated_client["client"]["client_id"], "replacement-id");
    assert_eq!(updated_client["client"]["extra_audiences"], json!([]));
    assert!(updated_client["client"]["meta"].is_null());
    let auth_settings = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/auth"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        auth_settings["oauth_service_providers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(auth_settings["oauth_clients"].as_array().unwrap().len(), 1);
    assert_eq!(
        auth_settings["default_sender_email"],
        "auth@self-hosted.example"
    );
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/oauth_clients/{oauth_client_id}"),
            personal,
            json!({}),
        ))
        .await
        .unwrap();
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/ephemeral/{claimable_app_id}/claim"),
            &other_refresh.to_string(),
            json!({"token": claimable_admin}),
        ))
        .await
        .unwrap();
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/status"),
            &refresh_string,
            json!({"status": "read-only"}),
        ))
        .await
        .unwrap();
    let expiry = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/set-magic-code-expiry"),
                &refresh_string,
                json!({"expiry": 30}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(expiry["app"]["magic_code_expiry_minutes"], 30);
    let replacement_admin = Uuid::new_v4();
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/tokens"),
            &refresh_string,
            json!({"admin-token": replacement_admin}),
        ))
        .await
        .unwrap();
    let rotated = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/tokens/rotate"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    let server_admin = rotated["adminToken"].as_str().unwrap().to_owned();
    assert_ne!(server_admin, replacement_admin.to_string());
    let stale_admin = service
        .handle(request(
            Method::GET,
            &format!("/dash/apps/{app_id}"),
            &replacement_admin.to_string(),
            json!({}),
        ))
        .await
        .unwrap_err();
    assert_eq!(stale_admin.status().as_u16(), 401);
    let rotated_app = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}"),
                &server_admin,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(rotated_app["app"]["id"], app_id.to_string());
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/clear"),
            &refresh_string,
            json!({}),
        ))
        .await
        .unwrap();
    let soft_deleted = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}/soft_deleted_attrs"),
                &server_admin,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(soft_deleted["grace-period-days"], 2);
    let personal_fetch = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}"),
                personal,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(personal_fetch["app"]["id"], app_id.to_string());
    let origin = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/authorized_redirect_origins"),
                personal,
                json!({"service": "generic", "params": ["app.example.com"]}),
            ))
            .await
            .unwrap(),
    );
    let origin_id = origin["origin"]["id"].as_str().unwrap();
    assert_eq!(origin["origin"]["params"], json!(["app.example.com"]));
    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}/authorized_redirect_origins/{origin_id}"),
            personal,
            json!({}),
        ))
        .await
        .unwrap();
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/invite/send"),
            &refresh_string,
            json!({"invitee-email": "ADMIN@EXAMPLE.COM", "role": "admin"}),
        ))
        .await
        .unwrap();

    // An Admin may not invite an Owner, but the Owner may.
    let admin_owner_invite = service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_member_app_id}/invite/send"),
            &admin_refresh.to_string(),
            json!({"invitee-email": "owner-invite@example.com", "role": "owner"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        admin_owner_invite.status().as_u16(),
        403,
        "an Admin must not invite an Owner"
    );
    assert_eq!(
        admin_owner_invite.body().message,
        "Only an owner may invite another owner"
    );
    service
        .handle(request(
            Method::POST,
            &format!("/dash/apps/{app_id}/invite/send"),
            &refresh_string,
            json!({"invitee-email": "co-owner@example.com", "role": "owner"}),
        ))
        .await
        .unwrap();

    let org = body(
        service
            .handle(request(
                Method::POST,
                "/dash/orgs",
                &refresh_string,
                json!({"title": "Team"}),
            ))
            .await
            .unwrap(),
    );
    let org_id = Uuid::parse_str(org["org"]["id"].as_str().unwrap()).unwrap();
    service
        .handle(request(
            Method::POST,
            &format!("/dash/orgs/{org_id}/rename"),
            &refresh_string,
            json!({"title": "Team renamed"}),
        ))
        .await
        .unwrap();
    let org_details = body(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/orgs/{org_id}"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(org_details["org"]["title"], "Team renamed");
    let transferred = body(
        service
            .handle(request(
                Method::POST,
                &format!("/dash/apps/{app_id}/transfer_to_org/{org_id}"),
                &refresh_string,
                json!({}),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(transferred["app_member_changes"]["removed"], json!([]));

    service
        .handle(request(
            Method::DELETE,
            &format!("/dash/apps/{app_id}"),
            &refresh_string,
            json!({}),
        ))
        .await
        .unwrap();
    assert!(
        service
            .handle(request(
                Method::GET,
                &format!("/dash/apps/{app_id}"),
                personal,
                json!({}),
            ))
            .await
            .is_err()
    );
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [
            "org",
            "webhook-notify",
            "special-config",
            "create",
            "special-config",
            "special-config",
            "special-config",
            "create",
            "special-config",
            "create",
            "app-stats",
            "session-counts",
            "restore-authorize",
            "active-session-stats",
            "email-default",
            "email-sync",
            "email-sender",
            "email-authorize",
            "email-deliver",
            "email-default",
            "email-authorize",
            "email-domain",
            "email-sender",
            "email-authorize",
            "email-deliver",
            "index",
            "storage-upload",
            "storage-delete",
            "backup-authorize",
            "backup-files",
            "backup-url",
            "backup-storage",
            "backup-authorize",
            "backup-enqueue",
            "index",
            "webhook-validate",
            "webhook-logs",
            "webhook-validate",
            "webhook-logs",
            "webhook-validate",
            "get",
            "get",
            "get",
            "get",
            "get",
            "get",
            "get",
            "get",
            "clear",
            "create",
            "encrypt",
            "sender",
            "claim",
            "get",
            "clear",
            "soft-delete",
            "get",
            "invite",
            "invite",
            "org",
            "transfer"
        ]
    );
}

/// Regression guard for the analytics removal: the production dashboard chain
/// must claim every operation emitted by the dashboard service instead of
/// falling through to the strict terminal adapter. Ephemeral and platform app
/// creation previously emitted `CaptureAnalytics` and returned a 500 once the
/// analytics adapter was deleted.
#[test]
fn production_dashboard_chain_claims_every_remaining_operation() {
    std::thread::Builder::new()
        .name("live-production-dashboard-chain".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_production_dashboard_chain());
        })
        .unwrap()
        .join()
        .unwrap();
}

// Same Tink AES-256-GCM keyset shape used by the cipher unit tests.
const KEYSET: &str = r#"{"primaryKeyId":42,"key":[{"keyData":{"typeUrl":"type.googleapis.com/google.crypto.tink.AesGcmKey","value":"CAAaIAABAgMEBQYHCAkKCwwNDw8QERITFBUWFxgZGhscHR4f","keyMaterialType":"SYMMETRIC"},"status":"ENABLED","keyId":42,"outputPrefixType":"TINK"}]}"#;

async fn connect_test_client(url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[allow(clippy::too_many_lines)]
async fn run_production_dashboard_chain() {
    let Some(url) = instant_test_support::test_database_url("run_production_dashboard_chain")
    else {
        return;
    };
    let cipher = OAuthSecretCipher::from_tink_json(KEYSET).unwrap();
    let config = ServerConfig::from_pairs([
        ("INSTANT_BACKEND_URL", "https://api.example.test"),
        ("INSTANT_DASHBOARD_URL", "https://dash.example.test"),
        (
            "INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL",
            "verify@example.test",
        ),
        ("INSTANT_TEAM_EMAIL_SENDER_EMAIL", "teams@example.test"),
        ("INSTANT_EPHEMERAL_APPS", "true"),
    ])
    .unwrap();
    // The same request must be rejected when ephemeral apps are not enabled.
    let disabled_config = ServerConfig::from_pairs([
        ("INSTANT_BACKEND_URL", "https://api.example.test"),
        ("INSTANT_DASHBOARD_URL", "https://dash.example.test"),
        (
            "INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL",
            "verify@example.test",
        ),
        ("INSTANT_TEAM_EMAIL_SENDER_EMAIL", "teams@example.test"),
    ])
    .unwrap();
    // `production_routes.rs` composes the same providers with
    // `production_dashboard_adapters_with_runtime_pools`; the fixed-client
    // constructor builds an identical adapter chain for this live test.
    let adapters = production_dashboard_adapters(
        &config,
        connect_test_client(&url).await,
        connect_test_client(&url).await,
        connect_test_client(&url).await,
        connect_test_client(&url).await,
        cipher,
        Vec::new(),
        ProductionDashboardTerminalHooks,
    );
    let hooks = DashboardRateLimitHooks::new(
        adapters.dashboard_hooks,
        Arc::new(InMemoryRateLimiter::default()),
        Arc::new(NoopMagicCodeRateLimiter),
    );
    let disabled_adapters = production_dashboard_adapters(
        &disabled_config,
        connect_test_client(&url).await,
        connect_test_client(&url).await,
        connect_test_client(&url).await,
        connect_test_client(&url).await,
        OAuthSecretCipher::from_tink_json(KEYSET).unwrap(),
        Vec::new(),
        ProductionDashboardTerminalHooks,
    );
    let disabled_hooks = DashboardRateLimitHooks::new(
        disabled_adapters.dashboard_hooks,
        Arc::new(InMemoryRateLimiter::default()),
        Arc::new(NoopMagicCodeRateLimiter),
    );
    let app_id = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    // Operations owned by the indexing/backup/webhook/storage/restore adapters
    // sit below this PostgreSQL-only chain, and the Google code exchange needs
    // network I/O; none of them is emitted by the app-creation paths.
    let operations = vec![
        DashboardControlHookOperation::AfterCreate {
            app_id,
            body: json!({}),
        },
        DashboardControlHookOperation::ClearApp { app_id },
        DashboardControlHookOperation::EnrichApp {
            app_id,
            user_id: Some(user_id),
            app: json!({}),
        },
        DashboardControlHookOperation::SoftDeletedAttributes { app_id },
        DashboardControlHookOperation::OrgDetails {
            org_id,
            user_id,
            org: json!({}),
        },
        DashboardControlHookOperation::AfterTransfer {
            app_id,
            org_id,
            user_id,
            removed_members: json!([]),
        },
        DashboardControlHookOperation::DeliverInvite {
            target: "app",
            foreign_key: app_id,
            inviter_id: user_id,
            inviter_email: "inviter@example.test".to_owned(),
            invitee_email: "invitee@example.test".to_owned(),
            title: "App".to_owned(),
            role: "creator".to_owned(),
        },
        DashboardControlHookOperation::AuthorizeClaim {
            app_id,
            current_creator_id: None,
            claimant_id: user_id,
            claimant_email: "claimant@example.test".to_owned(),
        },
        DashboardControlHookOperation::EncryptOAuthSecret {
            client_id: app_id,
            plaintext: "client-secret".to_owned(),
        },
        DashboardControlHookOperation::ValidateOAuthDiscovery {
            endpoint: "file:///etc/passwd".to_owned(),
        },
        DashboardControlHookOperation::AuthorizeSharedOAuthCredentials {
            app_id,
            provider_name: "google".to_owned(),
        },
        DashboardControlHookOperation::DashboardAuthDefaultSender { app_id },
        DashboardControlHookOperation::SpecialAppConfiguration { kind: "ephemeral" },
        DashboardControlHookOperation::SessionCounts,
        DashboardControlHookOperation::AppSessionStats { app_id },
        DashboardControlHookOperation::AppSessionCounts,
        DashboardControlHookOperation::ActiveSessionStats,
        DashboardControlHookOperation::DefaultEmailTemplate,
        DashboardControlHookOperation::SyncEmailSender {
            app_id,
            email: "sender@example.test".to_owned(),
            name: "Sender".to_owned(),
        },
        DashboardControlHookOperation::GetEmailSender { postmark_id: 1 },
        DashboardControlHookOperation::AuthorizeEmailSend {
            kind: "test",
            app_id,
            email: "user@example.test".to_owned(),
        },
        DashboardControlHookOperation::DeliverDashboardEmail {
            kind: "test",
            app_id,
            to: "user@example.test".to_owned(),
            sender_email: None,
            sender_name: "Instant".to_owned(),
            subject: "Subject".to_owned(),
            body: "Body".to_owned(),
        },
        DashboardControlHookOperation::AuthorizeBackup {
            app_id,
            creator_id: Some(user_id),
            work_estimate: 1,
            client_ip: None,
        },
        DashboardControlHookOperation::BackupRateLimitConfiguration,
        DashboardControlHookOperation::EnableWebhookLogs { app_id },
        DashboardControlHookOperation::AuthorizeDashboardAdmin {
            email: "admin@example.test".to_owned(),
        },
        DashboardControlHookOperation::DashboardAdminRecentUsers,
        DashboardControlHookOperation::DashboardAdminTopUsers { days: 7 },
        DashboardControlHookOperation::DashboardAdminStorageMetrics,
        DashboardControlHookOperation::DashboardAdminOverviewDaily,
        DashboardControlHookOperation::DashboardAdminOverviewMinute,
        DashboardControlHookOperation::DashboardOAuthConfiguration {
            redirect_to_dev: false,
        },
        DashboardControlHookOperation::AuthorizeDashboardOAuthSignup {
            email: "signup@example.test".to_owned(),
        },
    ];
    for operation in operations {
        let label = format!("{operation:?}");
        if let Err(error) = hooks.execute(operation).await {
            assert!(
                !error.body().message.contains("no native adapter"),
                "{label} reached the strict production terminal: {}",
                error.body().message
            );
        }
    }
    // The ephemeral-create path must complete end to end; this is the request
    // that returned a 500 when the analytics adapter was removed.
    let seed = connect_test_client(&url).await;
    seed.execute(
        "INSERT INTO instant_users (id, email) VALUES ($1, $2)
         ON CONFLICT (email) DO NOTHING",
        &[&Uuid::new_v4(), &"ephemeral-apps-dev@backendrelay.invalid"],
    )
    .await
    .unwrap();
    let service =
        DashboardControlService::from_pool(PostgresPool::connect(&url, 4).await.unwrap(), hooks);
    let response = service
        .handle(DashboardRequest {
            method: Method::POST,
            path: "/dash/apps/ephemeral".to_owned(),
            query: None,
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(&json!({"title": "Ephemeral"})).unwrap()),
            staged_body_path: None,
            remote_addr: None,
        })
        .await
        .expect("ephemeral app creation must not reach the strict terminal");
    assert_eq!(body(response)["app"]["title"], "Ephemeral");

    let disabled_service = DashboardControlService::from_pool(
        PostgresPool::connect(&url, 4).await.unwrap(),
        disabled_hooks,
    );
    let denied = disabled_service
        .handle(DashboardRequest {
            method: Method::POST,
            path: "/dash/apps/ephemeral".to_owned(),
            query: None,
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(&json!({"title": "Ephemeral"})).unwrap()),
            staged_body_path: None,
            remote_addr: None,
        })
        .await
        .unwrap_err();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(
        denied
            .body()
            .message
            .contains("Ephemeral app creation is disabled")
    );
}
