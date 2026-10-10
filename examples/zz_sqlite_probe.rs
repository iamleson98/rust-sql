// TEMPORARY local probe (not committed).
fn main() {
    let sql = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let c = rusqlite::Connection::open_in_memory().unwrap();
    for stmt in sql.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
        let mut st = match c.prepare(stmt) {
            Ok(s) => s,
            Err(e) => {
                println!("{}\n  PREPARE ERR {e}", &stmt[..stmt.len().min(110)]);
                continue;
            }
        };
        let n = st.column_count();
        let mut rows = st.query([]).unwrap();
        let mut out = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(r)) => out.push(
                    (0..n)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                Ok(None) => break,
                Err(e) => {
                    out.push(format!("ERR {e}"));
                    break;
                }
            }
        }
        if n > 0 {
            println!("{}\n  {}", &stmt[..stmt.len().min(110)], out.join("\n  "));
        }
    }
}
