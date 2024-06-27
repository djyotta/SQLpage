use super::csv_import::{extract_csv_copy_statement, CsvImport};
use super::sqlpage_functions::functions::SqlPageFunctionName;
use super::sqlpage_functions::{are_params_extractable, func_call_to_param};
use super::syntax_tree::StmtParam;
use crate::file_cache::AsyncFromStrWithState;
use crate::{AppState, Database};
use anyhow::Context;
use async_trait::async_trait;
use sqlparser::ast::{
    BinaryOperator, CastKind, CharacterLength, DataType, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArgumentList, FunctionArguments, Ident, ObjectName,
    OneOrManyWithParens, SelectItem, Statement, Value, VisitMut, VisitorMut,
};
use sqlparser::dialect::{Dialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token::{SemiColon, EOF};
use sqlparser::tokenizer::Tokenizer;
use sqlx::any::AnyKind;
use std::fmt::Write;
use std::ops::ControlFlow;
use std::str::FromStr;

#[derive(Default)]
pub struct ParsedSqlFile {
    pub(super) statements: Vec<ParsedStatement>,
}

impl ParsedSqlFile {
    #[must_use]
    pub fn new(db: &Database, sql: &str) -> ParsedSqlFile {
        let dialect = dialect_for_db(db.connection.any_kind());
        let parsed_statements = match parse_sql(dialect.as_ref(), sql) {
            Ok(parsed) => parsed,
            Err(err) => return Self::from_err(err),
        };
        let statements = parsed_statements.collect();
        ParsedSqlFile { statements }
    }

    fn from_err(e: impl Into<anyhow::Error>) -> Self {
        Self {
            statements: vec![ParsedStatement::Error(
                e.into().context("SQLPage could not parse the SQL file"),
            )],
        }
    }
}

#[async_trait(? Send)]
impl AsyncFromStrWithState for ParsedSqlFile {
    async fn from_str_with_state(app_state: &AppState, source: &str) -> anyhow::Result<Self> {
        Ok(ParsedSqlFile::new(&app_state.db, source))
    }
}

/// A single SQL statement that has been parsed from a SQL file.
#[derive(Debug, PartialEq)]
pub(super) struct StmtWithParams {
    /// The SQL query with placeholders for parameters.
    pub query: String,
    /// Parameters that should be bound to the query.
    /// They can contain functions that will be called before the query is executed,
    /// the result of which will be bound to the query.
    pub params: Vec<StmtParam>,
    /// Functions that are called on the result set after the query has been executed,
    /// and which can be passed the result of the query as an argument.
    pub delayed_functions: Vec<DelayedFunctionCall>,
}

#[derive(Debug)]
pub(super) enum ParsedStatement {
    StmtWithParams(StmtWithParams),
    StaticSimpleSelect(Vec<(String, SimpleSelectValue)>),
    SetVariable {
        variable: StmtParam,
        value: StmtWithParams,
    },
    CsvImport(CsvImport),
    Error(anyhow::Error),
}

#[derive(Debug, PartialEq)]
pub(super) enum SimpleSelectValue {
    Static(serde_json::Value),
    Dynamic(StmtParam),
}

fn parse_sql<'a>(
    dialect: &'a dyn Dialect,
    sql: &'a str,
) -> anyhow::Result<impl Iterator<Item = ParsedStatement> + 'a> {
    log::trace!("Parsing SQL: {sql}");
    let tokens = Tokenizer::new(dialect, sql)
        .tokenize_with_location()
        .with_context(|| "SQLPage's SQL parser could not tokenize the sql file")?;
    let mut parser = Parser::new(dialect).with_tokens_with_locations(tokens);
    let db_kind = kind_of_dialect(dialect);
    Ok(std::iter::from_fn(move || {
        parse_single_statement(&mut parser, db_kind)
    }))
}

fn parse_single_statement(parser: &mut Parser<'_>, db_kind: AnyKind) -> Option<ParsedStatement> {
    if parser.peek_token() == EOF {
        return None;
    }
    let mut stmt = match parser.parse_statement() {
        Ok(stmt) => stmt,
        Err(err) => return Some(syntax_error(err, parser)),
    };
    log::debug!("Parsed statement: {stmt}");
    let mut semicolon = false;
    while parser.consume_token(&SemiColon) {
        semicolon = true;
    }
    let mut params = ParameterExtractor::extract_parameters(&mut stmt, db_kind);
    if let Some((variable, query)) = extract_set_variable(&mut stmt) {
        return Some(ParsedStatement::SetVariable {
            variable,
            value: StmtWithParams {
                query,
                params,
                delayed_functions: Vec::new(),
            },
        });
    }
    if let Some(csv_import) = extract_csv_copy_statement(&mut stmt) {
        return Some(ParsedStatement::CsvImport(csv_import));
    }
    if let Some(static_statement) = extract_static_simple_select(&stmt, &params) {
        log::debug!("Optimised a static simple select to avoid a trivial database query: {stmt} optimized to {static_statement:?}");
        return Some(ParsedStatement::StaticSimpleSelect(static_statement));
    }
    let delayed_functions = extract_toplevel_functions(&mut stmt);
    remove_invalid_function_calls(&mut stmt, &mut params);
    let query = format!(
        "{stmt}{semicolon}",
        semicolon = if semicolon { ";" } else { "" }
    );
    log::debug!("Final transformed statement: {stmt}");
    Some(ParsedStatement::StmtWithParams(StmtWithParams {
        query,
        params,
        delayed_functions,
    }))
}

fn syntax_error(err: ParserError, parser: &mut Parser) -> ParsedStatement {
    let mut err_msg = String::with_capacity(128);
    parser.prev_token(); // go back to the token that caused the error
    for i in 0..32 {
        let next_token = parser.next_token();
        if i == 0 {
            writeln!(
                &mut err_msg,
                "SQLPage found a syntax error on line {}, character {}:",
                next_token.location.line, next_token.location.column
            )
            .unwrap();
        }
        if next_token == EOF {
            break;
        }
        write!(&mut err_msg, "{next_token} ").unwrap();
    }
    ParsedStatement::Error(anyhow::Error::from(err).context(err_msg))
}

fn dialect_for_db(db_kind: AnyKind) -> Box<dyn Dialect> {
    match db_kind {
        AnyKind::Postgres => Box::new(PostgreSqlDialect {}),
        AnyKind::Mssql => Box::new(MsSqlDialect {}),
        AnyKind::MySql => Box::new(MySqlDialect {}),
        AnyKind::Sqlite => Box::new(SQLiteDialect {}),
    }
}

fn kind_of_dialect(dialect: &dyn Dialect) -> AnyKind {
    if dialect.is::<PostgreSqlDialect>() {
        AnyKind::Postgres
    } else if dialect.is::<MsSqlDialect>() {
        AnyKind::Mssql
    } else if dialect.is::<MySqlDialect>() {
        AnyKind::MySql
    } else if dialect.is::<SQLiteDialect>() {
        AnyKind::Sqlite
    } else {
        unreachable!("Unknown dialect")
    }
}

fn map_param(mut name: String) -> StmtParam {
    if name.is_empty() {
        return StmtParam::PostOrGet(name);
    }
    let prefix = name.remove(0);
    match prefix {
        '$' => StmtParam::PostOrGet(name),
        ':' => StmtParam::Post(name),
        _ => StmtParam::Get(name),
    }
}

#[derive(Debug, PartialEq)]
pub struct DelayedFunctionCall {
    pub function: SqlPageFunctionName,
    pub argument_col_names: Vec<String>,
    pub target_col_name: String,
}

/// The execution of top-level functions is delayed until after the query has been executed.
/// For instance, `SELECT sqlpage.fetch(x) FROM t` will be executed as `SELECT x as _sqlpage_f0_a0 FROM t`
/// and the `sqlpage.fetch` function will be called with the value of `_sqlpage_f0_a0` after the query has been executed,
/// on each row of the result set.
fn extract_toplevel_functions(stmt: &mut Statement) -> Vec<DelayedFunctionCall> {
    let mut delayed_function_calls: Vec<DelayedFunctionCall> = Vec::new();
    let set_expr = match stmt {
        Statement::Query(q) => q.body.as_mut(),
        _ => return delayed_function_calls,
    };
    let select_items = match set_expr {
        sqlparser::ast::SetExpr::Select(s) => &mut s.projection,
        _ => return delayed_function_calls,
    };
    let mut select_items_to_add = Vec::new();
    for select_item in select_items.iter_mut() {
        match select_item {
            SelectItem::ExprWithAlias {
                expr:
                    Expr::Function(Function {
                        name: ObjectName(func_name_parts),
                        args:
                            FunctionArguments::List(FunctionArgumentList {
                                args,
                                duplicate_treatment: None,
                                ..
                            }),
                        ..
                    }),
                alias,
            } => {
                if let Some(func_name) = extract_sqlpage_function_name(func_name_parts) {
                    func_name_parts.clear(); // mark the function for deletion
                    let mut argument_col_names = Vec::with_capacity(args.len());
                    for (arg_idx, arg) in args.iter_mut().enumerate() {
                        match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                            | FunctionArg::Named {
                                arg: FunctionArgExpr::Expr(expr),
                                ..
                            } => {
                                let func_idx = delayed_function_calls.len();
                                let argument_col_name = format!("_sqlpage_f{func_idx}_a{arg_idx}");
                                argument_col_names.push(argument_col_name.clone());
                                select_items_to_add.push(SelectItem::ExprWithAlias {
                                    expr: std::mem::replace(expr, Expr::Value(Value::Null)),
                                    alias: Ident::new(argument_col_name),
                                });
                            }
                            other => {
                                log::error!("Unsupported argument to {func_name}: {other}");
                            }
                        }
                    }
                    delayed_function_calls.push(DelayedFunctionCall {
                        function: func_name,
                        argument_col_names,
                        target_col_name: alias.value.clone(),
                    });
                }
            }
            _ => continue,
        }
    }
    // Now remove the function calls from the select items
    select_items.retain(|item| {
        !matches!(item, SelectItem::ExprWithAlias {
            expr:
                Expr::Function(Function {
                    name: ObjectName(func_name_parts),
                    ..
                }),
            ..
        } if func_name_parts.is_empty())
    });
    // And add the new select items
    select_items.extend(select_items_to_add);
    delayed_function_calls
}

fn extract_static_simple_select(
    stmt: &Statement,
    params: &[StmtParam],
) -> Option<Vec<(String, SimpleSelectValue)>> {
    let set_expr = match stmt {
        Statement::Query(q)
            if q.limit.is_none()
                && q.fetch.is_none()
                && q.order_by.is_empty()
                && q.with.is_none()
                && q.offset.is_none()
                && q.locks.is_empty() =>
        {
            q.body.as_ref()
        }
        _ => return None,
    };
    let select_items = match set_expr {
        sqlparser::ast::SetExpr::Select(s)
            if s.cluster_by.is_empty()
                && s.distinct.is_none()
                && s.distribute_by.is_empty()
                && s.from.is_empty()
                && matches!(&s.group_by, sqlparser::ast::GroupByExpr::Expressions(e) if e.is_empty())
                && s.having.is_none()
                && s.into.is_none()
                && s.lateral_views.is_empty()
                && s.named_window.is_empty()
                && s.qualify.is_none()
                && s.selection.is_none()
                && s.sort_by.is_empty()
                && s.top.is_none() =>
        {
            &s.projection
        }
        _ => return None,
    };
    let mut items = Vec::with_capacity(select_items.len());
    let mut params_iter = params.iter().cloned();
    for select_item in select_items {
        use serde_json::Value::{Bool, Null, Number, String};
        use SimpleSelectValue::{Dynamic, Static};
        let sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } = select_item else {
            return None;
        };
        let value = match expr {
            Expr::Value(Value::Boolean(b)) => Static(Bool(*b)),
            Expr::Value(Value::Number(n, _)) => Static(Number(n.parse().ok()?)),
            Expr::Value(Value::SingleQuotedString(s)) => Static(String(s.clone())),
            Expr::Value(Value::Null) => Static(Null),
            e if is_simple_select_placeholder(e) => {
                if let Some(p) = params_iter.next() {
                    Dynamic(p)
                } else {
                    log::error!("Parameter not extracted for placehorder: {expr:?}");
                    return None;
                }
            }
            other => {
                log::trace!("Cancelling simple select optimization because of expr: {other:?}");
                return None;
            }
        };
        let key = alias.value.clone();
        items.push((key, value));
    }
    if let Some(p) = params_iter.next() {
        log::error!("static select extraction failed because of extraneous parameter: {p:?}");
        return None;
    }
    Some(items)
}

fn is_simple_select_placeholder(e: &Expr) -> bool {
    match e {
        Expr::Value(Value::Placeholder(_)) => true,
        Expr::Cast {
            expr,
            data_type: DataType::Text | DataType::Varchar(_) | DataType::Char(_),
            format: None,
            kind: CastKind::Cast,
        } if is_simple_select_placeholder(expr) => true,
        _ => false,
    }
}

fn extract_set_variable(stmt: &mut Statement) -> Option<(StmtParam, String)> {
    if let Statement::SetVariable {
        variables: OneOrManyWithParens::One(ObjectName(name)),
        value,
        local: false,
        hivevar: false,
    } = stmt
    {
        if let ([ident], [value]) = (name.as_mut_slice(), value.as_mut_slice()) {
            let variable = if let Some(variable) = extract_ident_param(ident) {
                variable
            } else {
                StmtParam::PostOrGet(std::mem::take(&mut ident.value))
            };
            return Some((variable, format!("SELECT {value}")));
        }
    }
    None
}

struct ParameterExtractor {
    db_kind: AnyKind,
    parameters: Vec<StmtParam>,
}

const PLACEHOLDER_PREFIXES: [(AnyKind, &str); 2] =
    [(AnyKind::Postgres, "$"), (AnyKind::Mssql, "@p")];
const DEFAULT_PLACEHOLDER: &str = "?";

impl ParameterExtractor {
    fn extract_parameters(
        sql_ast: &mut sqlparser::ast::Statement,
        db_kind: AnyKind,
    ) -> Vec<StmtParam> {
        let mut this = Self {
            db_kind,
            parameters: vec![],
        };
        sql_ast.visit(&mut this);
        this.parameters
    }

    fn make_placeholder(&self) -> Expr {
        let name = make_placeholder(self.db_kind, self.parameters.len() + 1);
        // We cast our placeholders to TEXT even though we always bind TEXT data to them anyway
        // because that helps the database engine to prepare the query.
        // For instance in PostgreSQL, the query planner will not be able to use an index on a
        // column if the column is compared to a placeholder of type VARCHAR, but it will be able
        // to use the index if the column is compared to a placeholder of type TEXT.
        let data_type = match self.db_kind {
            // MySQL requires CAST(? AS CHAR) and does not understand CAST(? AS TEXT)
            AnyKind::MySql => DataType::Char(None),
            AnyKind::Mssql => DataType::Varchar(Some(CharacterLength::Max)),
            _ => DataType::Text,
        };
        let value = Expr::Value(Value::Placeholder(name));
        Expr::Cast {
            expr: Box::new(value),
            data_type,
            format: None,
            kind: CastKind::Cast,
        }
    }

    fn handle_builtin_function(
        &mut self,
        func_name: &str,
        mut arguments: Vec<FunctionArg>,
    ) -> Expr {
        #[allow(clippy::single_match_else)]
        let placeholder = self.make_placeholder();
        let param = func_call_to_param(func_name, &mut arguments);
        self.parameters.push(param);
        placeholder
    }

    fn is_own_placeholder(&self, param: &str) -> bool {
        if let Some((_, prefix)) = PLACEHOLDER_PREFIXES
            .iter()
            .find(|(kind, _prefix)| *kind == self.db_kind)
        {
            if let Some(param) = param.strip_prefix(prefix) {
                if let Ok(index) = param.parse::<usize>() {
                    return index <= self.parameters.len() + 1;
                }
            }
            return false;
        }
        param == DEFAULT_PLACEHOLDER
    }
}

struct BadFunctionRemover;
impl VisitorMut for BadFunctionRemover {
    type Break = StmtParam;
    fn pre_visit_expr(&mut self, value: &mut Expr) -> ControlFlow<Self::Break> {
        match value {
            Expr::Function(Function {
                name: ObjectName(func_name_parts),
                args:
                    FunctionArguments::List(FunctionArgumentList {
                        args,
                        duplicate_treatment: None,
                        ..
                    }),
                ..
            }) if is_sqlpage_func(func_name_parts) => {
                let func_name = sqlpage_func_name(func_name_parts);
                log::error!("Invalid function call to sqlpage.{func_name}. SQLPage function arguments must be static if the function is not at the top level of a select statement.");
                let mut arguments = std::mem::take(args);
                let param = func_call_to_param(func_name, &mut arguments);
                return ControlFlow::Break(param);
            }
            _ => (),
        }
        ControlFlow::Continue(())
    }
}

fn remove_invalid_function_calls(stmt: &mut Statement, params: &mut Vec<StmtParam>) {
    let mut remover = BadFunctionRemover;
    if let ControlFlow::Break(param) = stmt.visit(&mut remover) {
        params.push(param);
    }
}

/** This is a helper struct to format a list of arguments for an error message. */
pub(super) struct FormatArguments<'a>(pub &'a [FunctionArg]);
impl std::fmt::Display for FormatArguments<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut args = self.0.iter();
        if let Some(arg) = args.next() {
            write!(f, "{arg}")?;
        }
        for arg in args {
            write!(f, ", {arg}")?;
        }
        Ok(())
    }
}

pub(super) fn function_arg_to_stmt_param(arg: &mut FunctionArg) -> Option<StmtParam> {
    function_arg_expr(arg).and_then(expr_to_stmt_param)
}

pub(super) fn function_args_to_stmt_params(
    arguments: &mut [FunctionArg],
) -> anyhow::Result<Vec<StmtParam>> {
    arguments
        .iter_mut()
        .map(|arg| {
            function_arg_to_stmt_param(arg)
                .ok_or_else(|| anyhow::anyhow!("Passing \"{arg}\" as a function argument is not supported.\n\n\
                    The only supported sqlpage function argument types are : \n\
                      - variables (such as $my_variable), \n\
                      - other sqlpage function calls (such as sqlpage.cookie('my_cookie')), \n\
                      - literal strings (such as 'my_string'), \n\
                      - concatenations of the above (such as CONCAT(x, y)).\n\n\
                    Arbitrary SQL expressions as function arguments are not supported.\n\
                    Try executing the SQL expression in a separate SET expression, then passing it to the function:\n\n\
                    SET $my_parameter = {arg}; \n\
                    SELECT sqlpage.my_function($my_parameter);\n\n\
                    "))
        })
        .collect::<anyhow::Result<Vec<_>>>()
}

fn expr_to_stmt_param(arg: &mut Expr) -> Option<StmtParam> {
    match arg {
        Expr::Value(Value::Placeholder(placeholder)) => {
            Some(map_param(std::mem::take(placeholder)))
        }
        Expr::Identifier(ident) => extract_ident_param(ident),
        Expr::Function(Function {
            name: ObjectName(func_name_parts),
            args:
                FunctionArguments::List(FunctionArgumentList {
                    args,
                    duplicate_treatment: None,
                    ..
                }),
            ..
        }) if is_sqlpage_func(func_name_parts) => Some(func_call_to_param(
            sqlpage_func_name(func_name_parts),
            args.as_mut_slice(),
        )),
        Expr::Value(Value::SingleQuotedString(param_value)) => {
            Some(StmtParam::Literal(std::mem::take(param_value)))
        }
        Expr::Value(Value::Number(param_value, _is_long)) => {
            Some(StmtParam::Literal(param_value.clone()))
        }
        Expr::Value(Value::Null) => Some(StmtParam::Null),
        Expr::BinaryOp {
            // 'str1' || 'str2'
            left,
            op: BinaryOperator::StringConcat,
            right,
        } => {
            let left = expr_to_stmt_param(left)?;
            let right = expr_to_stmt_param(right)?;
            Some(StmtParam::Concat(vec![left, right]))
        }
        // CONCAT('str1', 'str2', ...)
        Expr::Function(Function {
            name: ObjectName(func_name_parts),
            args:
                FunctionArguments::List(FunctionArgumentList {
                    args,
                    duplicate_treatment: None,
                    ..
                }),
            ..
        }) if func_name_parts.len() == 1
            && func_name_parts[0].value.eq_ignore_ascii_case("concat") =>
        {
            let mut concat_args = Vec::with_capacity(args.len());
            for arg in args {
                concat_args.push(function_arg_to_stmt_param(arg)?);
            }
            Some(StmtParam::Concat(concat_args))
        }
        _ => {
            log::warn!("Unsupported function argument: {arg}");
            None
        }
    }
}

fn function_arg_expr(arg: &mut FunctionArg) -> Option<&mut Expr> {
    match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
        other => {
            log::warn!(
                "Using named function arguments ({other}) is not supported by SQLPage functions."
            );
            None
        }
    }
}

#[inline]
#[must_use]
pub fn make_placeholder(db_kind: AnyKind, arg_number: usize) -> String {
    if let Some((_, prefix)) = PLACEHOLDER_PREFIXES
        .iter()
        .find(|(kind, _)| *kind == db_kind)
    {
        return format!("{prefix}{arg_number}");
    }
    DEFAULT_PLACEHOLDER.to_string()
}

fn extract_ident_param(Ident { value, .. }: &mut Ident) -> Option<StmtParam> {
    if value.starts_with('$') || value.starts_with(':') {
        let name = std::mem::take(value);
        Some(map_param(name))
    } else {
        None
    }
}

impl VisitorMut for ParameterExtractor {
    type Break = ();
    fn pre_visit_expr(&mut self, value: &mut Expr) -> ControlFlow<Self::Break> {
        match value {
            Expr::Identifier(ident) => {
                if let Some(param) = extract_ident_param(ident) {
                    *value = self.make_placeholder();
                    self.parameters.push(param);
                }
            }
            Expr::Value(Value::Placeholder(param)) if !self.is_own_placeholder(param) =>
            // this check is to avoid recursively replacing placeholders in the form of '?', or '$1', '$2', which we emit ourselves
            {
                let new_expr = self.make_placeholder();
                let name = std::mem::take(param);
                self.parameters.push(map_param(name));
                *value = new_expr;
            }
            Expr::Function(Function {
                name: ObjectName(func_name_parts),
                args:
                    FunctionArguments::List(FunctionArgumentList {
                        args,
                        duplicate_treatment: None,
                        ..
                    }),
                filter: None,
                null_treatment: None,
                over: None,
                ..
            }) if is_sqlpage_func(func_name_parts) && are_params_extractable(args) => {
                let func_name = sqlpage_func_name(func_name_parts);
                log::trace!("Handling builtin function: {func_name}");
                let arguments = std::mem::take(args);
                *value = self.handle_builtin_function(func_name, arguments);
            }
            // Replace 'str1' || 'str2' with CONCAT('str1', 'str2') for MSSQL
            Expr::BinaryOp {
                left,
                op: BinaryOperator::StringConcat,
                right,
            } if self.db_kind == AnyKind::Mssql => {
                let left = std::mem::replace(left.as_mut(), Expr::Value(Value::Null));
                let right = std::mem::replace(right.as_mut(), Expr::Value(Value::Null));
                *value = Expr::Function(Function {
                    name: ObjectName(vec![Ident::new("CONCAT")]),
                    args: FunctionArguments::List(FunctionArgumentList {
                        args: vec![
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(left)),
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(right)),
                        ],
                        duplicate_treatment: None,
                        clauses: Vec::new(),
                    }),
                    over: None,
                    filter: None,
                    null_treatment: None,
                    within_group: Vec::new(),
                });
            }
            Expr::Cast {
                kind: kind @ CastKind::DoubleColon,
                ..
            } if self.db_kind != AnyKind::Postgres => {
                log::warn!("Casting with '::' is not supported on your database. \
                For backwards compatibility with older SQLPage versions, we will transform it to CAST(... AS ...).");
                *kind = CastKind::Cast;
            }
            _ => (),
        }
        ControlFlow::<()>::Continue(())
    }
}

const SQLPAGE_FUNCTION_NAMESPACE: &str = "sqlpage";

fn is_sqlpage_func(func_name_parts: &[Ident]) -> bool {
    if let [Ident { value, .. }, Ident { .. }] = func_name_parts {
        value == SQLPAGE_FUNCTION_NAMESPACE
    } else {
        false
    }
}

fn extract_sqlpage_function_name(func_name_parts: &[Ident]) -> Option<SqlPageFunctionName> {
    if let [Ident {
        value: namespace, ..
    }, Ident { value, .. }] = func_name_parts
    {
        if namespace == SQLPAGE_FUNCTION_NAMESPACE {
            return SqlPageFunctionName::from_str(value).ok();
        }
    }
    None
}

fn sqlpage_func_name(func_name_parts: &[Ident]) -> &str {
    if let [Ident { .. }, Ident { value, .. }] = func_name_parts {
        value
    } else {
        debug_assert!(
            false,
            "sqlpage function name should have been checked by is_sqlpage_func"
        );
        ""
    }
}

#[cfg(test)]
mod test {
    use super::super::sqlpage_functions::functions::SqlPageFunctionName;
    use super::super::syntax_tree::SqlPageFunctionCall;

    use super::*;

    fn parse_stmt(sql: &str, dialect: &dyn Dialect) -> Statement {
        let mut ast = Parser::parse_sql(dialect, sql).unwrap();
        assert_eq!(ast.len(), 1);
        ast.pop().unwrap()
    }

    fn parse_postgres_stmt(sql: &str) -> Statement {
        parse_stmt(sql, &PostgreSqlDialect {})
    }

    #[test]
    fn test_statement_rewrite() {
        let mut ast =
            parse_postgres_stmt("select $a from t where $x > $a OR $x = sqlpage.cookie('cookoo')");
        let parameters = ParameterExtractor::extract_parameters(&mut ast, AnyKind::Postgres);
        assert_eq!(
        ast.to_string(),
        "SELECT CAST($1 AS TEXT) FROM t WHERE CAST($2 AS TEXT) > CAST($3 AS TEXT) OR CAST($4 AS TEXT) = CAST($5 AS TEXT)"
    );
        assert_eq!(
            parameters,
            [
                StmtParam::PostOrGet("a".to_string()),
                StmtParam::PostOrGet("x".to_string()),
                StmtParam::PostOrGet("a".to_string()),
                StmtParam::PostOrGet("x".to_string()),
                StmtParam::FunctionCall(SqlPageFunctionCall {
                    function: SqlPageFunctionName::cookie,
                    arguments: vec![StmtParam::Literal("cookoo".to_string())]
                }),
            ]
        );
    }

    #[test]
    fn test_statement_rewrite_sqlite() {
        let mut ast = parse_stmt("select $x, :y from t", &SQLiteDialect {});
        let parameters = ParameterExtractor::extract_parameters(&mut ast, AnyKind::Sqlite);
        assert_eq!(
            ast.to_string(),
            "SELECT CAST(? AS TEXT), CAST(? AS TEXT) FROM t"
        );
        assert_eq!(
            parameters,
            [
                StmtParam::PostOrGet("x".to_string()),
                StmtParam::Post("y".to_string()),
            ]
        );
    }

    const ALL_DIALECTS: &[(&dyn Dialect, AnyKind)] = &[
        (&PostgreSqlDialect {}, AnyKind::Postgres),
        (&MsSqlDialect {}, AnyKind::Mssql),
        (&MySqlDialect {}, AnyKind::MySql),
        (&SQLiteDialect {}, AnyKind::Sqlite),
    ];

    #[test]
    fn test_extract_toplevel_delayed_functions() {
        let mut ast = parse_stmt(
            "select sqlpage.fetch($x) as x, sqlpage.persist_uploaded_file('a', 'b') as y from t",
            &PostgreSqlDialect {},
        );
        let functions = extract_toplevel_functions(&mut ast);
        assert_eq!(
            ast.to_string(),
            "SELECT $x AS _sqlpage_f0_a0, 'a' AS _sqlpage_f1_a0, 'b' AS _sqlpage_f1_a1 FROM t"
        );
        assert_eq!(
            functions,
            vec![
                DelayedFunctionCall {
                    function: SqlPageFunctionName::fetch,
                    argument_col_names: vec!["_sqlpage_f0_a0".to_string()],
                    target_col_name: "x".to_string()
                },
                DelayedFunctionCall {
                    function: SqlPageFunctionName::persist_uploaded_file,
                    argument_col_names: vec![
                        "_sqlpage_f1_a0".to_string(),
                        "_sqlpage_f1_a1".to_string()
                    ],
                    target_col_name: "y".to_string()
                }
            ]
        );
    }

    #[test]
    fn test_sqlpage_function_with_argument() {
        for &(dialect, kind) in ALL_DIALECTS {
            let mut ast = parse_stmt("select sqlpage.fetch($x)", dialect);
            let parameters = ParameterExtractor::extract_parameters(&mut ast, kind);
            assert_eq!(
                parameters,
                [StmtParam::FunctionCall(SqlPageFunctionCall {
                    function: SqlPageFunctionName::fetch,
                    arguments: vec![StmtParam::PostOrGet("x".to_string())]
                })],
                "Failed for dialect {dialect:?}"
            );
        }
    }

    #[test]
    fn test_set_variable() {
        let sql = "set x = $y";
        for &(dialect, db_kind) in ALL_DIALECTS {
            let mut parser = Parser::new(dialect).try_with_sql(sql).unwrap();
            let stmt = parse_single_statement(&mut parser, db_kind);
            if let Some(ParsedStatement::SetVariable {
                variable,
                value: StmtWithParams { query, params, .. },
            }) = stmt
            {
                assert_eq!(
                    variable,
                    StmtParam::PostOrGet("x".to_string()),
                    "{dialect:?}"
                );
                assert!(query.starts_with("SELECT "));
                assert_eq!(params, [StmtParam::PostOrGet("y".to_string())]);
            } else {
                panic!("Failed for dialect {dialect:?}: {stmt:#?}",);
            }
        }
    }

    #[test]
    fn is_own_placeholder() {
        assert!(ParameterExtractor {
            db_kind: AnyKind::Postgres,
            parameters: vec![]
        }
        .is_own_placeholder("$1"));

        assert!(ParameterExtractor {
            db_kind: AnyKind::Postgres,
            parameters: vec![StmtParam::Get('x'.to_string())]
        }
        .is_own_placeholder("$2"));

        assert!(!ParameterExtractor {
            db_kind: AnyKind::Postgres,
            parameters: vec![]
        }
        .is_own_placeholder("$2"));

        assert!(ParameterExtractor {
            db_kind: AnyKind::Sqlite,
            parameters: vec![]
        }
        .is_own_placeholder("?"));

        assert!(!ParameterExtractor {
            db_kind: AnyKind::Sqlite,
            parameters: vec![]
        }
        .is_own_placeholder("$1"));
    }

    #[test]
    fn test_mssql_statement_rewrite() {
        let mut ast = parse_stmt(
            "select '' || $1 from [a schema].[a table]",
            &MsSqlDialect {},
        );
        let parameters = ParameterExtractor::extract_parameters(&mut ast, AnyKind::Mssql);
        assert_eq!(
            ast.to_string(),
            "SELECT CONCAT('', CAST(@p1 AS VARCHAR(MAX))) FROM [a schema].[a table]"
        );
        assert_eq!(parameters, [StmtParam::PostOrGet("1".to_string()),]);
    }

    #[test]
    fn test_static_extract() {
        use SimpleSelectValue::Static;

        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt(
                    "select 'hello' as hello, 42 as answer, null as nothing, 'world' as hello"
                ),
                &[]
            ),
            Some(vec![
                ("hello".into(), Static("hello".into())),
                ("answer".into(), Static(42.into())),
                ("nothing".into(), Static(().into())),
                ("hello".into(), Static("world".into())),
            ])
        );
    }

    #[test]
    fn test_simple_select_with_sqlpage_pseudofunction() {
        let sql = "select 'text' as component, $x as contents, $y as title";
        let dialects: &[&dyn Dialect] = &[
            &PostgreSqlDialect {},
            &SQLiteDialect {},
            &MySqlDialect {},
            &MsSqlDialect {},
        ];
        for &dialect in dialects {
            use SimpleSelectValue::{Dynamic, Static};
            use StmtParam::PostOrGet;

            let parsed: Vec<ParsedStatement> = parse_sql(dialect, sql).unwrap().collect();
            match &parsed[..] {
                [ParsedStatement::StaticSimpleSelect(q)] => assert_eq!(
                    q,
                    &[
                        ("component".into(), Static("text".into())),
                        ("contents".into(), Dynamic(PostOrGet("x".into()))),
                        ("title".into(), Dynamic(PostOrGet("y".into()))),
                    ]
                ),
                other => panic!("failed to extract simple select in {dialect:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn test_simple_select_only_extraction() {
        use SimpleSelectValue::{Dynamic, Static};
        use StmtParam::PostOrGet;
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select 'text' as component, $1 as contents"),
                &[PostOrGet("cook".into())]
            ),
            Some(vec![
                ("component".into(), Static("text".into())),
                ("contents".into(), Dynamic(PostOrGet("cook".into()))),
            ])
        );
    }

    #[test]
    fn test_static_extract_doesnt_match() {
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select 'hello' as hello, 42 as answer limit 0"),
                &[]
            ),
            None
        );
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select 'hello' as hello, 42 as answer order by 1"),
                &[]
            ),
            None
        );
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select 'hello' as hello, 42 as answer offset 1"),
                &[]
            ),
            None
        );
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select 'hello' as hello, 42 as answer where 1 = 0"),
                &[]
            ),
            None
        );
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select 'hello' as hello, 42 as answer FROM t"),
                &[]
            ),
            None
        );
        assert_eq!(
            extract_static_simple_select(
                &parse_postgres_stmt("select x'CAFEBABE' as hello, 42 as answer"),
                &[]
            ),
            None
        );
    }
}
