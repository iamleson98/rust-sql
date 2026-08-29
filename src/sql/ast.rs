//! SQL Abstract Syntax Tree.
//!
//! The AST is the contract between the parser and the planner. It mirrors
//! the source SQL closely (only minor desugaring), so that EXPLAIN output
//! and error messages stay close to what the user wrote.

use crate::types::Value;

/// A top-level SQL statement.
#[derive(Clone, Debug)]
pub enum Statement {
    Create(CreateStatement),
    Drop(DropStatement),
    Insert(InsertStatement),
    Select(SelectStatement),
    Update(UpdateStatement),
    Delete(DeleteStatement),
    Begin(BeginStatement),
    Commit,
    Rollback(RollbackStatement),
    Explain(Box<Statement>),
    Pragma(PragmaStatement),
    Attach(AttachStatement),
    Detach(DetachStatement),
    Vacuum(VacuumStatement),
    Alter(AlterStatement),
}

#[derive(Clone, Debug)]
pub enum CreateStatement {
    Table {
        if_not_exists: bool,
        name: TableName,
        columns: Vec<ColumnDef>,
        constraints: Vec<TableConstraint>,
        without_rowid: bool,
        strict: bool,
    },
    Index {
        unique: bool,
        if_not_exists: bool,
        name: String,
        table: String,
        columns: Vec<IndexedColumn>,
        where_clause: Option<Expr>,
    },
    View {
        if_not_exists: bool,
        name: TableName,
        columns: Option<Vec<String>>,
        select: Box<SelectStatement>,
    },
    Trigger(CreateTrigger),
}

#[derive(Clone, Debug)]
pub struct CreateTrigger {
    pub name: String,
    pub table: String,
    pub when: TriggerWhen,
    pub events: Vec<TriggerEvent>,
    pub for_each_row: bool,
    pub when_clause: Option<Expr>,
    pub body: Vec<Statement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerWhen {
    Before,
    After,
    InsteadOf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update(Vec<String>),
    Delete,
}

#[derive(Clone, Debug)]
pub struct ColumnDef {
    pub name: String,
    pub type_name: String,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Clone, Debug)]
pub enum ColumnConstraint {
    PrimaryKey { autoincrement: bool, order: Order },
    NotNull,
    Null,
    Unique,
    Check(Expr),
    Default(Expr),
    Collate(String),
    References {
        table: String,
        columns: Vec<String>,
        on_delete: ForeignKeyAction,
        on_update: ForeignKeyAction,
    },
    GeneratedAs {
        expr: Expr,
        stored: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignKeyAction {
    NoAction,
    Restrict,
    SetNull,
    SetDefault,
    Cascade,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Debug)]
pub enum TableConstraint {
    PrimaryKey { columns: Vec<IndexedColumn> },
    Unique(Vec<IndexedColumn>),
    Check(Expr),
    ForeignKey {
        columns: Vec<String>,
        ref_table: String,
        ref_columns: Vec<String>,
        on_delete: ForeignKeyAction,
        on_update: ForeignKeyAction,
    },
}

#[derive(Clone, Debug)]
pub struct IndexedColumn {
    pub name: String,
    pub order: Order,
    pub collation: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DropStatement {
    pub if_exists: bool,
    pub kind: DropKind,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropKind {
    Table,
    Index,
    View,
    Trigger,
}

/// `ALTER TABLE <table> <action>`.
#[derive(Clone, Debug)]
pub struct AlterStatement {
    pub table: String,
    pub action: AlterAction,
}

#[derive(Clone, Debug)]
pub enum AlterAction {
    /// `RENAME TO <new_name>`
    RenameTable { new_name: String },
    /// `ADD [COLUMN] <def>` — SQLite requires a DEFAULT (or a nullable
    /// column); existing rows are back-filled with it.
    AddColumn { column: ColumnDef },
    /// `RENAME COLUMN <old> TO <new>`
    RenameColumn { old: String, new: String },
    /// `DROP COLUMN <name>`
    DropColumn { name: String },
}

/// A qualified table name: `main.users` or just `users`.
#[derive(Clone, Debug)]
pub struct TableName {
    pub schema: Option<String>,
    pub name: String,
}

impl TableName {
    pub fn new(name: impl Into<String>) -> Self {
        Self { schema: None, name: name.into() }
    }
    pub fn qualified(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self { schema: Some(schema.into()), name: name.into() }
    }
}

#[derive(Clone, Debug)]
pub struct InsertStatement {
    pub or: Option<ConflictResolution>,
    pub table: String,
    pub alias: Option<String>,
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
    pub upsert: Option<UpsertClause>,
    pub returning: Option<Vec<ResultColumn>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictResolution {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
}

#[derive(Clone, Debug)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<SelectStatement>),
    DefaultValues,
}

#[derive(Clone, Debug)]
pub struct UpsertClause {
    pub target: Vec<IndexedColumn>,
    pub target_where: Option<Expr>,
    pub action: UpsertAction,
}

#[derive(Clone, Debug)]
pub enum UpsertAction {
    DoNothing,
    DoUpdate {
        set: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
}

#[derive(Clone, Debug)]
pub struct SelectStatement {
    pub with: Option<WithClause>,
    pub body: SelectBody,
    pub order_by: Vec<OrderTerm>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum SelectBody {
    /// `SELECT ... FROM ...`
    Simple(SimpleSelect),
    /// `lhs UNION [ALL] rhs`
    Binary { op: SetOp, left: Box<SelectBody>, right: Box<SelectBody> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    UnionAll,
    Intersect,
    Except,
}

#[derive(Clone, Debug)]
pub struct SimpleSelect {
    pub distinct: bool,
    pub columns: Vec<ResultColumn>,
    pub from: Option<TableExpression>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub window: Vec<WindowDef>,
}

#[derive(Clone, Debug)]
pub enum ResultColumn {
    Star,
    TableStar(String),
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Clone, Debug)]
pub enum TableExpression {
    Table {
        name: String,
        schema: Option<String>,
        alias: Option<String>,
        indexed: Option<IndexedHint>,
    },
    Subquery {
        select: Box<SelectStatement>,
        alias: Option<String>,
        column_aliases: Option<Vec<String>>,
    },
    Join {
        left: Box<TableExpression>,
        right: Box<TableExpression>,
        join_type: JoinType,
        constraint: JoinConstraint,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Clone, Debug)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<String>),
    Natural,
    None,
}

#[derive(Clone, Debug)]
pub enum IndexedHint {
    Indexed(String),
    NotIndexed,
}

#[derive(Clone, Debug)]
pub struct OrderTerm {
    pub expr: Expr,
    pub order: Order,
    pub nulls: NullsOrder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NullsOrder {
    #[default]
    First,
    Last,
    Default,
}

#[derive(Clone, Debug)]
pub struct WithClause {
    pub recursive: bool,
    pub ctes: Vec<Cte>,
}

#[derive(Clone, Debug)]
pub struct Cte {
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub select: Box<SelectStatement>,
    pub materialized: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct WindowDef {
    pub name: String,
    pub base: Option<String>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderTerm>,
    pub frame: Option<Box<WindowFrame>>,
}

#[derive(Clone, Debug)]
pub struct WindowFrame {
    pub kind: FrameKind,
    pub start: Box<FrameBound>,
    pub end: Option<Box<FrameBound>>,
    pub exclude: FrameExclude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Range,
    Rows,
    Groups,
}

#[derive(Clone, Debug)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FrameExclude {
    #[default]
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

#[derive(Clone, Debug)]
pub struct UpdateStatement {
    pub or: Option<ConflictResolution>,
    pub table: String,
    pub alias: Option<String>,
    pub set: Vec<(String, Expr)>,
    pub from: Option<TableExpression>,
    pub where_clause: Option<Expr>,
    pub returning: Option<Vec<ResultColumn>>,
}

#[derive(Clone, Debug)]
pub struct DeleteStatement {
    pub from: String,
    pub alias: Option<String>,
    pub where_clause: Option<Expr>,
    pub returning: Option<Vec<ResultColumn>>,
    pub limit: Option<Expr>,
    pub order_by: Vec<OrderTerm>,
}

#[derive(Clone, Debug)]
pub struct BeginStatement {
    pub mode: BeginMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginMode {
    Deferred,
    Immediate,
    Exclusive,
}

#[derive(Clone, Debug)]
pub struct RollbackStatement {
    pub savepoint: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PragmaStatement {
    pub schema: Option<String>,
    pub name: String,
    pub value: Option<PragmaValue>,
}

#[derive(Clone, Debug)]
pub enum PragmaValue {
    Expr(Expr),
    Call(Expr),
}

#[derive(Clone, Debug)]
pub struct AttachStatement {
    pub expr: Expr,
    pub schema: String,
}

#[derive(Clone, Debug)]
pub struct DetachStatement {
    pub schema: String,
}

#[derive(Clone, Debug)]
pub struct VacuumStatement {
    pub schema: Option<String>,
    pub into: Option<String>,
}

// ============================================================================
// Expressions
// ============================================================================

/// A SQL expression.
#[derive(Clone, Debug)]
pub enum Expr {
    /// A literal value.
    Literal(Value),
    /// A bound parameter (`?`, `:name`, `@col`, `$var`).
    Parameter(String),
    /// A column reference: `table.column` or just `column`.
    Column { table: Option<String>, name: String },
    /// `expr op expr`
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    /// `op expr` (unary)
    Unary { op: UnaryOp, expr: Box<Expr> },
    /// `expr BETWEEN low AND high`
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },
    /// `expr IN (values|subquery)`
    In { expr: Box<Expr>, source: InSource, negated: bool },
    /// `expr LIKE pattern` (also GLOB, REGEXP, MATCH)
    Like { op: LikeOp, expr: Box<Expr>, pattern: Box<Expr>, escape: Option<Box<Expr>>, negated: bool },
    /// `expr IS NULL` / `expr IS NOT NULL`
    IsNull { expr: Box<Expr>, negated: bool },
    /// `expr IS [NOT] expr2`
    Is { left: Box<Expr>, right: Box<Expr>, negated: bool },
    /// Function call: `func(args...)` or `func(DISTINCT args...)`
    Function {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
        filter: Option<Box<Expr>>,
        over: Option<Box<WindowSpec>>,
    },
    /// `CASE WHEN ... THEN ... ELSE ... END` or `CASE expr WHEN ... THEN ... END`
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// `(expr1, expr2, ...)` — a row value constructor.
    Row(Vec<Expr>),
    /// `(subquery)`
    Subquery(Box<SelectStatement>),
    /// `EXISTS (subquery)`
    Exists(Box<SelectStatement>),
    /// `CAST(expr AS type)`
    Cast { expr: Box<Expr>, type_name: String },
    /// `expr COLLATE collation`
    Collate { expr: Box<Expr>, collation: String },
    /// `Raise(action, message)`
    Raise { action: RaiseAction, message: Option<Box<Expr>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// String concatenation `||`
    Concat,
    BitAnd,
    BitOr,
    BitXor,
    ShiftLeft,
    ShiftRight,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl BinaryOp {
    pub fn precedence(&self) -> u8 {
        use BinaryOp::*;
        match self {
            Or => 1,
            And => 2,
            Eq | NotEq | Lt | LtEq | Gt | GtEq => 3,
            BitOr => 4,
            BitXor => 5,
            BitAnd => 6,
            ShiftLeft | ShiftRight => 7,
            Add | Sub | Concat => 8,
            Mul | Div | Mod => 9,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Pos,
    Not,
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LikeOp {
    Like,
    Glob,
    Regexp,
    Match,
}

#[derive(Clone, Debug)]
pub enum InSource {
    List(Vec<Expr>),
    Subquery(Box<SelectStatement>),
    Table(String),
}

#[derive(Clone, Debug)]
pub enum WindowSpec {
    Named(String),
    Inline(Box<WindowDef>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseAction {
    Ignore,
    Rollback,
    Abort,
    Fail,
}
