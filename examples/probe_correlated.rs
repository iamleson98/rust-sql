use rustqlite::Database;

fn q(db: &mut Database, sql: &str) -> String {
    match db.query(sql, []) {
        Ok(rows) => format!("{:?}", rows),
        Err(e) => format!("ERR: {}", e),
    }
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE cakes (id INTEGER PRIMARY KEY, name TEXT)", [])
        .unwrap();
    db.execute(
        "CREATE TABLE bakers (id INTEGER PRIMARY KEY, name TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE TABLE cake_baker (cake_id INT, baker_id INT)", [])
        .unwrap();
    db.execute("INSERT INTO cakes VALUES (1,'C1'), (2,'C2'), (3,'C3')", [])
        .unwrap();
    db.execute("INSERT INTO bakers VALUES (10,'B10'), (11,'B11')", [])
        .unwrap();
    db.execute("INSERT INTO cake_baker VALUES (1,10), (2,11)", [])
        .unwrap();

    // sea-orm find_also_related generates LEFT JOIN + correlated EXISTS-style subqueries.
    println!("exists correlated:  {}", q(&mut db, "SELECT c.id FROM cakes c WHERE EXISTS (SELECT 1 FROM cake_baker cb WHERE cb.cake_id = c.id)"));
    println!("in correlated:      {}", q(&mut db, "SELECT c.id FROM cakes c WHERE c.id IN (SELECT cb.cake_id FROM cake_baker cb WHERE cb.baker_id > 10)"));
    println!("scalar correlated:  {}", q(&mut db, "SELECT c.id, (SELECT COUNT(*) FROM cake_baker cb WHERE cb.cake_id = c.id) FROM cakes c ORDER BY c.id"));
    println!("not exists:         {}", q(&mut db, "SELECT c.id FROM cakes c WHERE NOT EXISTS (SELECT 1 FROM cake_baker cb WHERE cb.cake_id = c.id)"));
    // Lateral-ish: correlated in SELECT list referencing outer alias
    println!("left join related:  {}", q(&mut db, "SELECT c.id, b.name FROM cakes c LEFT JOIN cake_baker cb ON cb.cake_id = c.id LEFT JOIN bakers b ON b.id = cb.baker_id ORDER BY c.id"));
    // Sea-orm's `.find_with_related` uses IN (subquery on outer pk):
    println!(
        "in subquery:        {}",
        q(
            &mut db,
            "SELECT * FROM cake_baker WHERE cake_id IN (SELECT id FROM cakes WHERE name LIKE 'C%')"
        )
    );
}
