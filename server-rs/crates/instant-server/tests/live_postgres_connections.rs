use instant_server::PostgresConnectionDrivers;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn connection_owner_polls_every_client_and_stops_on_process_cancellation() {
    let Some(database_url) = instant_test_support::test_database_url(
        "connection_owner_polls_every_client_and_stops_on_process_cancellation",
    ) else {
        return;
    };
    let (clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 2)
        .await
        .expect("open independent PostgreSQL clients");

    for client in &clients {
        let answer: i32 = client
            .query_one("select 42::integer", &[])
            .await
            .expect("query through supervised driver")
            .get(0);
        assert_eq!(answer, 42);
    }

    drop(clients);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    drivers
        .supervise(cancellation)
        .await
        .expect("intentional shutdown is not a driver failure");
}
