//! Interactive CLI shell for rustqlite.
//!
//! Usage:
//!   rustqlite-cli [DB_PATH]
//!
//! If DB_PATH is not given, opens an in-memory database.
//! Reads SQL statements from stdin (one per line, terminated by `;`).
//! Special commands:
//!   .help            Show help.
//!   .tables          List all tables.
//!   .schema [name]   Show schema for a table (or all tables).
//!   .quit / .exit    Exit the shell.
//!   .mode json       Switch output mode to JSON.
//!   .mode table      Switch output mode to ASCII table.

use rustqlite::{Database, Value};
use std::env;
use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = if args.len() >= 2 {
        args[1].clone()
    } else {
        ":memory:".to_string()
    };

    let mut db = if path == ":memory:" {
        match Database::open_in_memory() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error opening in-memory database: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match Database::open(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error opening {}: {}", path, e);
                std::process::exit(1);
            }
        }
    };

    println!("rustqlite v{} (connected to {})", rustqlite::VERSION, path);
    println!("Type .help for help, .quit to exit.");

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut buffer = String::new();
    let mut mode = OutputMode::Table;

    loop {
        write!(stdout, "{}> ", if buffer.is_empty() { "rustqlite" } else { "  ..." }).unwrap();
        stdout.flush().unwrap();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let line = line.trim();
        if buffer.is_empty() && line.starts_with('.') {
            // Dot command
            if let Err(e) = handle_dot_command(&mut db, line, &mut mode, &mut stdout) {
                eprintln!("error: {}", e);
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        buffer.push_str(line);
        buffer.push('\n');
        if buffer.trim_end().ends_with(';') {
            let sql = buffer.trim().to_string();
            buffer.clear();
            match execute_sql(&mut db, &sql, &mode, &mut stdout) {
                Ok(_) => {}
                Err(e) => eprintln!("error: {}", e),
            }
        }
    }
    println!();
}

fn handle_dot_command(
    db: &mut Database,
    line: &str,
    mode: &mut OutputMode,
    out: &mut impl Write,
) -> Result<(), rustqlite::Error> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(());
    }
    match parts[0] {
        ".help" => {
            writeln!(out, "Commands:").unwrap();
            writeln!(out, "  .help            Show this help.").unwrap();
            writeln!(out, "  .tables          List all tables.").unwrap();
            writeln!(out, "  .schema [name]   Show schema for a table.").unwrap();
            writeln!(out, "  .mode json       Output rows as JSON.").unwrap();
            writeln!(out, "  .mode table      Output rows as ASCII table (default).").unwrap();
            writeln!(out, "  .quit / .exit    Exit the shell.").unwrap();
        }
        ".tables" => {
            // Query the schema table.
            let rows = db.query("SELECT name FROM sqlite_master WHERE type = 'table'", [])?;
            // sqlite_master doesn't exist as such in our impl; use a different approach.
            let _ = rows;
            // We don't expose the catalog directly. As a workaround, query a
            // synthetic table — but since we don't have one, just print a message.
            writeln!(out, "(schema introspection not exposed via SQL yet)").unwrap();
        }
        ".schema" => {
            writeln!(out, "(schema introspection not exposed via SQL yet)").unwrap();
        }
        ".mode" => {
            if parts.len() >= 2 {
                *mode = match parts[1] {
                    "json" => OutputMode::Json,
                    "table" => OutputMode::Table,
                    "csv" => OutputMode::Csv,
                    "line" => OutputMode::Line,
                    _ => {
                        writeln!(out, "unknown mode: {}", parts[1]).unwrap();
                        return Ok(());
                    }
                };
            } else {
                writeln!(out, "current mode: {:?}", mode).unwrap();
            }
        }
        ".quit" | ".exit" => {
            std::process::exit(0);
        }
        _ => {
            writeln!(out, "unknown command: {}", parts[0]).unwrap();
        }
    }
    Ok(())
}

fn execute_sql(db: &mut Database, sql: &str, mode: &OutputMode, out: &mut impl Write) -> Result<(), rustqlite::Error> {
    // Determine if this is a query or a DML/DDL statement. We do this by
    // looking at the leading keyword.
    let trimmed = sql.trim_start();
    let is_query = trimmed.to_uppercase().starts_with("SELECT")
        || trimmed.to_uppercase().starts_with("WITH")
        || trimmed.to_uppercase().starts_with("VALUES")
        || trimmed.to_uppercase().starts_with("EXPLAIN")
        || trimmed.to_uppercase().starts_with("PRAGMA");

    if is_query {
        let (cols, rows) = db.query_with_columns(sql, [])?;
        print_rows(out, &cols, &rows, mode);
    } else {
        db.execute(sql, [])?;
        writeln!(out, "OK").unwrap();
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum OutputMode {
    Table,
    Json,
    Csv,
    Line,
}

fn print_rows(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>], mode: &OutputMode) {
    if rows.is_empty() {
        writeln!(out, "(no rows)").unwrap();
        return;
    }
    match mode {
        OutputMode::Table => print_table(out, cols, rows),
        OutputMode::Json => print_json(out, cols, rows),
        OutputMode::Csv => print_csv(out, cols, rows),
        OutputMode::Line => print_line(out, cols, rows),
    }
}

fn print_table(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    // Compute column widths.
    let mut widths: Vec<usize> = cols.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            let len = format_value(v).len();
            if i < widths.len() && len > widths[i] {
                widths[i] = len;
            }
        }
    }
    // Header
    let header: Vec<String> = cols.iter().enumerate().map(|(i, c)| format!("{:width$}", c, width = widths[i])).collect();
    writeln!(out, "| {} |", header.join(" | ")).unwrap();
    // Separator
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    writeln!(out, "|-{}-|", sep.join("-|-")).unwrap();
    // Rows
    for row in rows {
        let cells: Vec<String> = row.iter().enumerate().map(|(i, v)| {
            let s = format_value(v);
            format!("{:width$}", s, width = widths.get(i).copied().unwrap_or(0))
        }).collect();
        writeln!(out, "| {} |", cells.join(" | ")).unwrap();
    }
    writeln!(out, "({} rows)", rows.len()).unwrap();
}

fn print_json(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    writeln!(out, "[").unwrap();
    for (i, row) in rows.iter().enumerate() {
        write!(out, "  {{").unwrap();
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                write!(out, ", ").unwrap();
            }
            let col_name = cols.get(j).map(|s| s.as_str()).unwrap_or("?");
            write!(out, "{:?}: {}", col_name, json_value(v)).unwrap();
        }
        write!(out, "}}").unwrap();
        if i < rows.len() - 1 {
            write!(out, ",").unwrap();
        }
        writeln!(out).unwrap();
    }
    writeln!(out, "]").unwrap();
}

fn print_csv(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    let _ = cols;
    for row in rows {
        let cells: Vec<String> = row.iter().map(format_value).collect();
        writeln!(out, "{}", cells.join(",")).unwrap();
    }
}

fn print_line(out: &mut impl Write, cols: &[String], rows: &[Vec<Value>]) {
    let max_col_len = cols.iter().map(|c| c.len()).max().unwrap_or(0);
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            let name = cols.get(i).map(|s| s.as_str()).unwrap_or("?");
            writeln!(out, "{:>width$} = {}", name, format_value(v), width = max_col_len).unwrap();
        }
        writeln!(out).unwrap();
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "".to_string(),
        _ => format!("{}", v),
    }
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => format!("{:?}", s),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("\"{}\"", hex)
        }
    }
}
