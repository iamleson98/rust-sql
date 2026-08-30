//! Recursive-descent SQL parser.
//!
//! The parser takes a `Vec<SpannedToken>` and produces a `Statement`. It uses
//! precedence-climbing for binary operators and a standard recursive-descent
//! pattern for the rest. Errors are reported with line/column context.

use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::sql::lexer::{Lexer, SpannedToken, Token};
use crate::types::Value;

pub struct Parser {
    toks: Vec<SpannedToken>,
    pos: usize,
}

impl Parser {
    pub fn new(src: &str) -> Result<Self> {
        let toks = Lexer::new(src).tokenize()?;
        Ok(Self { toks, pos: 0 })
    }

    pub fn parse(&mut self) -> Result<Statement> {
        let stmt = self.parse_statement()?;
        // Allow trailing semicolon.
        if self.peek().is_punct(';') {
            self.advance();
        }
        // Anything after is an error.
        if !matches!(self.peek().token, Token::Eof) {
            let t = self.peek();
            return Err(Error::parse(t.line, t.col, format!("unexpected token after statement: {:?}", t.token)));
        }
        Ok(stmt)
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        let t = self.peek();
        if t.is_keyword("EXPLAIN") {
            self.advance();
            if self.peek().is_keyword("QUERY") {
                self.advance();
                self.expect_keyword("PLAN")?;
            }
            let inner = self.parse_statement()?;
            return Ok(Statement::Explain(Box::new(inner)));
        }
        if t.is_keyword("WITH") {
            // CTEs are part of SELECT/INSERT/UPDATE/DELETE
            return self.parse_with_statement();
        }
        match &t.token {
            Token::Keyword(k) => match *k {
                "CREATE" => self.parse_create(),
                "DROP" => self.parse_drop(),
                "INSERT" | "REPLACE" => self.parse_insert(),
                "SELECT" | "VALUES" => Ok(Statement::Select(self.parse_select()?)),
                "UPDATE" => self.parse_update(),
                "DELETE" => self.parse_delete(),
                "BEGIN" => self.parse_begin(),
                "COMMIT" | "END" => {
                    self.advance();
                    self.expect_keyword("TRANSACTION").ok();
                    Ok(Statement::Commit)
                }
                "ROLLBACK" => {
                    self.advance();
                    self.expect_keyword("TRANSACTION").ok();
                    let savepoint = if self.peek().is_keyword("TO") {
                        self.advance();
                        self.expect_keyword("SAVEPOINT").ok();
                        Some(self.parse_ident()?)
                    } else {
                        None
                    };
                    Ok(Statement::Rollback(RollbackStatement { savepoint }))
                }
                "SAVEPOINT" => {
                    self.advance();
                    let name = self.parse_ident()?;
                    Ok(Statement::Savepoint(name))
                }
                "RELEASE" => {
                    // RELEASE [SAVEPOINT] name — discards the savepoint (and
                    // everything above it) without rolling back.
                    self.advance();
                    if self.peek().is_keyword("SAVEPOINT") {
                        self.advance();
                    }
                    let name = self.parse_ident()?;
                    Ok(Statement::Release(name))
                }
                "PRAGMA" => self.parse_pragma(),
                "ALTER" => self.parse_alter(),
                "ATTACH" => self.parse_attach(),
                "DETACH" => self.parse_detach(),
                "VACUUM" => self.parse_vacuum(),
                _ => Err(Error::parse(t.line, t.col, format!("unexpected keyword: {}", k))),
            },
            _ => Err(Error::parse(t.line, t.col, format!("unexpected token: {:?}", t.token))),
        }
    }

    fn parse_with_statement(&mut self) -> Result<Statement> {
        let with = self.parse_with_clause()?;
        // A WITH can prefix SELECT, INSERT, UPDATE, or DELETE.
        let t = self.peek();
        match &t.token {
            Token::Keyword(k) => match *k {
                "SELECT" | "VALUES" => {
                    let mut sel = self.parse_select()?;
                    sel.with = Some(with);
                    Ok(Statement::Select(sel))
                }
                "INSERT" | "REPLACE" => {
                    let ins = self.parse_insert_inner()?;
                    // insert.with is not modeled separately; attach to select if any
                    let _ = ins;
                    unreachable!("WITH ... INSERT not yet supported")
                }
                "UPDATE" => {
                    let upd = self.parse_update()?;
                    let _ = upd;
                    unreachable!("WITH ... UPDATE not yet supported")
                }
                "DELETE" => {
                    let del = self.parse_delete()?;
                    let _ = del;
                    unreachable!("WITH ... DELETE not yet supported")
                }
                _ => Err(Error::parse(t.line, t.col, format!("expected SELECT/INSERT/UPDATE/DELETE after WITH, got {}", k))),
            },
            _ => Err(Error::parse(t.line, t.col, "expected SELECT/INSERT/UPDATE/DELETE after WITH")),
        }
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.advance(); // CREATE
        let unique = self.consume_keyword("UNIQUE");
        if unique || self.peek().is_keyword("INDEX") {
            return self.parse_create_index(unique);
        }
        if self.peek().is_keyword("TABLE") {
            return self.parse_create_table();
        }
        if self.peek().is_keyword("VIEW") {
            return self.parse_create_view();
        }
        if self.peek().is_keyword("TRIGGER") {
            return self.parse_create_trigger();
        }
        if self.peek().is_keyword("VIRTUAL") {
            // CREATE VIRTUAL TABLE — not supported, parse and reject
            return Err(Error::Unsupported("CREATE VIRTUAL TABLE"));
        }
        let t = self.peek();
        Err(Error::parse(t.line, t.col, format!("expected TABLE/INDEX/VIEW/TRIGGER after CREATE, got {:?}", t.token)))
    }

    fn parse_create_table(&mut self) -> Result<Statement> {
        self.expect_keyword("TABLE")?;
        let if_not_exists = if self.peek().is_keyword("IF") {
            self.advance();
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.parse_table_name()?;
        self.expect_punct('(')?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.peek().is_keyword("PRIMARY")
                || self.peek().is_keyword("UNIQUE")
                || self.peek().is_keyword("CHECK")
                || self.peek().is_keyword("FOREIGN")
                || self.peek().is_keyword("CONSTRAINT")
            {
                constraints.push(self.parse_table_constraint()?);
            } else {
                columns.push(self.parse_column_def()?);
            }
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_punct(')')?;
        let mut without_rowid = false;
        let mut strict = false;
        while !matches!(self.peek().token, Token::Eof | Token::Punct(';')) {
            if self.peek().is_keyword("WITHOUT") {
                self.advance();
                let id = self.parse_ident()?;
                if id.eq_ignore_ascii_case("ROWID") {
                    without_rowid = true;
                }
            } else if self.peek().is_keyword("STRICT") {
                self.advance();
                strict = true;
            } else {
                break;
            }
            if self.peek().is_punct(',') {
                self.advance();
            }
        }
        Ok(Statement::Create(CreateStatement::Table {
            if_not_exists,
            name,
            columns,
            constraints,
            without_rowid,
            strict,
        }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.parse_ident()?;
        let mut type_name = String::new();
        // Type name: 0 or more identifier tokens (some types take args like VARCHAR(10))
        if !self.is_column_constraint_start() && !self.peek().is_punct(',') && !self.peek().is_punct(')') {
            let parts: Vec<String> = self.parse_type_name_parts()?;
            type_name = parts.join(" ");
        }
        let mut constraints = Vec::new();
        while self.is_column_constraint_start() {
            constraints.push(self.parse_column_constraint()?);
        }
        Ok(ColumnDef { name, type_name, constraints })
    }

    fn parse_type_name_parts(&mut self) -> Result<Vec<String>> {
        let mut parts = Vec::new();
        // First token must be an identifier.
        if let Token::Ident(s) = &self.peek().token {
            parts.push(s.clone());
            self.advance();
        } else {
            return Ok(parts);
        }
        // Optional `(n)` or `(n, m)` after the type.
        if self.peek().is_punct('(') {
            self.advance();
            // Skip until matching close paren.
            let mut depth = 1;
            while depth > 0 && !matches!(self.peek().token, Token::Eof) {
                if self.peek().is_punct('(') {
                    depth += 1;
                } else if self.peek().is_punct(')') {
                    depth -= 1;
                }
                if depth > 0 {
                    self.advance();
                }
            }
            if self.peek().is_punct(')') {
                self.advance();
            }
        }
        // Additional type name parts (e.g. "DOUBLE PRECISION", "UNSIGNED BIG INT")
        while let Token::Ident(s) = &self.peek().token {
            // Stop at constraint keywords.
            if is_constraint_keyword(s) {
                break;
            }
            parts.push(s.clone());
            self.advance();
        }
        Ok(parts)
    }

    fn is_column_constraint_start(&self) -> bool {
        match &self.peek().token {
            Token::Keyword(k) => matches!(
                *k,
                "PRIMARY" | "NOT" | "NULL" | "UNIQUE" | "CHECK" | "DEFAULT"
                    | "COLLATE" | "REFERENCES" | "GENERATED" | "AS" | "CONSTRAINT"
            ),
            _ => false,
        }
    }

    fn parse_column_constraint(&mut self) -> Result<ColumnConstraint> {
        // Skip optional CONSTRAINT name
        if self.peek().is_keyword("CONSTRAINT") {
            self.advance();
            let _ = self.parse_ident()?;
        }
        let t = self.peek();
        if t.is_keyword("PRIMARY") {
            self.advance();
            self.expect_keyword("KEY")?;
            let mut order = Order::Asc;
            if self.peek().is_keyword("ASC") {
                self.advance();
            } else if self.peek().is_keyword("DESC") {
                order = Order::Desc;
                self.advance();
            }
            let autoincrement = if self.peek().is_keyword("AUTOINCREMENT") {
                self.advance();
                true
            } else {
                false
            };
            Ok(ColumnConstraint::PrimaryKey { autoincrement, order })
        } else if t.is_keyword("NOT") {
            self.advance();
            self.expect_keyword("NULL")?;
            Ok(ColumnConstraint::NotNull)
        } else if t.is_keyword("NULL") {
            self.advance();
            Ok(ColumnConstraint::Null)
        } else if t.is_keyword("UNIQUE") {
            self.advance();
            Ok(ColumnConstraint::Unique)
        } else if t.is_keyword("CHECK") {
            self.advance();
            self.expect_punct('(')?;
            let e = self.parse_expr()?;
            self.expect_punct(')')?;
            Ok(ColumnConstraint::Check(e))
        } else if t.is_keyword("DEFAULT") {
            self.advance();
            let e = self.parse_primary_expr()?;
            Ok(ColumnConstraint::Default(e))
        } else if t.is_keyword("COLLATE") {
            self.advance();
            let s = self.parse_ident()?;
            Ok(ColumnConstraint::Collate(s))
        } else if t.is_keyword("REFERENCES") {
            self.advance();
            let table = self.parse_ident()?;
            let mut columns = Vec::new();
            if self.peek().is_punct('(') {
                self.advance();
                loop {
                    columns.push(self.parse_ident()?);
                    if self.peek().is_punct(',') {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect_punct(')')?;
            }
            let on_delete = self.parse_fk_action("ON DELETE")?;
            let on_update = self.parse_fk_action("ON UPDATE")?;
            Ok(ColumnConstraint::References {
                table,
                columns,
                on_delete,
                on_update,
            })
        } else if t.is_keyword("GENERATED") {
            self.advance();
            self.expect_keyword("ALWAYS")?;
            self.expect_keyword("AS")?;
            self.expect_punct('(')?;
            let expr = self.parse_expr()?;
            self.expect_punct(')')?;
            let stored = if self.peek().is_keyword("STORED") {
                self.advance();
                true
            } else if self.peek().is_keyword("VIRTUAL") {
                self.advance();
                false
            } else {
                false
            };
            Ok(ColumnConstraint::GeneratedAs { expr, stored })
        } else if t.is_keyword("AS") {
            // Column alias AS expr (without GENERATED)
            self.advance();
            self.expect_punct('(')?;
            let expr = self.parse_expr()?;
            self.expect_punct(')')?;
            Ok(ColumnConstraint::GeneratedAs { expr, stored: false })
        } else {
            Err(Error::parse(t.line, t.col, format!("expected column constraint, got {:?}", t.token)))
        }
    }

    fn parse_fk_action(&mut self, prefix: &str) -> Result<ForeignKeyAction> {
        let action = if self.peek().is_keyword("ON") {
            self.advance();
            // Next should be DELETE or UPDATE
            if self.peek().is_keyword("DELETE") {
                self.advance();
            } else if self.peek().is_keyword("UPDATE") {
                self.advance();
            } else {
                let t = self.peek();
                return Err(Error::parse(t.line, t.col, format!("expected DELETE or UPDATE after ON, got {:?}", t.token)));
            }
            if self.peek().is_keyword("NO") {
                self.advance();
                self.expect_keyword("ACTION")?;
                ForeignKeyAction::NoAction
            } else if self.peek().is_keyword("RESTRICT") {
                self.advance();
                ForeignKeyAction::Restrict
            } else if self.peek().is_keyword("SET") {
                self.advance();
                if self.peek().is_keyword("NULL") {
                    self.advance();
                    ForeignKeyAction::SetNull
                } else {
                    self.expect_keyword("DEFAULT")?;
                    ForeignKeyAction::SetDefault
                }
            } else if self.peek().is_keyword("CASCADE") {
                self.advance();
                ForeignKeyAction::Cascade
            } else {
                let t = self.peek();
                return Err(Error::parse(t.line, t.col, format!("expected FK action, got {:?}", t.token)));
            }
        } else {
            ForeignKeyAction::NoAction
        };
        let _ = prefix;
        Ok(action)
    }

    fn parse_table_constraint(&mut self) -> Result<TableConstraint> {
        if self.peek().is_keyword("CONSTRAINT") {
            self.advance();
            let _ = self.parse_ident()?;
        }
        if self.peek().is_keyword("PRIMARY") {
            self.advance();
            self.expect_keyword("KEY")?;
            self.expect_punct('(')?;
            let cols = self.parse_indexed_columns()?;
            self.expect_punct(')')?;
            Ok(TableConstraint::PrimaryKey { columns: cols })
        } else if self.peek().is_keyword("UNIQUE") {
            self.advance();
            self.expect_punct('(')?;
            let cols = self.parse_indexed_columns()?;
            self.expect_punct(')')?;
            Ok(TableConstraint::Unique(cols))
        } else if self.peek().is_keyword("CHECK") {
            self.advance();
            self.expect_punct('(')?;
            let e = self.parse_expr()?;
            self.expect_punct(')')?;
            Ok(TableConstraint::Check(e))
        } else if self.peek().is_keyword("FOREIGN") {
            self.advance();
            self.expect_keyword("KEY")?;
            self.expect_punct('(')?;
            let cols: Vec<String> = self.parse_ident_list()?;
            self.expect_punct(')')?;
            self.expect_keyword("REFERENCES")?;
            let ref_table = self.parse_ident()?;
            let mut ref_columns = Vec::new();
            if self.peek().is_punct('(') {
                self.advance();
                ref_columns = self.parse_ident_list()?;
                self.expect_punct(')')?;
            }
            let on_delete = self.parse_fk_action("ON DELETE")?;
            let on_update = self.parse_fk_action("ON UPDATE")?;
            Ok(TableConstraint::ForeignKey {
                columns: cols,
                ref_table,
                ref_columns,
                on_delete,
                on_update,
            })
        } else {
            let t = self.peek();
            Err(Error::parse(t.line, t.col, format!("expected table constraint, got {:?}", t.token)))
        }
    }

    fn parse_indexed_columns(&mut self) -> Result<Vec<IndexedColumn>> {
        let mut cols = Vec::new();
        loop {
            cols.push(self.parse_indexed_column()?);
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(cols)
    }

    fn parse_indexed_column(&mut self) -> Result<IndexedColumn> {
        let name = self.parse_ident()?;
        let mut order = Order::Asc;
        if self.peek().is_keyword("ASC") {
            self.advance();
        } else if self.peek().is_keyword("DESC") {
            order = Order::Desc;
            self.advance();
        }
        let collation = if self.peek().is_keyword("COLLATE") {
            self.advance();
            Some(self.parse_ident()?)
        } else {
            None
        };
        Ok(IndexedColumn { name, order, collation })
    }

    fn parse_ident_list(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        loop {
            out.push(self.parse_ident()?);
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<Statement> {
        self.expect_keyword("INDEX")?;
        let if_not_exists = if self.peek().is_keyword("IF") {
            self.advance();
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.parse_ident()?;
        self.expect_keyword("ON")?;
        let table = self.parse_ident()?;
        self.expect_punct('(')?;
        let columns = self.parse_indexed_columns()?;
        self.expect_punct(')')?;
        let where_clause = if self.peek().is_keyword("WHERE") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Create(CreateStatement::Index {
            unique,
            if_not_exists,
            name,
            table,
            columns,
            where_clause,
        }))
    }

    fn parse_create_view(&mut self) -> Result<Statement> {
        self.expect_keyword("VIEW")?;
        let if_not_exists = if self.peek().is_keyword("IF") {
            self.advance();
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.parse_table_name()?;
        let columns = if self.peek().is_punct('(') {
            self.advance();
            let cols = self.parse_ident_list()?;
            self.expect_punct(')')?;
            Some(cols)
        } else {
            None
        };
        self.expect_keyword("AS")?;
        let select = self.parse_select()?;
        Ok(Statement::Create(CreateStatement::View {
            if_not_exists,
            name,
            columns,
            select: Box::new(select),
        }))
    }

    fn parse_create_trigger(&mut self) -> Result<Statement> {
        self.expect_keyword("TRIGGER")?;
        let name = self.parse_ident()?;
        let when = if self.peek().is_keyword("BEFORE") {
            self.advance();
            TriggerWhen::Before
        } else if self.peek().is_keyword("AFTER") {
            self.advance();
            TriggerWhen::After
        } else if self.peek().is_keyword("INSTEAD") {
            self.advance();
            self.expect_keyword("OF")?;
            TriggerWhen::InsteadOf
        } else {
            TriggerWhen::After
        };
        let mut events = Vec::new();
        loop {
            if self.peek().is_keyword("INSERT") {
                self.advance();
                events.push(TriggerEvent::Insert);
            } else if self.peek().is_keyword("DELETE") {
                self.advance();
                events.push(TriggerEvent::Delete);
            } else if self.peek().is_keyword("UPDATE") {
                self.advance();
                if self.peek().is_keyword("OF") {
                    self.advance();
                    let cols = self.parse_ident_list()?;
                    events.push(TriggerEvent::Update(cols));
                } else {
                    events.push(TriggerEvent::Update(Vec::new()));
                }
            } else {
                break;
            }
            if self.peek().is_keyword("OR") {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_keyword("ON")?;
        let table = self.parse_ident()?;
        let for_each_row = if self.peek().is_keyword("FOR") {
            self.advance();
            self.expect_keyword("EACH")?;
            self.expect_keyword("ROW")?;
            true
        } else {
            false
        };
        let when_clause = if self.peek().is_keyword("WHEN") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_keyword("BEGIN")?;
        let mut body = Vec::new();
        while !self.peek().is_keyword("END") {
            body.push(self.parse_statement()?);
            if self.peek().is_punct(';') {
                self.advance();
            }
        }
        self.expect_keyword("END")?;
        Ok(Statement::Create(CreateStatement::Trigger(CreateTrigger {
            name,
            table,
            when,
            events,
            for_each_row,
            when_clause,
            body,
        })))
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.advance(); // DROP
        let kind = if self.peek().is_keyword("TABLE") {
            self.advance();
            DropKind::Table
        } else if self.peek().is_keyword("INDEX") {
            self.advance();
            DropKind::Index
        } else if self.peek().is_keyword("VIEW") {
            self.advance();
            DropKind::View
        } else if self.peek().is_keyword("TRIGGER") {
            self.advance();
            DropKind::Trigger
        } else {
            let t = self.peek();
            return Err(Error::parse(t.line, t.col, format!("expected TABLE/INDEX/VIEW/TRIGGER after DROP, got {:?}", t.token)));
        };
        let if_exists = if self.peek().is_keyword("IF") {
            self.advance();
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.parse_ident()?;
        Ok(Statement::Drop(DropStatement { if_exists, kind, name }))
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.parse_insert_inner()
    }

    fn parse_insert_inner(&mut self) -> Result<Statement> {
        let or = if self.peek().is_keyword("REPLACE") {
            self.advance();
            Some(ConflictResolution::Replace)
        } else {
            self.expect_keyword("INSERT")?;
            if self.peek().is_keyword("OR") {
                self.advance();
                let t = self.peek();
                let r = match &t.token {
                    Token::Keyword(k) => match *k {
                        "ROLLBACK" => ConflictResolution::Rollback,
                        "ABORT" => ConflictResolution::Abort,
                        "FAIL" => ConflictResolution::Fail,
                        "IGNORE" => ConflictResolution::Ignore,
                        "REPLACE" => ConflictResolution::Replace,
                        _ => return Err(Error::parse(t.line, t.col, format!("unknown conflict resolution: {}", k))),
                    },
                    _ => return Err(Error::parse(t.line, t.col, "expected conflict resolution keyword")),
                };
                self.advance();
                Some(r)
            } else {
                None
            }
        };
        self.expect_keyword("INTO")?;
        let table = self.parse_ident()?;
        let alias = if self.peek().is_keyword("AS") {
            self.advance();
            Some(self.parse_ident()?)
        } else {
            None
        };
        let columns = if self.peek().is_punct('(') {
            // Peek ahead: this could be a column list or a VALUES tuple.
            // We assume column list (SQLite requires it before VALUES).
            self.advance();
            let cols = self.parse_ident_list()?;
            self.expect_punct(')')?;
            Some(cols)
        } else {
            None
        };
        let source = if self.peek().is_keyword("VALUES") {
            self.advance();
            let mut rows = Vec::new();
            loop {
                self.expect_punct('(')?;
                let mut row = Vec::new();
                loop {
                    row.push(self.parse_expr()?);
                    if self.peek().is_punct(',') {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect_punct(')')?;
                rows.push(row);
                if self.peek().is_punct(',') {
                    self.advance();
                } else {
                    break;
                }
            }
            InsertSource::Values(rows)
        } else if self.peek().is_keyword("SELECT") {
            InsertSource::Select(Box::new(self.parse_select()?))
        } else if self.peek().is_keyword("DEFAULT") {
            self.advance();
            self.expect_keyword("VALUES")?;
            InsertSource::DefaultValues
        } else {
            let t = self.peek();
            return Err(Error::parse(t.line, t.col, format!("expected VALUES/SELECT/DEFAULT VALUES, got {:?}", t.token)));
        };
        let upsert = if self.peek().is_keyword("ON") {
            self.advance();
            self.expect_keyword("CONFLICT")?;
            let target = if self.peek().is_punct('(') {
                self.advance();
                let t = self.parse_indexed_columns()?;
                self.expect_punct(')')?;
                t
            } else {
                Vec::new()
            };
            let target_where = if self.peek().is_keyword("WHERE") {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect_keyword("DO")?;
            let action = if self.peek().is_keyword("NOTHING") {
                self.advance();
                UpsertAction::DoNothing
            } else {
                self.expect_keyword("UPDATE")?;
                self.expect_keyword("SET")?;
                let set = self.parse_set_clause()?;
                let where_clause = if self.peek().is_keyword("WHERE") {
                    self.advance();
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                UpsertAction::DoUpdate { set, where_clause }
            };
            Some(UpsertClause { target, target_where, action })
        } else {
            None
        };
        let returning = if self.peek().is_keyword("RETURNING") {
            self.advance();
            Some(self.parse_result_columns()?)
        } else {
            None
        };
        Ok(Statement::Insert(InsertStatement {
            or,
            table,
            alias,
            columns,
            source,
            upsert,
            returning,
        }))
    }

    fn parse_set_clause(&mut self) -> Result<Vec<(String, Expr)>> {
        let mut out = Vec::new();
        loop {
            let name = self.parse_ident()?;
            // Optional `=` after column name. SQLite also allows `(a, b) = (expr1, expr2)`.
            self.expect_op("=")?;
            let e = self.parse_expr()?;
            out.push((name, e));
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn parse_update(&mut self) -> Result<Statement> {
        self.advance(); // UPDATE
        let or = if self.peek().is_keyword("OR") {
            self.advance();
            let t = self.peek();
            let r = match &t.token {
                Token::Keyword(k) => match *k {
                    "ROLLBACK" => ConflictResolution::Rollback,
                    "ABORT" => ConflictResolution::Abort,
                    "FAIL" => ConflictResolution::Fail,
                    "IGNORE" => ConflictResolution::Ignore,
                    "REPLACE" => ConflictResolution::Replace,
                    _ => return Err(Error::parse(t.line, t.col, format!("unknown conflict resolution: {}", k))),
                },
                _ => return Err(Error::parse(t.line, t.col, "expected conflict resolution keyword")),
            };
            self.advance();
            Some(r)
        } else {
            None
        };
        let table = self.parse_ident()?;
        let alias = if self.peek().is_keyword("AS") {
            self.advance();
            Some(self.parse_ident()?)
        } else if let Token::Ident(_) = &self.peek().token {
            if !self.peek().is_keyword("SET") {
                Some(self.parse_ident()?)
            } else {
                None
            }
        } else {
            None
        };
        self.expect_keyword("SET")?;
        let set = self.parse_set_clause()?;
        let from = if self.peek().is_keyword("FROM") {
            self.advance();
            Some(self.parse_table_expression()?)
        } else {
            None
        };
        let where_clause = if self.peek().is_keyword("WHERE") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = if self.peek().is_keyword("RETURNING") {
            self.advance();
            Some(self.parse_result_columns()?)
        } else {
            None
        };
        Ok(Statement::Update(UpdateStatement {
            or,
            table,
            alias,
            set,
            from,
            where_clause,
            returning,
        }))
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.advance(); // DELETE
        self.expect_keyword("FROM")?;
        let from = self.parse_ident()?;
        let alias = if self.peek().is_keyword("AS") {
            self.advance();
            Some(self.parse_ident()?)
        } else if let Token::Ident(_) = &self.peek().token {
            if !self.peek().is_keyword("WHERE") && !self.peek().is_keyword("LIMIT")
                && !self.peek().is_keyword("ORDER") && !self.peek().is_keyword("RETURNING")
                && !self.peek().is_punct(';') && !matches!(self.peek().token, Token::Eof)
            {
                Some(self.parse_ident()?)
            } else {
                None
            }
        } else {
            None
        };
        let where_clause = if self.peek().is_keyword("WHERE") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = if self.peek().is_keyword("RETURNING") {
            self.advance();
            Some(self.parse_result_columns()?)
        } else {
            None
        };
        let order_by = if self.peek().is_keyword("ORDER") {
            self.advance();
            self.expect_keyword("BY")?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        let limit = if self.peek().is_keyword("LIMIT") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Delete(DeleteStatement {
            from,
            alias,
            where_clause,
            returning,
            limit,
            order_by,
        }))
    }

    fn parse_begin(&mut self) -> Result<Statement> {
        self.advance(); // BEGIN
        let mode = if self.peek().is_keyword("DEFERRED") {
            self.advance();
            BeginMode::Deferred
        } else if self.peek().is_keyword("IMMEDIATE") {
            self.advance();
            BeginMode::Immediate
        } else if self.peek().is_keyword("EXCLUSIVE") {
            self.advance();
            BeginMode::Exclusive
        } else {
            BeginMode::Deferred
        };
        self.expect_keyword("TRANSACTION").ok();
        Ok(Statement::Begin(BeginStatement { mode }))
    }

    fn parse_pragma(&mut self) -> Result<Statement> {
        self.advance(); // PRAGMA
        let (schema, name) = self.parse_qualified_name()?;
        let value = if self.peek().is_op("=") {
            self.advance();
            // Keyword literals (ON/OFF/TRUE/FALSE) are valid pragma values
            // — they arrive as identifier-ish keywords, not expressions.
            let v = match self.peek() {
                t if t.is_keyword("ON") => {
                    self.advance();
                    Expr::Literal(Value::Integer(1))
                }
                t if t.is_keyword("OFF") => {
                    self.advance();
                    Expr::Literal(Value::Integer(0))
                }
                t if t.is_keyword("TRUE") => {
                    self.advance();
                    Expr::Literal(Value::Integer(1))
                }
                t if t.is_keyword("FALSE") => {
                    self.advance();
                    Expr::Literal(Value::Integer(0))
                }
                // Bare keyword values (`PRAGMA journal_mode = DELETE`,
                // `= WAL`, `= NORMAL`, ...): capture the spelled keyword as
                // text — SQLite treats pragma values as plain words.
                t if keyword_text(&t.token).is_some() => {
                    let txt = keyword_text(&t.token).unwrap();
                    self.advance();
                    Expr::Literal(Value::Text(txt.into()))
                }
                _ => self.parse_primary_expr()?,
            };
            Some(PragmaValue::Expr(v))
        } else if self.peek().is_punct('(') {
            self.advance();
            let e = self.parse_expr()?;
            self.expect_punct(')')?;
            Some(PragmaValue::Call(e))
        } else {
            None
        };
        Ok(Statement::Pragma(PragmaStatement { schema, name, value }))
    }

    fn parse_alter(&mut self) -> Result<Statement> {
        self.advance(); // ALTER
        self.expect_keyword("TABLE")?;
        let table = self.parse_ident()?;
        if self.peek().is_keyword("RENAME") {
            self.advance();
            // RENAME TO x  |  RENAME COLUMN a TO b  |  RENAME a TO b
            if self.peek().is_keyword("TO") {
                self.advance();
                let new_name = self.parse_ident()?;
                return Ok(Statement::Alter(AlterStatement {
                    table,
                    action: AlterAction::RenameTable { new_name },
                }));
            }
            if self.peek().is_keyword("COLUMN") {
                self.advance();
            }
            let old = self.parse_ident()?;
            self.expect_keyword("TO")?;
            let new = self.parse_ident()?;
            Ok(Statement::Alter(AlterStatement {
                table,
                action: AlterAction::RenameColumn { old, new },
            }))
        } else if self.peek().is_keyword("ADD") {
            self.advance();
            if self.peek().is_keyword("COLUMN") {
                self.advance();
            }
            let column = self.parse_column_def()?;
            Ok(Statement::Alter(AlterStatement {
                table,
                action: AlterAction::AddColumn { column },
            }))
        } else if self.peek().is_keyword("DROP") {
            self.advance();
            if self.peek().is_keyword("COLUMN") {
                self.advance();
            }
            let name = self.parse_ident()?;
            Ok(Statement::Alter(AlterStatement {
                table,
                action: AlterAction::DropColumn { name },
            }))
        } else {
            Err(Error::parse(
                self.peek().line,
                self.peek().col,
                "expected RENAME, ADD, or DROP after table name",
            ))
        }
    }

    fn parse_attach(&mut self) -> Result<Statement> {
        self.advance(); // ATTACH
        self.expect_keyword("DATABASE").ok();
        let expr = self.parse_primary_expr()?;
        self.expect_keyword("AS")?;
        let schema = self.parse_ident()?;
        Ok(Statement::Attach(AttachStatement { expr, schema }))
    }

    fn parse_detach(&mut self) -> Result<Statement> {
        self.advance(); // DETACH
        self.expect_keyword("DATABASE").ok();
        let schema = self.parse_ident()?;
        Ok(Statement::Detach(DetachStatement { schema }))
    }

    fn parse_vacuum(&mut self) -> Result<Statement> {
        self.advance(); // VACUUM
        let schema = if let Token::Ident(_) = &self.peek().token {
            Some(self.parse_ident()?)
        } else {
            None
        };
        let into = if self.peek().is_keyword("INTO") {
            self.advance();
            Some(self.parse_string_literal()?)
        } else {
            None
        };
        Ok(Statement::Vacuum(VacuumStatement { schema, into }))
    }

    // ========================================================================
    // SELECT parsing
    // ========================================================================

    fn parse_with_clause(&mut self) -> Result<WithClause> {
        self.expect_keyword("WITH")?;
        let recursive = self.consume_keyword("RECURSIVE");
        let mut ctes = Vec::new();
        loop {
            let name = self.parse_ident()?;
            let columns = if self.peek().is_punct('(') {
                self.advance();
                let cols = self.parse_ident_list()?;
                self.expect_punct(')')?;
                Some(cols)
            } else {
                None
            };
            self.expect_keyword("AS")?;
            let materialized = if self.peek().is_keyword("MATERIALIZED") {
                self.advance();
                Some(true)
            } else if self.peek().is_keyword("NOT") {
                self.advance();
                self.expect_keyword("MATERIALIZED")?;
                Some(false)
            } else {
                None
            };
            self.expect_punct('(')?;
            let select = self.parse_select()?;
            self.expect_punct(')')?;
            ctes.push(Cte { name, columns, select: Box::new(select), materialized });
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(WithClause { recursive, ctes })
    }

    fn parse_select(&mut self) -> Result<SelectStatement> {
        let with = if self.peek().is_keyword("WITH") {
            Some(self.parse_with_clause()?)
        } else {
            None
        };
        let body = self.parse_select_body()?;
        let order_by = if self.peek().is_keyword("ORDER") {
            self.advance();
            self.expect_keyword("BY")?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        let (limit, offset) = if self.peek().is_keyword("LIMIT") {
            self.advance();
            let l = self.parse_expr()?;
            let o = if self.peek().is_keyword("OFFSET") {
                self.advance();
                Some(self.parse_expr()?)
            } else if self.peek().is_punct(',') {
                self.advance();
                Some(l.clone())
            } else {
                None
            };
            (Some(l), o)
        } else {
            (None, None)
        };
        Ok(SelectStatement { with, body, order_by, limit, offset })
    }

    fn parse_select_body(&mut self) -> Result<SelectBody> {
        let left = self.parse_simple_select()?;
        // Look for set operators
        if self.peek().is_keyword("UNION") || self.peek().is_keyword("INTERSECT") || self.peek().is_keyword("EXCEPT") {
            let op = if self.peek().is_keyword("UNION") {
                self.advance();
                if self.consume_keyword("ALL") {
                    SetOp::UnionAll
                } else {
                    SetOp::Union
                }
            } else if self.peek().is_keyword("INTERSECT") {
                self.advance();
                SetOp::Intersect
            } else {
                self.advance();
                SetOp::Except
            };
            let right = self.parse_select_body()?;
            return Ok(SelectBody::Binary {
                op,
                left: Box::new(SelectBody::Simple(left)),
                right: Box::new(right),
            });
        }
        Ok(SelectBody::Simple(left))
    }

    fn parse_simple_select(&mut self) -> Result<SimpleSelect> {
        if self.peek().is_keyword("VALUES") {
            // VALUES (..), (..) — convert to a SELECT 1, 2, ... FROM (VALUES ...) form
            self.advance();
            let mut rows = Vec::new();
            loop {
                self.expect_punct('(')?;
                let mut row = Vec::new();
                loop {
                    row.push(self.parse_expr()?);
                    if self.peek().is_punct(',') {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect_punct(')')?;
                rows.push(row);
                if self.peek().is_punct(',') {
                    self.advance();
                } else {
                    break;
                }
            }
            // Simplify: just return VALUES as a single-row select for now.
            // Each row becomes a literal expression column. We use the first row.
            // Note: a real implementation would handle multi-row VALUES, but this
            // suffices for the common single-row case.
            let first_row = rows.into_iter().next().unwrap();
            let n_cols = first_row.len();
            let columns: Vec<ResultColumn> = (0..n_cols)
                .map(|i| ResultColumn::Expr {
                    expr: first_row[i].clone(),
                    alias: None,
                })
                .collect();
            return Ok(SimpleSelect {
                distinct: false,
                columns,
                from: None,
                where_clause: None,
                group_by: Vec::new(),
                having: None,
                window: Vec::new(),
            });
        }
        self.expect_keyword("SELECT")?;
        let distinct = if self.peek().is_keyword("DISTINCT") {
            self.advance();
            true
        } else if self.peek().is_keyword("ALL") {
            self.advance();
            false
        } else {
            false
        };
        let columns = self.parse_result_columns()?;
        let from = if self.peek().is_keyword("FROM") {
            self.advance();
            Some(self.parse_table_expression()?)
        } else {
            None
        };
        let where_clause = if self.peek().is_keyword("WHERE") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let group_by = if self.peek().is_keyword("GROUP") {
            self.advance();
            self.expect_keyword("BY")?;
            let mut g = Vec::new();
            loop {
                g.push(self.parse_expr()?);
                if self.peek().is_punct(',') {
                    self.advance();
                } else {
                    break;
                }
            }
            g
        } else {
            Vec::new()
        };
        let having = if self.peek().is_keyword("HAVING") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let window = if self.peek().is_keyword("WINDOW") {
            self.advance();
            self.parse_window_defs()?
        } else {
            Vec::new()
        };
        Ok(SimpleSelect {
            distinct,
            columns,
            from,
            where_clause,
            group_by,
            having,
            window,
        })
    }

    fn parse_result_columns(&mut self) -> Result<Vec<ResultColumn>> {
        let mut out = Vec::new();
        loop {
            if self.peek().is_op("*") {
                self.advance();
                out.push(ResultColumn::Star);
            } else if let Token::Ident(s) = &self.peek().token {
                if s == "*" {
                    self.advance();
                    out.push(ResultColumn::Star);
                } else {
                    // Could be `table.*` or an expression.
                    let next = self.peek_n(1);
                    if next.is_punct('.') && self.peek_n(2).is_op("*") {
                        let table = s.clone();
                        self.advance();
                        self.advance();
                        self.advance();
                        out.push(ResultColumn::TableStar(table));
                    } else {
                        let e = self.parse_expr()?;
                        let alias = if self.peek().is_keyword("AS") {
                            self.advance();
                            Some(self.parse_ident()?)
                        } else if let Token::Ident(_) = &self.peek().token {
                            // Implicit alias (no AS keyword)
                            Some(self.parse_ident()?)
                        } else if let Token::QuotedIdent(s) = &self.peek().token {
                            Some(s.clone())
                        } else {
                            None
                        };
                        out.push(ResultColumn::Expr { expr: e, alias });
                    }
                }
            } else {
                let e = self.parse_expr()?;
                let alias = if self.peek().is_keyword("AS") {
                    self.advance();
                    Some(self.parse_ident()?)
                } else if let Token::Ident(_) | Token::QuotedIdent(_) = &self.peek().token {
                    if !is_clause_keyword(&self.peek().token) {
                        Some(self.parse_ident()?)
                    } else {
                        None
                    }
                } else {
                    None
                };
                out.push(ResultColumn::Expr { expr: e, alias });
            }
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn parse_table_expression(&mut self) -> Result<TableExpression> {
        let mut left = self.parse_table_primary()?;
        loop {
            let join_type = if self.peek().is_keyword("JOIN") {
                self.advance();
                JoinType::Inner
            } else if self.peek().is_keyword("INNER") {
                self.advance();
                self.expect_keyword("JOIN")?;
                JoinType::Inner
            } else if self.peek().is_keyword("LEFT") {
                self.advance();
                self.consume_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinType::Left
            } else if self.peek().is_keyword("RIGHT") {
                self.advance();
                self.consume_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinType::Right
            } else if self.peek().is_keyword("FULL") {
                self.advance();
                self.consume_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinType::Full
            } else if self.peek().is_keyword("CROSS") {
                self.advance();
                self.expect_keyword("JOIN")?;
                JoinType::Cross
            } else if self.peek().is_keyword("NATURAL") {
                self.advance();
                self.expect_keyword("JOIN")?;
                return Ok(TableExpression::Join {
                    left: Box::new(left),
                    right: Box::new(self.parse_table_primary()?),
                    join_type: JoinType::Inner,
                    constraint: JoinConstraint::Natural,
                });
            } else {
                break;
            };
            let right = self.parse_table_primary()?;
            let constraint = if self.peek().is_keyword("ON") {
                self.advance();
                JoinConstraint::On(self.parse_expr()?)
            } else if self.peek().is_keyword("USING") {
                self.advance();
                self.expect_punct('(')?;
                let cols = self.parse_ident_list()?;
                self.expect_punct(')')?;
                JoinConstraint::Using(cols)
            } else {
                JoinConstraint::None
            };
            left = TableExpression::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type,
                constraint,
            };
        }
        Ok(left)
    }

    fn parse_table_primary(&mut self) -> Result<TableExpression> {
        if self.peek().is_punct('(') {
            self.advance();
            // Could be a subquery or a parenthesized table expression.
            if self.peek().is_keyword("SELECT") || self.peek().is_keyword("WITH") || self.peek().is_keyword("VALUES") {
                let select = self.parse_select()?;
                self.expect_punct(')')?;
                let alias = if self.peek().is_keyword("AS") {
                    self.advance();
                    Some(self.parse_ident()?)
                } else if let Token::Ident(_) = &self.peek().token {
                    Some(self.parse_ident()?)
                } else {
                    None
                };
                let column_aliases = if alias.is_some() && self.peek().is_punct('(') {
                    self.advance();
                    let cols = self.parse_ident_list()?;
                    self.expect_punct(')')?;
                    Some(cols)
                } else {
                    None
                };
                return Ok(TableExpression::Subquery {
                    select: Box::new(select),
                    alias,
                    column_aliases,
                });
            } else {
                let inner = self.parse_table_expression()?;
                self.expect_punct(')')?;
                return Ok(inner);
            }
        }
        let (schema, name) = self.parse_qualified_name()?;
        let alias = if self.peek().is_keyword("AS") {
            self.advance();
            Some(self.parse_ident()?)
        } else if let Token::Ident(_) = &self.peek().token {
            if !is_clause_keyword(&self.peek().token) {
                Some(self.parse_ident()?)
            } else {
                None
            }
        } else {
            None
        };
        let indexed = if self.peek().is_keyword("INDEXED") {
            self.advance();
            self.expect_keyword("BY")?;
            Some(IndexedHint::Indexed(self.parse_ident()?))
        } else if self.peek().is_keyword("NOT") {
            self.advance();
            self.expect_keyword("INDEXED")?;
            Some(IndexedHint::NotIndexed)
        } else {
            None
        };
        Ok(TableExpression::Table { name, schema, alias, indexed })
    }

    fn parse_order_terms(&mut self) -> Result<Vec<OrderTerm>> {
        let mut out = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let order = if self.peek().is_keyword("ASC") {
                self.advance();
                Order::Asc
            } else if self.peek().is_keyword("DESC") {
                self.advance();
                Order::Desc
            } else {
                Order::Asc
            };
            let nulls = if self.peek().is_keyword("NULLS") {
                self.advance();
                if self.peek().is_keyword("FIRST") {
                    self.advance();
                    NullsOrder::First
                } else {
                    self.expect_keyword("LAST")?;
                    NullsOrder::Last
                }
            } else {
                NullsOrder::Default
            };
            out.push(OrderTerm { expr, order, nulls });
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn parse_window_defs(&mut self) -> Result<Vec<WindowDef>> {
        let mut out = Vec::new();
        loop {
            let name = self.parse_ident()?;
            self.expect_keyword("AS")?;
            self.expect_punct('(')?;
            let base = if let Token::Ident(_) = &self.peek().token {
                if !self.peek().is_keyword("PARTITION") && !self.peek().is_keyword("ORDER")
                    && !self.peek().is_keyword("ROWS") && !self.peek().is_keyword("RANGE")
                    && !self.peek().is_keyword("GROUPS")
                {
                    let b = self.parse_ident()?;
                    Some(b)
                } else {
                    None
                }
            } else {
                None
            };
            let partition_by = if self.peek().is_keyword("PARTITION") {
                self.advance();
                self.expect_keyword("BY")?;
                let mut p = Vec::new();
                loop {
                    p.push(self.parse_expr()?);
                    if self.peek().is_punct(',') {
                        self.advance();
                    } else {
                        break;
                    }
                }
                p
            } else {
                Vec::new()
            };
            let order_by = if self.peek().is_keyword("ORDER") {
                self.advance();
                self.expect_keyword("BY")?;
                self.parse_order_terms()?
            } else {
                Vec::new()
            };
            let frame = if self.peek().is_keyword("ROWS") || self.peek().is_keyword("RANGE") || self.peek().is_keyword("GROUPS") {
                Some(Box::new(self.parse_window_frame()?))
            } else {
                None
            };
            self.expect_punct(')')?;
            out.push(WindowDef { name, base, partition_by, order_by, frame });
            if self.peek().is_punct(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(out)
    }

    fn parse_window_frame(&mut self) -> Result<WindowFrame> {
        let kind = if self.peek().is_keyword("ROWS") {
            self.advance();
            FrameKind::Rows
        } else if self.peek().is_keyword("RANGE") {
            self.advance();
            FrameKind::Range
        } else {
            self.advance();
            FrameKind::Groups
        };
        let (start, end) = if self.peek().is_keyword("BETWEEN") {
            self.advance();
            let start = self.parse_frame_bound()?;
            self.expect_keyword("AND")?;
            let end = self.parse_frame_bound()?;
            (Box::new(start), Some(Box::new(end)))
        } else {
            (Box::new(self.parse_frame_bound()?), None)
        };
        let exclude = if self.peek().is_keyword("EXCLUDE") {
            self.advance();
            if self.peek().is_keyword("NO") {
                self.advance();
                self.expect_keyword("OTHERS")?;
                FrameExclude::NoOthers
            } else if self.peek().is_keyword("CURRENT") {
                self.advance();
                self.expect_keyword("ROW")?;
                FrameExclude::CurrentRow
            } else if self.peek().is_keyword("GROUP") {
                self.advance();
                FrameExclude::Group
            } else {
                self.expect_keyword("TIES")?;
                FrameExclude::Ties
            }
        } else {
            FrameExclude::NoOthers
        };
        Ok(WindowFrame { kind, start, end, exclude })
    }

    fn parse_frame_bound(&mut self) -> Result<FrameBound> {
        if self.peek().is_keyword("UNBOUNDED") {
            self.advance();
            if self.peek().is_keyword("PRECEDING") {
                self.advance();
                Ok(FrameBound::UnboundedPreceding)
            } else {
                self.expect_keyword("FOLLOWING")?;
                Ok(FrameBound::UnboundedFollowing)
            }
        } else if self.peek().is_keyword("CURRENT") {
            self.advance();
            self.expect_keyword("ROW")?;
            Ok(FrameBound::CurrentRow)
        } else {
            let e = self.parse_expr()?;
            if self.peek().is_keyword("PRECEDING") {
                self.advance();
                Ok(FrameBound::Preceding(Box::new(e)))
            } else {
                self.expect_keyword("FOLLOWING")?;
                Ok(FrameBound::Following(Box::new(e)))
            }
        }
    }

    // ========================================================================
    // Expression parsing (precedence climbing)
    // ========================================================================

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_binary(1)
    }

    fn parse_binary(&mut self, min_prec: u8) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.try_binary_op() {
                Some(o) => o,
                None => break,
            };
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            self.advance_op();
            // Right-associative? None of ours are.
            let right = self.parse_binary(prec + 1)?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn try_binary_op(&self) -> Option<BinaryOp> {
        let t = &self.peek().token;
        match t {
            Token::Op(s) => match *s {
                "+" => Some(BinaryOp::Add),
                "-" => Some(BinaryOp::Sub),
                "*" => Some(BinaryOp::Mul),
                "/" => Some(BinaryOp::Div),
                "%" => Some(BinaryOp::Mod),
                "||" => Some(BinaryOp::Concat),
                "&" => Some(BinaryOp::BitAnd),
                "|" => Some(BinaryOp::BitOr),
                "^" => Some(BinaryOp::BitXor),
                "<<" => Some(BinaryOp::ShiftLeft),
                ">>" => Some(BinaryOp::ShiftRight),
                "=" | "==" => Some(BinaryOp::Eq),
                "!=" | "<>" => Some(BinaryOp::NotEq),
                "<" => Some(BinaryOp::Lt),
                "<=" => Some(BinaryOp::LtEq),
                ">" => Some(BinaryOp::Gt),
                ">=" => Some(BinaryOp::GtEq),
                _ => None,
            },
            Token::Keyword(k) => match *k {
                "AND" => Some(BinaryOp::And),
                "OR" => Some(BinaryOp::Or),
                _ => None,
            },
            _ => None,
        }
    }

    fn advance_op(&mut self) {
        self.advance();
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        let t = self.peek();
        if t.is_op("-") {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(e) });
        }
        if t.is_op("+") {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary { op: UnaryOp::Pos, expr: Box::new(e) });
        }
        if t.is_op("~") {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary { op: UnaryOp::BitNot, expr: Box::new(e) });
        }
        if t.is_keyword("NOT") {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary { op: UnaryOp::Not, expr: Box::new(e) });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut e = self.parse_primary_expr()?;
        loop {
            if self.peek().is_keyword("COLLATE") {
                self.advance();
                let c = self.parse_ident()?;
                e = Expr::Collate { expr: Box::new(e), collation: c };
            } else if self.peek().is_keyword("IS") {
                self.advance();
                let negated = self.consume_keyword("NOT");
                if self.peek().is_keyword("NULL") {
                    self.advance();
                    e = Expr::IsNull { expr: Box::new(e), negated };
                } else {
                    let right = self.parse_primary_expr()?;
                    e = Expr::Is { left: Box::new(e), right: Box::new(right), negated };
                }
            } else if self.peek().is_keyword("ISNULL") {
                self.advance();
                e = Expr::IsNull { expr: Box::new(e), negated: false };
            } else if self.peek().is_keyword("NOTNULL") {
                self.advance();
                e = Expr::IsNull { expr: Box::new(e), negated: true };
            } else if self.peek().is_keyword("NOT") {
                // NOT LIKE / NOT IN / NOT BETWEEN
                self.advance();
                if self.peek().is_keyword("LIKE") {
                    self.advance();
                    let pat = self.parse_primary_expr()?;
                    let esc = if self.peek().is_keyword("ESCAPE") {
                        self.advance();
                        Some(Box::new(self.parse_primary_expr()?))
                    } else {
                        None
                    };
                    e = Expr::Like { op: LikeOp::Like, expr: Box::new(e), pattern: Box::new(pat), escape: esc, negated: true };
                } else if self.peek().is_keyword("GLOB") {
                    self.advance();
                    let pat = self.parse_primary_expr()?;
                    e = Expr::Like { op: LikeOp::Glob, expr: Box::new(e), pattern: Box::new(pat), escape: None, negated: true };
                } else if self.peek().is_keyword("REGEXP") {
                    self.advance();
                    let pat = self.parse_primary_expr()?;
                    e = Expr::Like { op: LikeOp::Regexp, expr: Box::new(e), pattern: Box::new(pat), escape: None, negated: true };
                } else if self.peek().is_keyword("MATCH") {
                    self.advance();
                    let pat = self.parse_primary_expr()?;
                    e = Expr::Like { op: LikeOp::Match, expr: Box::new(e), pattern: Box::new(pat), escape: None, negated: true };
                } else if self.peek().is_keyword("IN") {
                    self.advance();
                    let src = self.parse_in_source()?;
                    e = Expr::In { expr: Box::new(e), source: src, negated: true };
                } else if self.peek().is_keyword("BETWEEN") {
                    self.advance();
                    let low = self.parse_binary(8)?;
                    self.expect_keyword("AND")?;
                    let high = self.parse_binary(8)?;
                    e = Expr::Between { expr: Box::new(e), low: Box::new(low), high: Box::new(high), negated: true };
                } else {
                    // NOT was consumed but no follow-up — that's a parse error.
                    let t = self.peek();
                    return Err(Error::parse(t.line, t.col, format!("unexpected token after NOT: {:?}", t.token)));
                }
            } else if self.peek().is_keyword("LIKE") {
                self.advance();
                let pat = self.parse_primary_expr()?;
                let esc = if self.peek().is_keyword("ESCAPE") {
                    self.advance();
                    Some(Box::new(self.parse_primary_expr()?))
                } else {
                    None
                };
                e = Expr::Like { op: LikeOp::Like, expr: Box::new(e), pattern: Box::new(pat), escape: esc, negated: false };
            } else if self.peek().is_keyword("GLOB") {
                self.advance();
                let pat = self.parse_primary_expr()?;
                e = Expr::Like { op: LikeOp::Glob, expr: Box::new(e), pattern: Box::new(pat), escape: None, negated: false };
            } else if self.peek().is_keyword("REGEXP") {
                self.advance();
                let pat = self.parse_primary_expr()?;
                e = Expr::Like { op: LikeOp::Regexp, expr: Box::new(e), pattern: Box::new(pat), escape: None, negated: false };
            } else if self.peek().is_keyword("MATCH") {
                self.advance();
                let pat = self.parse_primary_expr()?;
                e = Expr::Like { op: LikeOp::Match, expr: Box::new(e), pattern: Box::new(pat), escape: None, negated: false };
            } else if self.peek().is_keyword("IN") {
                self.advance();
                let src = self.parse_in_source()?;
                e = Expr::In { expr: Box::new(e), source: src, negated: false };
            } else if self.peek().is_keyword("BETWEEN") {
                self.advance();
                let low = self.parse_binary(8)?;
                self.expect_keyword("AND")?;
                let high = self.parse_binary(8)?;
                e = Expr::Between { expr: Box::new(e), low: Box::new(low), high: Box::new(high), negated: false };
            } else if self.peek().is_keyword("FILTER") {
                self.advance();
                self.expect_punct('(')?;
                self.expect_keyword("WHERE")?;
                let f = self.parse_expr()?;
                self.expect_punct(')')?;
                // FILTER must attach to a function call.
                if let Expr::Function { name, distinct, args, over, .. } = e {
                    e = Expr::Function {
                        name,
                        distinct,
                        args,
                        filter: Some(Box::new(f)),
                        over,
                    };
                } else {
                    let t = self.peek();
                    return Err(Error::parse(t.line, t.col, "FILTER must follow a function call"));
                }
            } else if self.peek().is_keyword("OVER") {
                self.advance();
                let over = if self.peek().is_punct('(') {
                    self.advance();
                    let w = self.parse_window_def_inline()?;
                    self.expect_punct(')')?;
                    WindowSpec::Inline(Box::new(w))
                } else {
                    WindowSpec::Named(self.parse_ident()?)
                };
                if let Expr::Function { name, distinct, args, filter, .. } = e {
                    e = Expr::Function {
                        name,
                        distinct,
                        args,
                        filter,
                        over: Some(Box::new(over)),
                    };
                } else {
                    let t = self.peek();
                    return Err(Error::parse(t.line, t.col, "OVER must follow a function call"));
                }
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_window_def_inline(&mut self) -> Result<WindowDef> {
        let base = if let Token::Ident(_) = &self.peek().token {
            if !self.peek().is_keyword("PARTITION") && !self.peek().is_keyword("ORDER")
                && !self.peek().is_keyword("ROWS") && !self.peek().is_keyword("RANGE")
                && !self.peek().is_keyword("GROUPS")
            {
                let b = self.parse_ident()?;
                Some(b)
            } else {
                None
            }
        } else {
            None
        };
        let partition_by = if self.peek().is_keyword("PARTITION") {
            self.advance();
            self.expect_keyword("BY")?;
            let mut p = Vec::new();
            loop {
                p.push(self.parse_expr()?);
                if self.peek().is_punct(',') {
                    self.advance();
                } else {
                    break;
                }
            }
            p
        } else {
            Vec::new()
        };
        let order_by = if self.peek().is_keyword("ORDER") {
            self.advance();
            self.expect_keyword("BY")?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        let frame = if self.peek().is_keyword("ROWS") || self.peek().is_keyword("RANGE") || self.peek().is_keyword("GROUPS") {
            Some(Box::new(self.parse_window_frame()?))
        } else {
            None
        };
        Ok(WindowDef { name: String::new(), base, partition_by, order_by, frame })
    }

    fn parse_in_source(&mut self) -> Result<InSource> {
        if self.peek().is_punct('(') {
            self.advance();
            if self.peek().is_keyword("SELECT") || self.peek().is_keyword("WITH") || self.peek().is_keyword("VALUES") {
                let s = self.parse_select()?;
                self.expect_punct(')')?;
                Ok(InSource::Subquery(Box::new(s)))
            } else {
                let mut list = Vec::new();
                if !self.peek().is_punct(')') {
                    loop {
                        list.push(self.parse_expr()?);
                        if self.peek().is_punct(',') {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect_punct(')')?;
                Ok(InSource::List(list))
            }
        } else {
            let name = self.parse_ident()?;
            Ok(InSource::Table(name))
        }
    }

    fn parse_primary_expr(&mut self) -> Result<Expr> {
        // Clone the token's discriminant info to release the borrow on self
        // before we call self.advance() etc.
        let tok = self.peek().token.clone();
        let line = self.peek().line;
        let col = self.peek().col;
        match &tok {
            Token::Integer(i) => {
                self.advance();
                Ok(Expr::Literal(Value::Integer(*i)))
            }
            Token::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Value::Real(*f)))
            }
            Token::String(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s.clone().into())))
            }
            Token::Blob(b) => {
                self.advance();
                Ok(Expr::Literal(Value::Blob(b.clone())))
            }
            Token::Parameter(p) => {
                self.advance();
                Ok(Expr::Parameter(p.clone()))
            }
            Token::Keyword(k) => match *k {
                "NULL" => {
                    self.advance();
                    Ok(Expr::Literal(Value::Null))
                }
                "TRUE" => {
                    self.advance();
                    Ok(Expr::Literal(Value::Integer(1)))
                }
                "FALSE" => {
                    self.advance();
                    Ok(Expr::Literal(Value::Integer(0)))
                }
                "CURRENT_DATE" | "CURRENT_TIME" | "CURRENT_TIMESTAMP" | "CURRENT" => {
                    // Treat as a function with no args.
                    let name = (*k).to_string();
                    self.advance();
                    if self.peek().is_punct('(') {
                        self.advance();
                        self.expect_punct(')')?;
                    }
                    Ok(Expr::Function {
                        name,
                        distinct: false,
                        args: Vec::new(),
                        filter: None,
                        over: None,
                    })
                }
                "CASE" => self.parse_case(),
                "CAST" => self.parse_cast(),
                "EXISTS" => {
                    self.advance();
                    self.expect_punct('(')?;
                    let s = self.parse_select()?;
                    self.expect_punct(')')?;
                    Ok(Expr::Exists(Box::new(s)))
                }
                "RAISE" => self.parse_raise(),
                _ => {
                    // Could be a function call with a keyword name (e.g. LEFT, RIGHT).
                    if let Some(next) = self.toks.get(self.pos + 1) {
                        if next.is_punct('(') {
                            let name = (*k).to_string();
                            self.advance();
                            self.advance(); // consume (
                            return self.parse_function_call(name);
                        }
                    }
                    Err(Error::parse(line, col, format!("unexpected keyword in expression: {}", k)))
                }
            },
            Token::Ident(s) => {
                // Could be column, function call, or qualified column.
                let name = s.clone();
                self.advance();
                if self.peek().is_punct('(') {
                    self.advance();
                    return self.parse_function_call(name);
                }
                if self.peek().is_punct('.') {
                    self.advance();
                    if self.peek().is_op("*") {
                        // table.* — shouldn't happen in expression context
                        let t = self.peek();
                        return Err(Error::parse(t.line, t.col, "table.* not valid in expression"));
                    }
                    let col = self.parse_ident_or_keyword()?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    });
                }
                Ok(Expr::Column { table: None, name })
            }
            Token::QuotedIdent(s) => {
                let name = s.clone();
                self.advance();
                if self.peek().is_punct('.') {
                    self.advance();
                    let col = self.parse_ident()?;
                    return Ok(Expr::Column { table: Some(name), name: col });
                }
                Ok(Expr::Column { table: None, name })
            }
            Token::Punct('(') => {
                self.advance();
                // Could be subquery or parenthesized expression or row value.
                if self.peek().is_keyword("SELECT") || self.peek().is_keyword("WITH") || self.peek().is_keyword("VALUES") {
                    let s = self.parse_select()?;
                    self.expect_punct(')')?;
                    return Ok(Expr::Subquery(Box::new(s)));
                }
                let e = self.parse_expr()?;
                if self.peek().is_punct(',') {
                    // Row value
                    let mut row = vec![e];
                    while self.peek().is_punct(',') {
                        self.advance();
                        row.push(self.parse_expr()?);
                    }
                    self.expect_punct(')')?;
                    return Ok(Expr::Row(row));
                }
                self.expect_punct(')')?;
                Ok(e)
            }
            _ => Err(Error::parse(line, col, format!("unexpected token in expression: {:?}", tok))),
        }
    }

    fn parse_function_call(&mut self, name: String) -> Result<Expr> {
        let distinct = self.consume_keyword("DISTINCT");
        let mut args = Vec::new();
        if self.peek().is_op("*") {
            // COUNT(*) special case
            self.advance();
            args.push(Expr::Column { table: None, name: "*".to_string() });
        } else if !self.peek().is_punct(')') {
            loop {
                args.push(self.parse_expr()?);
                if self.peek().is_punct(',') {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.expect_punct(')')?;
        Ok(Expr::Function {
            name,
            distinct,
            args,
            filter: None,
            over: None,
        })
    }

    fn parse_case(&mut self) -> Result<Expr> {
        self.advance(); // CASE
        let operand = if !self.peek().is_keyword("WHEN") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        let mut whens = Vec::new();
        while self.peek().is_keyword("WHEN") {
            self.advance();
            let cond = self.parse_expr()?;
            self.expect_keyword("THEN")?;
            let val = self.parse_expr()?;
            whens.push((cond, val));
        }
        let else_ = if self.peek().is_keyword("ELSE") {
            self.advance();
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_keyword("END")?;
        Ok(Expr::Case { operand, whens, else_ })
    }

    fn parse_cast(&mut self) -> Result<Expr> {
        self.advance(); // CAST
        self.expect_punct('(')?;
        let expr = self.parse_expr()?;
        self.expect_keyword("AS")?;
        // Type name can be multi-word
        let mut parts = Vec::new();
        while let Token::Ident(s) = &self.peek().token {
            parts.push(s.clone());
            self.advance();
            if self.peek().is_punct('(') {
                // Skip (n) or (n, m)
                self.advance();
                let mut depth = 1;
                while depth > 0 {
                    if self.peek().is_punct('(') {
                        depth += 1;
                    } else if self.peek().is_punct(')') {
                        depth -= 1;
                    }
                    if depth > 0 {
                        self.advance();
                    }
                }
                if self.peek().is_punct(')') {
                    self.advance();
                }
                break;
            }
        }
        self.expect_punct(')')?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            type_name: parts.join(" "),
        })
    }

    fn parse_raise(&mut self) -> Result<Expr> {
        self.advance(); // RAISE
        self.expect_punct('(')?;
        let action = if self.peek().is_keyword("IGNORE") {
            self.advance();
            RaiseAction::Ignore
        } else if self.peek().is_keyword("ROLLBACK") {
            self.advance();
            RaiseAction::Rollback
        } else if self.peek().is_keyword("ABORT") {
            self.advance();
            RaiseAction::Abort
        } else {
            self.expect_keyword("FAIL")?;
            RaiseAction::Fail
        };
        let message = if self.peek().is_punct(',') {
            self.advance();
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_punct(')')?;
        Ok(Expr::Raise { action, message })
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn parse_ident(&mut self) -> Result<String> {
        let t = self.peek();
        match &t.token {
            Token::Ident(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            Token::QuotedIdent(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(Error::parse(t.line, t.col, format!("expected identifier, got {:?}", t.token))),
        }
    }

    fn parse_ident_or_keyword(&mut self) -> Result<String> {
        let t = self.peek();
        match &t.token {
            Token::Ident(s) | Token::QuotedIdent(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            Token::Keyword(k) => {
                // Allow keywords as column names in qualified context.
                let s = k.clone();
                self.advance();
                Ok(s.to_ascii_lowercase())
            }
            _ => Err(Error::parse(t.line, t.col, format!("expected identifier, got {:?}", t.token))),
        }
    }

    fn parse_string_literal(&mut self) -> Result<String> {
        let t = self.peek();
        match &t.token {
            Token::String(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(Error::parse(t.line, t.col, format!("expected string literal, got {:?}", t.token))),
        }
    }

    fn parse_table_name(&mut self) -> Result<TableName> {
        let (schema, name) = self.parse_qualified_name()?;
        Ok(TableName { schema, name })
    }

    fn parse_qualified_name(&mut self) -> Result<(Option<String>, String)> {
        let first = self.parse_ident()?;
        if self.peek().is_punct('.') {
            self.advance();
            let second = self.parse_ident()?;
            Ok((Some(first), second))
        } else {
            Ok((None, first))
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        let t = self.peek();
        if t.is_keyword(kw) {
            self.advance();
            Ok(())
        } else {
            Err(Error::parse(t.line, t.col, format!("expected keyword {}, got {:?}", kw, t.token)))
        }
    }

    fn consume_keyword(&mut self, kw: &str) -> bool {
        if self.peek().is_keyword(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, c: char) -> Result<()> {
        let t = self.peek();
        if t.is_punct(c) {
            self.advance();
            Ok(())
        } else {
            Err(Error::parse(t.line, t.col, format!("expected '{}', got {:?}", c, t.token)))
        }
    }

    fn expect_op(&mut self, s: &str) -> Result<()> {
        let t = self.peek();
        if t.is_op(s) {
            self.advance();
            Ok(())
        } else {
            Err(Error::parse(t.line, t.col, format!("expected '{}', got {:?}", s, t.token)))
        }
    }

    fn peek(&self) -> &SpannedToken {
        &self.toks[self.pos]
    }

    fn peek_n(&self, n: usize) -> &SpannedToken {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i]
    }

    fn advance(&mut self) {
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
    }
}

fn is_clause_keyword(t: &Token) -> bool {
    match t {
        Token::Keyword(k) => matches!(
            *k,
            "FROM" | "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "OFFSET"
                | "JOIN" | "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS" | "NATURAL"
                | "ON" | "USING" | "AS" | "WINDOW" | "UNION" | "INTERSECT" | "EXCEPT"
                | "RETURNING" | "SET" | "VALUES" | "DEFAULT" | "INTO" | "BY" | "AND"
                | "OR" | "BETWEEN" | "IN" | "LIKE" | "GLOB" | "REGEXP" | "MATCH"
                | "IS" | "ISNULL" | "NOTNULL" | "ESCAPE" | "COLLATE" | "FILTER"
                | "OVER" | "THEN" | "ELSE" | "END" | "WHEN" | "ASC" | "DESC" | "NULLS"
        ),
        _ => false,
    }
}

fn is_constraint_keyword(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "PRIMARY" | "NOT" | "NULL" | "UNIQUE" | "CHECK" | "DEFAULT" | "COLLATE"
            | "REFERENCES" | "GENERATED" | "AS" | "CONSTRAINT"
    )
}

/// Parse a single SQL statement from a string.
pub fn parse(src: &str) -> Result<Statement> {
    Parser::new(src)?.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(s: &str) -> Statement {
        parse(s).unwrap_or_else(|e| panic!("failed to parse {:?}: {}", s, e))
    }

    #[test]
    fn select_basic() {
        let _ = parse_ok("SELECT 1");
        let _ = parse_ok("SELECT * FROM users");
        let _ = parse_ok("SELECT id, name, email FROM users WHERE id = 1");
    }

    #[test]
    fn select_with_join() {
        let _ = parse_ok("SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id");
        let _ = parse_ok("SELECT * FROM a LEFT JOIN b ON a.x = b.x");
        let _ = parse_ok("SELECT * FROM a RIGHT JOIN b ON a.x = b.x");
        let _ = parse_ok("SELECT * FROM a FULL JOIN b ON a.x = b.x");
    }

    #[test]
    fn select_with_grouping() {
        let _ = parse_ok("SELECT user_id, COUNT(*) FROM orders GROUP BY user_id");
        let _ = parse_ok("SELECT user_id, SUM(total) AS t FROM orders GROUP BY user_id HAVING t > 100");
    }

    #[test]
    fn select_with_window() {
        let _ = parse_ok("SELECT ROW_NUMBER() OVER (ORDER BY id) FROM users");
        let _ = parse_ok("SELECT name, SUM(salary) OVER (PARTITION BY dept) FROM employees");
        let _ = parse_ok("SELECT name, AVG(salary) OVER (PARTITION BY dept ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM employees");
    }

    #[test]
    fn select_with_cte() {
        let _ = parse_ok("WITH t AS (SELECT 1 AS x) SELECT * FROM t");
        let _ = parse_ok("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<10) SELECT * FROM r");
    }

    #[test]
    fn insert_basic() {
        let _ = parse_ok("INSERT INTO users (id, name) VALUES (1, 'Alice')");
        let _ = parse_ok("INSERT INTO users VALUES (1, 'Alice', 'alice@example.com')");
        let _ = parse_ok("INSERT OR REPLACE INTO users (id, name) VALUES (1, 'Alice')");
        let _ = parse_ok("INSERT INTO users (id, name) VALUES (1, 'Alice') ON CONFLICT(id) DO UPDATE SET name = excluded.name");
        let _ = parse_ok("INSERT INTO users (id, name) VALUES (1, 'Alice') RETURNING id");
    }

    #[test]
    fn update_basic() {
        let _ = parse_ok("UPDATE users SET name = 'Bob' WHERE id = 1");
        let _ = parse_ok("UPDATE users SET name = 'Bob', age = 30 WHERE id = 1 RETURNING *");
    }

    #[test]
    fn delete_basic() {
        let _ = parse_ok("DELETE FROM users WHERE id = 1");
        let _ = parse_ok("DELETE FROM users WHERE id = 1 RETURNING *");
    }

    #[test]
    fn create_table() {
        let _ = parse_ok("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)");
        let _ = parse_ok("CREATE TABLE IF NOT EXISTS users (id INTEGER, name TEXT, UNIQUE(name))");
        let _ = parse_ok("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id) ON DELETE CASCADE)");
    }

    #[test]
    fn create_index() {
        let _ = parse_ok("CREATE INDEX idx_users_email ON users(email)");
        let _ = parse_ok("CREATE UNIQUE INDEX idx_users_email ON users(email) WHERE active = 1");
    }

    #[test]
    fn create_view() {
        let _ = parse_ok("CREATE VIEW active_users AS SELECT * FROM users WHERE active = 1");
    }

    #[test]
    fn create_trigger() {
        let _ = parse_ok("CREATE TRIGGER trg AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO log(msg) VALUES ('new user'); END");
    }

    #[test]
    fn transactions() {
        let _ = parse_ok("BEGIN");
        let _ = parse_ok("BEGIN TRANSACTION");
        let _ = parse_ok("BEGIN DEFERRED");
        let _ = parse_ok("BEGIN IMMEDIATE");
        let _ = parse_ok("COMMIT");
        let _ = parse_ok("ROLLBACK");
        let _ = parse_ok("ROLLBACK TO SAVEPOINT sp1");
    }

    #[test]
    fn expressions() {
        let _ = parse_ok("SELECT 1 + 2 * 3");
        let _ = parse_ok("SELECT (1 + 2) * 3");
        let _ = parse_ok("SELECT a = b AND c = d OR e = f");
        let _ = parse_ok("SELECT CASE WHEN x > 0 THEN 'pos' ELSE 'neg' END");
        let _ = parse_ok("SELECT CAST('42' AS INTEGER)");
        let _ = parse_ok("SELECT name LIKE 'A%' FROM users");
        let _ = parse_ok("SELECT * FROM users WHERE id IN (SELECT user_id FROM orders)");
        let _ = parse_ok("SELECT * FROM users WHERE id IN (1, 2, 3)");
        let _ = parse_ok("SELECT * FROM users WHERE created_at BETWEEN '2020-01-01' AND '2020-12-31'");
        let _ = parse_ok("SELECT EXISTS (SELECT 1 FROM users WHERE id = 1)");
    }

    #[test]
    fn pragma() {
        let _ = parse_ok("PRAGMA page_size");
        let _ = parse_ok("PRAGMA page_size = 4096");
        let _ = parse_ok("PRAGMA journal_mode(WAL)");
    }
}

/// Pragma-value keywords: bare words accepted where an expression is
/// normally expected (`PRAGMA journal_mode = WAL`). Returns the canonical
/// uppercase spelling.
fn keyword_text(t: &crate::sql::lexer::Token) -> Option<String> {
    if let crate::sql::lexer::Token::Keyword(k) = t {
        match *k {
            "DELETE" | "WAL" | "MEMORY" | "TRUNCATE" | "PERSIST" | "NORMAL"
            | "FULL" | "EXTRA" | "ROW" | "STATEMENT" | "QUERY" | "INCREMENTAL"
            | "RESTART" | "PASSIVE" | "FORCE" | "OPTIMIZE" => Some(k.to_string()),
            _ => None,
        }
    } else {
        None
    }
}
