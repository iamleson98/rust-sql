//! sea-orm 2.0 RELATIONS on rustqlite: junction tables (Linked), the
//! canonical `find_also_related` LEFT-JOIN pattern, `find_with_related`
//! grouped loading, and the paginator (COUNT + LIMIT/OFFSET) — the SQL
//! shapes every real sea-orm app generates.

pub mod cake {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "cake")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        #[sea_orm(has_many, via = "cake_baker")]
        pub bakers: HasMany<super::baker::Entity>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod baker {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "baker")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        #[sea_orm(has_many, via = "cake_baker")]
        pub cakes: HasMany<super::cake::Entity>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod cake_baker {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "cake_baker")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub cake_id: i32,
        #[sea_orm(primary_key, auto_increment = false)]
        pub baker_id: i32,
        #[sea_orm(belongs_to, from = "cake_id", to = "id")]
        pub cake: HasOne<super::cake::Entity>,
        #[sea_orm(belongs_to, from = "baker_id", to = "id")]
        pub baker: HasOne<super::baker::Entity>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

use sea_orm::sea_query::SqliteQueryBuilder;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DbBackend, EntityTrait, Linked,
    PaginatorTrait, QueryFilter, QueryOrder, RelationTrait, Schema, Set,
};

/// Junction-table loader (sea-orm "Linked") — the canonical many-to-many.
pub struct CakeToBaker;

impl Linked for CakeToBaker {
    type FromEntity = cake::Entity;
    type ToEntity = baker::Entity;

    fn link(&self) -> Vec<sea_orm::RelationDef> {
        vec![
            cake_baker::Relation::Cake.def().rev(),
            cake_baker::Relation::Baker.def(),
        ]
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!("rustqlite-seaorm-rel-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}?mode=rwc", path.display());

    let db = Database::connect(&url).await?;
    println!("connected");

    // Schema from entities (composite PK junction table included).
    let schema = Schema::new(DbBackend::Sqlite);
    for stmt in [
        schema.create_table_from_entity(cake::Entity),
        schema.create_table_from_entity(baker::Entity),
        schema.create_table_from_entity(cake_baker::Entity),
    ] {
        let sql = stmt.to_string(SqliteQueryBuilder);
        println!("  {}", sql);
        db.execute_unprepared(&sql).await?;
    }

    // Seed: 3 cakes, 2 bakers, links 1<->10, 1<->11, 2<->10.
    for (id, name) in [(1, "Cheesecake"), (2, "Brownie"), (3, "Tart")] {
        cake::ActiveModel { id: Set(id), name: Set(name.into()) }
            .insert(&db)
            .await?;
    }
    for (id, name) in [(10, "Remy"), (11, "Joon")] {
        baker::ActiveModel { id: Set(id), name: Set(name.into()) }
            .insert(&db)
            .await?;
    }
    for (cake_id, baker_id) in [(1, 10), (1, 11), (2, 10)] {
        cake_baker::ActiveModel {
            cake_id: Set(cake_id),
            baker_id: Set(baker_id),
        }
        .insert(&db)
        .await?;
    }
    println!("seeded");

    // 1. find_also_related — LEFT JOIN + nested select, NULL for no-link.
    let rows: Vec<(cake::Model, Option<baker::Model>)> =
        cake::Entity::find()
            .find_also_related(baker::Entity)
            .order_by_asc(cake::Column::Id)
            .all(&db)
            .await?;
    // SQLite LEFT JOIN with multiple matches: row per link + NULL row for Tart.
    let pairs: Vec<(i32, Option<String>)> = rows
        .into_iter()
        .map(|(c, b)| (c.id, b.map(|m| m.name)))
        .collect();
    println!("find_also_related: {:?}", pairs);
    assert_eq!(pairs.len(), 4, "3 links + 1 unlinked cake");
    assert!(pairs.contains(&(1, Some("Remy".into()))));
    assert!(pairs.contains(&(1, Some("Joon".into()))));
    assert!(pairs.contains(&(2, Some("Remy".into()))));
    assert!(pairs.contains(&(3, None)));
    println!("== find_also_related ok ==");

    // 2. Linked junction loader (many-to-many) — INNER JOIN chain
    //    baker -> cake_baker -> cake: one row per junction row.
    let linked: Vec<baker::Model> = CakeToBaker.find_linked().all(&db).await?;
    let linked_names: Vec<&str> = linked.iter().map(|b| b.name.as_str()).collect();
    println!("linked entities: {:?}", linked_names);
    assert_eq!(linked_names.len(), 3, "one row per junction row");
    assert!(linked_names.contains(&"Remy"));
    assert!(linked_names.contains(&"Joon"));
    println!("== linked junction ok ==");

    // 3. find_with_related (grouped, correlated IN subquery).
    let with_related: Vec<(cake::Model, Vec<baker::Model>)> = cake::Entity::find()
        .find_with_related(baker::Entity)
        .order_by_asc(cake::Column::Id)
        .all(&db)
        .await?;
    for (c, bakers) in &with_related {
        let names: Vec<&str> = bakers.iter().map(|b| b.name.as_str()).collect();
        println!("cake {} -> {:?}", c.name, names);
    }
    assert_eq!(with_related.len(), 3);
    assert_eq!(with_related[0].1.len(), 2, "Cheesecake has 2 bakers");
    assert_eq!(with_related[2].1.len(), 0, "Tart has none");
    println!("== find_with_related ok ==");

    // 4. Paginator: COUNT(*) + LIMIT/OFFSET pages.
    let paginator = cake::Entity::find()
        .filter(cake::Column::Name.contains("e"))
        .paginate(&db, 2);
    let total = paginator.num_items().await?;
    let total_pages = paginator.num_pages().await?;
    println!("paginator: {total} items, {total_pages} pages");
    assert_eq!(total, 2, "Cheesecake + Brownie contain 'e'");
    let page1: Vec<cake::Model> = paginator.fetch_page(0).await?;
    assert_eq!(page1.len(), 2);
    let page2: Vec<cake::Model> = paginator.fetch_page(1).await?;
    assert!(page2.is_empty(), "no items past the end");
    println!("== paginator ok ==");

    // 5. COUNT via junction (aggregate over many-to-many).
    let count = cake::Entity::find()
        .find_also_related(baker::Entity)
        .filter(baker::Column::Name.eq("Remy"))
        .count(&db)
        .await?;
    assert_eq!(count, 2, "Remy is linked to 2 cakes");
    println!("== relation count ok ==");

    let _ = std::fs::remove_file(&path);
    println!("\nALL SEA-ORM RELATION TESTS PASSED");
    Ok(())
}
