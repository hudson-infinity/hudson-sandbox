//! Apply the second migration over real first-version data without inventing observations.
#![allow(clippy::unwrap_used)]
use sandbox_store::MIGRATOR;
use sqlx::{PgPool, migrate::Migrator};
use std::borrow::Cow;

#[sqlx::test(migrations = false)]
async fn observation_source_upgrade_preserves_existing_rows(pool: PgPool) {
    let initial = Migrator {
        migrations: Cow::Owned(vec![MIGRATOR.iter().next().unwrap().clone()]),
        ..Migrator::DEFAULT
    };
    initial.run(&pool).await.unwrap();
    let project = uuid::Uuid::now_v7();
    let sandbox = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO projects(id,name,status,limits) VALUES($1,'upgrade','active','{}')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sandboxes(id,project_id,image_digest,resources,desired_state,observed_state) VALUES($1,$2,'sha256:old','{}','running','creating')")
        .bind(sandbox).bind(project).execute(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let (state, source): (String, Option<bool>) =
        sqlx::query_as("SELECT observed_state,observation_simulated FROM sandboxes WHERE id=$1")
            .bind(sandbox)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "creating");
    assert_eq!(source, None);
}
