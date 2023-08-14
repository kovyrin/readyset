use database_utils::QueryableConnection;
use serial_test::serial;

use crate::*;

fn readyset_postgres(name: &str) -> DeploymentBuilder {
    DeploymentBuilder::new(DatabaseType::PostgreSQL, name)
        .standalone()
        .deploy_upstream()
        .deploy_adapter()
}

fn readyset_postgres_cleanup(name: &str) -> DeploymentBuilder {
    DeploymentBuilder::new(DatabaseType::PostgreSQL, name)
        .standalone()
        .cleanup()
        .deploy_upstream()
        .deploy_adapter()
}

async fn replication_slot_exists(conn: &mut DatabaseConnection) -> bool {
    const QUERY: &str = "SELECT slot_name FROM pg_replication_slots WHERE slot_name = 'readyset'";
    if let Ok(row) = match conn {
        DatabaseConnection::MySQL(_) | DatabaseConnection::Vitess(_) => return false,
        DatabaseConnection::PostgreSQL(client, _) => client.query_one(QUERY, &[]).await,
        DatabaseConnection::PostgreSQLPool(client) => client.query_one(QUERY, &[]).await,
    } {
        let value: &str = row.get(0);
        value == "readyset"
    } else {
        false
    }
}

async fn publication_exists(conn: &mut DatabaseConnection) -> bool {
    const QUERY: &str = "SELECT pubname FROM pg_publication WHERE pubname = 'readyset'";
    if let Ok(row) = match conn {
        DatabaseConnection::MySQL(_) | DatabaseConnection::Vitess(_) => return false,
        DatabaseConnection::PostgreSQL(client, _) => client.query_one(QUERY, &[]).await,
        DatabaseConnection::PostgreSQLPool(client) => client.query_one(QUERY, &[]).await,
    } {
        let value: &str = row.get(0);
        value == "readyset"
    } else {
        false
    }
}

#[clustertest]
async fn cleanup_works() {
    let deployment_name = "ct_cleanup_works";
    let mut deployment = readyset_postgres(deployment_name).start().await.unwrap();

    let mut adapter = deployment.first_adapter().await;
    adapter
        .query_drop(
            r"CREATE TABLE t1 (
        uid INT NOT NULL,
        value INT NOT NULL
    );",
        )
        .await
        .unwrap();
    adapter
        .query_drop(r"INSERT INTO t1 VALUES (1, 4);")
        .await
        .unwrap();

    // TODO: Refactor query_until_expected to support postgres. For now this is a naive way to wait
    // for replication.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut upstream = deployment.upstream().await;

    deployment.teardown().await.unwrap();

    // At this point deployment related assets should still exist. Let's check for them.
    if let DatabaseConnection::PostgreSQL(_, _) = upstream {
        assert!(replication_slot_exists(&mut upstream).await);
        assert!(publication_exists(&mut upstream).await);
    }

    // Start up in cleanup mode.
    let mut deployment = readyset_postgres_cleanup(deployment_name)
        .start_without_waiting()
        .await
        .unwrap();

    let mut upstream = deployment.upstream().await;

    // Wait for adapters to die naturally, which should happen when cleanup finishes.
    deployment.wait_for_adapter_death().await;

    assert!(!replication_slot_exists(&mut upstream).await);
    assert!(!publication_exists(&mut upstream).await);
}
