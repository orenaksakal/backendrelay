//! Count protocol frame types only; SQL, bindings and returned data are not recorded.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::types::{Json, Type};
use tokio_postgres::{Config, NoTls};
use uuid::Uuid;

#[derive(Default)]
struct Frames {
    parse: AtomicUsize,
    execute: AtomicUsize,
    sync: AtomicUsize,
}

impl Frames {
    fn snapshot(&self) -> [usize; 3] {
        [&self.parse, &self.execute, &self.sync].map(|value| value.load(Ordering::SeqCst))
    }
}

async fn count_frontend(
    mut source: tokio::net::tcp::OwnedReadHalf,
    mut target: tokio::net::tcp::OwnedWriteHalf,
    frames: Arc<Frames>,
) -> std::io::Result<()> {
    // SSL is disabled on this disposable loopback connection. Forward startup
    // without interpreting its contents, then retain only P/E/S frame counts.
    let mut length = [0; 4];
    source.read_exact(&mut length).await?;
    target.write_all(&length).await?;
    let size = u32::from_be_bytes(length);
    assert!((8..=16 * 1024 * 1024).contains(&size));
    tokio::io::copy(&mut (&mut source).take(u64::from(size - 4)), &mut target).await?;
    loop {
        let mut header = [0; 5];
        match source.read_exact(&mut header).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
        let size = u32::from_be_bytes(header[1..].try_into().unwrap());
        assert!((4..=16 * 1024 * 1024).contains(&size));
        match header[0] {
            b'P' => {
                frames.parse.fetch_add(1, Ordering::SeqCst);
            }
            b'E' => {
                frames.execute.fetch_add(1, Ordering::SeqCst);
            }
            b'S' => {
                frames.sync.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        target.write_all(&header).await?;
        tokio::io::copy(&mut (&mut source).take(u64::from(size - 4)), &mut target).await?;
    }
    target.shutdown().await
}

#[tokio::test]
async fn built_in_write_bindings_preserve_values_errors_and_use_one_protocol_sync() {
    let Some(url) = instant_test_support::test_database_url(
        "built_in_write_bindings_preserve_values_errors_and_use_one_protocol_sync",
    ) else {
        return;
    };
    let original: Config = url.parse().unwrap();
    let host = match &original.get_hosts()[0] {
        Host::Tcp(host) => host.clone(),
        Host::Unix(_) => panic!("the acceptance harness must provide loopback TCP"),
    };
    assert_eq!(host, "127.0.0.1", "never proxy a non-disposable database");
    let postgres = (host, original.get_ports()[0]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let frames = Arc::new(Frames::default());
    let counted = frames.clone();
    let proxy = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        let upstream = TcpStream::connect(postgres).await.unwrap();
        upstream.set_nodelay(true).unwrap();
        let (front_read, mut front_write) = socket.into_split();
        let (mut back_read, back_write) = upstream.into_split();
        let (sent, received) = tokio::join!(
            count_frontend(front_read, back_write, counted),
            tokio::io::copy(&mut back_read, &mut front_write),
        );
        sent.unwrap();
        received.unwrap();
    });
    // Recreate the harness's connection options, never copy credentials to files.
    let mut config = Config::new();
    config
        .host("127.0.0.1")
        .port(port)
        .ssl_mode(SslMode::Disable);
    if let Some(user) = original.get_user() {
        config.user(user);
    }
    if let Some(database) = original.get_dbname() {
        config.dbname(database);
    }
    if let Some(password) = original.get_password() {
        config.password(password);
    }
    let (client, connection) = config.connect(NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    client.batch_execute("CREATE TEMP TABLE typed_write_protocol_probe(app_id uuid,payload jsonb,flag boolean CHECK(flag),note text)").await.unwrap();
    let sql = "INSERT INTO typed_write_protocol_probe(app_id,payload,flag,note) VALUES($1,$2,$3,$4) RETURNING app_id,payload,flag,note";
    let id = Uuid::new_v4();
    let payload = json!({"complete":[null,true,42,"apostrophe's value"],"fresh":"one"});
    let note = "bound text; never SQL";
    let before = frames.snapshot();
    let legacy = client
        .query_one(sql, &[&id, &Json(&payload), &true, &note])
        .await
        .unwrap();
    client.simple_query("SELECT 1").await.unwrap();
    let after = frames.snapshot();
    let legacy_frames = std::array::from_fn::<_, 3, _>(|n| after[n] - before[n]);
    let before = frames.snapshot();
    let typed = client
        .query_typed_one(
            sql,
            &[
                (&id, Type::UUID),
                (&Json(&payload), Type::JSONB),
                (&true, Type::BOOL),
                (&note, Type::TEXT),
            ],
        )
        .await
        .unwrap();
    client.simple_query("SELECT 1").await.unwrap();
    let after = frames.snapshot();
    let typed_frames = std::array::from_fn::<_, 3, _>(|n| after[n] - before[n]);
    assert_eq!(legacy.get::<_, Uuid>(0), typed.get::<_, Uuid>(0));
    assert_eq!(
        legacy.get::<_, Json<serde_json::Value>>(1).0,
        typed.get::<_, Json<serde_json::Value>>(1).0
    );
    assert_eq!(legacy.get::<_, bool>(2), typed.get::<_, bool>(2));
    assert_eq!(legacy.get::<_, String>(3), typed.get::<_, String>(3));
    assert_eq!(legacy_frames[..2], [1, 1]);
    assert_eq!(
        typed_frames,
        [1, 1, 1],
        "typed execution must still parse/execute freshly, in one exchange"
    );
    assert!(
        legacy_frames[2] >= 2,
        "legacy path did not expose its prepare/execute exchanges"
    );
    let old_error = client
        .query_one(sql, &[&id, &Json(&payload), &false, &note])
        .await
        .unwrap_err();
    let new_error = client
        .query_typed_one(
            sql,
            &[
                (&id, Type::UUID),
                (&Json(&payload), Type::JSONB),
                (&false, Type::BOOL),
                (&note, Type::TEXT),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(old_error.code(), new_error.code());
    assert_eq!(
        new_error.code(),
        Some(&tokio_postgres::error::SqlState::CHECK_VIOLATION)
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM typed_write_protocol_probe", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    drop(client);
    driver.await.unwrap().unwrap();
    proxy.await.unwrap();
}
