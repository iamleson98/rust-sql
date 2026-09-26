fn main() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = rustqlite::Database::open(dir.path().join("x.db")).unwrap();
    db.execute("CREATE TABLE dropme (v TEXT)", []).unwrap();
    {
        let mut sql = String::from("INSERT INTO dropme VALUES ");
        for i in 0..400i64 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("('row number {} padding padding padding')", i));
        }
        db.execute(&sql, []).unwrap();
    }
    db.execute("CREATE TABLE keep (v)", []).unwrap();
    db.execute("INSERT INTO keep VALUES ('k')", []).unwrap();
    db.execute("DROP TABLE dropme", []).unwrap();
    for p in ["PRAGMA page_count", "PRAGMA freelist_count"] {
        println!("{} = {:?}", p, db.query(p, []).unwrap()[0][0].as_integer());
    }
    let rows = db.query("SELECT pgno, descr FROM sqlite_dbdata('main') WHERE descr LIKE '%freelist%' OR descr LIKE '%zeroed%'", []).unwrap();
    for r in &rows {
        println!("{:?}", r);
    }
    let any = db
        .query("SELECT descr FROM sqlite_dbdata('main')", [])
        .unwrap();
    let mut types: Vec<String> = any.iter().map(|r| r[0].as_text()).collect();
    types.sort();
    types.dedup();
    println!(
        "all page-level descrs: {:?}",
        types.iter().take(20).collect::<Vec<_>>()
    );
}
