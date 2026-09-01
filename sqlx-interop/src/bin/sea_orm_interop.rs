//! sea-orm 2.0 (unmodified, crates.io) on rustqlite via sqlx.
//!
//! Covers: entity derive, schema creation from entity, inserts (with
//! auto-generated primary keys), finds, filters, updates, deletes,
//! transactions with rollback, and constraint violations.

pub mod cake {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "cake")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        pub price: f64,
        #[sea_orm(column_type = "Text", nullable)]
        pub note: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod baker {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "baker")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(unique)]
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

use sea_orm::sea_query::SqliteQueryBuilder;
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DbBackend, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Schema, Set, Statement, TransactionTrait};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!("rustqlite-seaorm-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}?mode=rwc", path.display());

    println!("== 1. connect ==");
    let db = Database::connect(&url).await?;
    println!("connected ({:?})", db.get_database_backend());

    println!("== 2. schema creation from entity ==");
    let schema = Schema::new(DbBackend::Sqlite);
    let stmts = vec![
        schema.create_table_from_entity(cake::Entity),
        schema.create_table_from_entity(baker::Entity),
    ];
    for st in &stmts {
        let sql = st.to_string(SqliteQueryBuilder);
        println!("  {}", sql);
        db.execute_unprepared(&sql).await?;
    }

    println!("== 3. insert (auto pk) ==");
    let cheesecake = cake::ActiveModel {
        name: Set("Cheesecake".to_owned()),
        price: Set(12.5),
        note: Set(Some("rich".to_owned())),
        ..Default::default()
    };
    let inserted = cheesecake.insert(&db).await?;
    println!("inserted id={}", inserted.id);
    assert!(inserted.id > 0, "auto primary key must be generated");
    let id = inserted.id;

    for name in ["Tart", "Brownie", "Croissant"] {
        let c = cake::ActiveModel {
            name: Set(name.to_owned()),
            price: Set(4.0),
            note: Set(None),
            ..Default::default()
        };
        c.insert(&db).await?;
    }
    let count = cake::Entity::find().count(&db).await?;
    assert_eq!(count, 4);
    println!("4 cakes inserted, count={}", count);

    println!("== 4. find by pk ==");
    let found: Option<cake::Model> = cake::Entity::find_by_id(id).one(&db).await?;
    let found = found.expect("cheesecake must exist");
    assert_eq!(found.name, "Cheesecake");
    assert_eq!(found.price, 12.5);
    assert_eq!(found.note.as_deref(), Some("rich"));
    println!("found: {:?}", found);

    println!("== 5. filter + order ==");
    let cheap: Vec<cake::Model> = cake::Entity::find()
        .filter(cake::Column::Price.lt(10.0))
        .order_by_asc(cake::Column::Id)
        .all(&db)
        .await?;
    assert_eq!(cheap.len(), 3);
    assert_eq!(cheap[0].name, "Tart");
    println!("cheap cakes: {:?}", cheap.iter().map(|c| c.name.clone()).collect::<Vec<_>>());

    println!("== 6. update ==");
    let mut upd: cake::ActiveModel = found.into();
    upd.price = Set(13.75);
    let updated = upd.update(&db).await?;
    assert_eq!(updated.price, 13.75);
    println!("updated price: {}", updated.price);

    println!("== 7. delete ==");
    let dead = cake::ActiveModel {
        id: Set(cheap[2].id),
        ..Default::default()
    };
    let res = dead.delete(&db).await?;
    assert_eq!(res.rows_affected, 1);
    let count = cake::Entity::find().count(&db).await?;
    assert_eq!(count, 3);
    println!("deleted 1, count={}", count);

    println!("== 8. transaction + rollback ==");
    let tx = db.begin().await?;
    cake::ActiveModel {
        name: Set("Stollen".to_owned()),
        price: Set(20.0),
        note: Set(None),
        ..Default::default()
    }
    .insert(&tx)
    .await?;
    let in_tx = cake::Entity::find().count(&tx).await?;
    assert_eq!(in_tx, 4);
    tx.rollback().await?;
    let after = cake::Entity::find().count(&db).await?;
    assert_eq!(after, 3, "rollback must undo the insert");
    println!("rollback ok, count={}", after);

    println!("== 9. commit path ==");
    let tx = db.begin().await?;
    let b = baker::ActiveModel {
        name: Set("Remy".to_owned()),
        ..Default::default()
    }
    .insert(&tx)
    .await?;
    tx.commit().await?;
    let bakers = baker::Entity::find().all(&db).await?;
    assert_eq!(bakers.len(), 1);
    assert_eq!(bakers[0].name, "Remy");
    assert!(b.id > 0);
    println!("committed baker: {:?}", bakers);

    println!("== 10. insert error propagation ==");
    let dup = baker::ActiveModel {
        name: Set("Remy".to_owned()),
        ..Default::default()
    }
    .save(&db)
    .await;
    match dup {
        Err(e) => {
            // Must surface as a unique-violation DbErr (sqlx error mapping
            // on the SQLite-exact "UNIQUE constraint failed: baker.name").
            let msg = e.to_string();
            assert!(
                msg.contains("UNIQUE constraint failed: baker.name"),
                "expected UNIQUE violation, got: {msg}"
            );
            println!("duplicate insert error correctly propagated: {msg}");
        }
        Ok(_) => panic!("duplicate insert must fail (baker.name is UNIQUE)"),
    }

    let _ = std::fs::remove_file(&path);
    println!("\nALL SEA-ORM INTEROP TESTS PASSED");
    Ok(())
}
