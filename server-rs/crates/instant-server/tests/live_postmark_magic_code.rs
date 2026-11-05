use instant_server::{
    EmailSender, MagicCodeDelivery, PostgresMagicCodeDelivery, PostmarkEmailClient,
};
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
async fn postgres_magic_code_delivery_resolves_current_template_and_verification_schema() {
    let Some(database_url) = instant_test_support::test_database_url(
        "postgres_magic_code_delivery_resolves_current_template_and_verification_schema",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY, title text NOT NULL,
               magic_code_expiry_minutes integer, deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_email_senders (
               id uuid PRIMARY KEY, email text NOT NULL
             );
             CREATE TEMP TABLE app_sender_domains (
               app_id uuid NOT NULL, domain text NOT NULL,
               cloudflare_zone_id text, verified_at timestamptz,
               ownership_verified_at timestamptz, last_checked_at timestamptz NOT NULL DEFAULT now(),
               last_error text, PRIMARY KEY (app_id, domain)
             );
             CREATE TEMP TABLE app_email_templates (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, email_type text NOT NULL,
               sender_id uuid, body text NOT NULL, subject text NOT NULL, name text
             );
             CREATE TEMP TABLE app_email_verifications (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, sender_id uuid NOT NULL,
               verified boolean NOT NULL
             );",
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let sender_id = Uuid::new_v4();
    client
        .execute("INSERT INTO apps VALUES ($1, 'A&B', 125, NULL)", &[&app_id])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_email_senders VALUES ($1, 'custom@example.com')",
            &[&sender_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_email_templates
               (id, app_id, email_type, sender_id, body, subject, name)
             VALUES ($1, $2, 'magic-code', $3, '<p>{app_title}: {code}</p>',
                     '{code} for {app_title}', 'Custom')",
            &[&Uuid::new_v4(), &app_id, &sender_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO app_email_verifications VALUES ($1, $2, $3, true)",
            &[&Uuid::new_v4(), &app_id, &sender_id],
        )
        .await
        .unwrap();

    let delivery = PostgresMagicCodeDelivery::new(
        client,
        PostmarkEmailClient::new(None),
        EmailSender {
            name: None,
            email: "verify@auth-pm.instantdb.com".to_owned(),
        },
    );
    delivery
        .deliver(app_id, "user@example.com", "123456", 125)
        .await
        .unwrap();
    let missing = delivery
        .deliver(Uuid::new_v4(), "user@example.com", "123456", 125)
        .await
        .unwrap_err();
    assert_eq!(missing.body().message, "Record not found: app");

    drop(delivery);
    connection_task.await.unwrap().unwrap();
}
