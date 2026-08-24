//! Fork delta (sightline oracle): reveal unannotated function returns from body
//! inference instead of `Unknown`.
//!
//! pyright reveals the *inferred* return of an unannotated function — `(path) -> Any`
//! for a `json.loads` passthrough, `(q: str) -> int` for a `str.count` predicate — and
//! sightline's #36/#40 oracle arms consume exactly that distinction. ty's signatures
//! leave unannotated returns `Unknown`, so `report_revealed_type` patches the revealed
//! type here: the return becomes the union of the body's return-expression types (from
//! the scope's final inference), plus `None` when the end of the scope is reachable.
//! A function that never returns reveals `Never`, mirroring pyright's `NoReturn`.
//!
//! Generators reveal `Generator[Y, S, R]` (`AsyncGenerator[Y, S]` when async), pinned
//! against basedpyright 1.39.10 out-of-corpus probes (2026-08-23): Y is the union of
//! yield-expression types (`yield from` contributes the delegate's element type), R the
//! same returns-plus-fallthrough union as plain functions, and S is `Any` while every
//! yield is directly an expression-statement value, flipping to `Unknown` once a yield
//! value is consumed or a `yield from` appears.
//!
//! A `return <call>` whose callee is itself a plain unannotated function (or a bound
//! method of one) contributes that callee's inferred return instead of the `Unknown` ty
//! leaves at unannotated call sites — pyright propagates inferred returns through call
//! chains; this bounded arm (direct returns only, cycle-guarded, depth-capped) mirrors
//! the common shape. Calls nested in other expressions and names bound from calls stay
//! as inferred.
//!
//! Decorators: an identity-typed decorator (`Callable[[F], F]`, a generic `__call__`
//! returning its argument) specializes the function type without changing its signature,
//! so the reveal follows the function through it; a ParamSpec-transparent decorator
//! rewrites it to a `Callable` (identity lost: stock display). A bound method (`C.m` for
//! a classmethod) reveals its bound signature with the inferred return.
//!
//! Async non-generator and overloaded functions keep their normal display: their return
//! shape is wrapped (Coroutine) or already author-declared, and pyright parity for them
//! is out of the oracle's scope.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ty_python_core::scope::ScopeId;
use ty_python_core::{FileScopeId, SemanticIndex, semantic_index};

use crate::Db;
use crate::reachability::evaluate_reachability_constraint;
use crate::types::infer::{ScopeInference, infer_complete_scope_types};
use crate::types::{
    CallableType, FunctionType, KnownClass, ProgramEnvironment, Type, UnionBuilder,
};

/// Shim-facing (batch protocol): the string `report_revealed_type` would print for
/// `ty` — the inferred-return patch plus ty's display, long unions preserved.
pub fn revealed_display<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    env: &ProgramEnvironment<'db>,
) -> String {
    with_inferred_return(db, ty)
        .display(db, env)
        .preserve_long_unions()
        .to_string()
}

/// The revealed form of `ty`: a return-unannotated function - plain, identity-decorated,
/// or bound (`C.m`) - as a callable carrying its inferred return.
pub(crate) fn with_inferred_return<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    let (function, bound) = match ty {
        Type::FunctionLiteral(function) => (function, None),
        Type::BoundMethod(method) => (method.function(db), Some(method)),
        _ => return ty,
    };
    let mut visiting = vec![function];
    let Some(inferred) = inferred_return(db, function, &mut visiting) else {
        return ty;
    };
    let signature = match bound {
        // the receiver-bound signature; a non-overloaded function has exactly one
        Some(method) => {
            let callable = method.into_callable_type(db);
            let [signature] = callable.signatures(db).overloads.as_slice() else {
                return ty;
            };
            signature.clone()
        }
        // decorator-updated signatures respected (identity decorators specialize)
        None => function.last_definition_signature(db).clone(),
    };
    Type::Callable(CallableType::single(db, signature.with_return_type(inferred)))
}

/// Call chains deeper than this keep the call's own (`Unknown`) type.
const MAX_CHAIN_DEPTH: usize = 8;

/// The inferred return of a plain unannotated function (`Generator[...]` /
/// `AsyncGenerator[...]` for generators). `None` where the patch does not apply or
/// some contributing expression has no inferred type — the stock display stays.
fn inferred_return<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
    visiting: &mut Vec<FunctionType<'db>>,
) -> Option<Type<'db>> {
    if function.is_overloaded(db) {
        return None;
    }
    let overload = function.literal(db).last_definition;
    if overload.has_explicit_return_annotation(db) {
        return None;
    }
    let scope = overload.body_scope(db);
    let program_file = scope.program_file(db);
    let index = semantic_index(db, program_file);
    let file_scope = scope.file_scope_id(db);
    let module = parsed_module(db, scope.python_file(db)).load(db);
    let node = scope.node(db).expect_function().node(&module);
    let is_generator = file_scope.is_generator_function(index);
    if node.is_async && !is_generator {
        return None;
    }

    let inference = infer_complete_scope_types(db, scope);
    let env = ProgramEnvironment::from_file(program_file);
    let returns = |visiting: &mut Vec<FunctionType<'db>>| {
        returns_union(
            db, scope, file_scope, index, inference, &env, &node.body, visiting,
        )
    };

    if !is_generator {
        return returns(visiting);
    }
    let mut yields = YieldCollector::default();
    yields.visit_body(&node.body);
    let mut builder = UnionBuilder::new(db, &env);
    for site in &yields.sites {
        // missing inference for a contributing expression: stay honest,
        // keep the stock `Unknown` display
        builder = builder.add(yield_site_type(db, &env, inference, site)?);
    }
    let yield_ty = builder.build();
    let send_ty = if yields.consumed || yields.has_yield_from {
        Type::unknown()
    } else {
        Type::any()
    };
    Some(if node.is_async {
        KnownClass::AsyncGenerator.to_specialized_instance(db, &env, &[yield_ty, send_ty])
    } else {
        let return_ty = returns(visiting)?;
        KnownClass::Generator.to_specialized_instance(db, &env, &[yield_ty, send_ty, return_ty])
    })
}

/// Union of the body's own return-expression types, plus `None` when the end of the
/// scope is reachable. `None` when some return expression has no inferred type.
#[allow(clippy::too_many_arguments)]
fn returns_union<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    file_scope: FileScopeId,
    index: &SemanticIndex,
    inference: &ScopeInference<'db>,
    env: &ProgramEnvironment<'db>,
    body: &[ast::Stmt],
    visiting: &mut Vec<FunctionType<'db>>,
) -> Option<Type<'db>> {
    let mut returns = Vec::new();
    collect_returns(body, &mut returns);
    let mut builder = UnionBuilder::new(db, env);
    for ret in returns {
        match ret.value.as_deref() {
            Some(expr) => {
                let own = inference.try_expression_type(ast::ExprRef::from(expr))?;
                let ty = if own.is_unknown() {
                    chained_call_return(db, inference, expr, visiting).unwrap_or(own)
                } else {
                    own
                };
                builder = builder.add(ty);
            }
            None => builder = builder.add(Type::none(db, env)),
        }
    }
    let use_def = index.use_def_map(file_scope);
    let end_reachable =
        !evaluate_reachability_constraint(db, scope, use_def.end_of_scope_reachability())
            .is_always_false();
    if end_reachable {
        builder = builder.add(Type::none(db, env));
    }
    Some(builder.build())
}

/// The chain arm: for `return f(...)` where `f` is a plain unannotated function (or a
/// bound method of one), that callee's inferred return. `None` keeps the call's own
/// type: a cycle, the depth cap, or a callee the patch does not apply to.
fn chained_call_return<'db>(
    db: &'db dyn Db,
    inference: &ScopeInference<'db>,
    expr: &ast::Expr,
    visiting: &mut Vec<FunctionType<'db>>,
) -> Option<Type<'db>> {
    let ast::Expr::Call(call) = expr else {
        return None;
    };
    let callee = match inference.try_expression_type(ast::ExprRef::from(&*call.func))? {
        Type::FunctionLiteral(function) => function,
        Type::BoundMethod(method) => method.function(db),
        _ => return None,
    };
    if visiting.len() >= MAX_CHAIN_DEPTH || visiting.contains(&callee) {
        return None;
    }
    visiting.push(callee);
    let inferred = inferred_return(db, callee, visiting);
    visiting.pop();
    inferred
}

/// What one yield site contributes to Y: the yielded expression's type (`None` for a
/// bare `yield`), or the delegate's element type for `yield from`.
fn yield_site_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    inference: &ScopeInference<'db>,
    site: &YieldSite<'_>,
) -> Option<Type<'db>> {
    match site {
        YieldSite::Yield(y) => match y.value.as_deref() {
            Some(expr) => inference.try_expression_type(ast::ExprRef::from(expr)),
            None => Some(Type::none(db, env)),
        },
        YieldSite::YieldFrom(yf) => {
            let delegate = inference.try_expression_type(ast::ExprRef::from(&*yf.value))?;
            Some(delegate.iterate(db, env).homogeneous_element_type(db, env))
        }
    }
}

enum YieldSite<'a> {
    Yield(&'a ast::ExprYield),
    YieldFrom(&'a ast::ExprYieldFrom),
}

/// Collects the yields belonging to one function scope (nested function and class
/// bodies, and lambdas, are their own scopes), in source order, tracking whether any
/// yield's value is consumed — i.e. the yield sits anywhere but directly as an
/// expression-statement value.
#[derive(Default)]
struct YieldCollector<'a> {
    sites: Vec<YieldSite<'a>>,
    consumed: bool,
    has_yield_from: bool,
}

impl<'a> YieldCollector<'a> {
    fn visit_body(&mut self, body: &'a [ast::Stmt]) {
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for YieldCollector<'a> {
    fn visit_stmt(&mut self, stmt: &'a ast::Stmt) {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            // statement-position yield: its value is not consumed
            ast::Stmt::Expr(stmt_expr) => {
                if let ast::Expr::Yield(y) = &*stmt_expr.value {
                    self.sites.push(YieldSite::Yield(y));
                    if let Some(value) = y.value.as_deref() {
                        self.visit_expr(value);
                    }
                } else {
                    source_order::walk_stmt(self, stmt);
                }
            }
            _ => source_order::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        match expr {
            ast::Expr::Lambda(_) => {}
            ast::Expr::Yield(y) => {
                self.sites.push(YieldSite::Yield(y));
                self.consumed = true;
                if let Some(value) = y.value.as_deref() {
                    self.visit_expr(value);
                }
            }
            ast::Expr::YieldFrom(yf) => {
                self.sites.push(YieldSite::YieldFrom(yf));
                self.has_yield_from = true;
                self.visit_expr(&yf.value);
            }
            _ => source_order::walk_expr(self, expr),
        }
    }
}

/// Every `return` statement belonging to this body's own scope (nested function
/// and class bodies are their own scopes; expressions cannot contain `return`).
fn collect_returns<'a>(body: &'a [ast::Stmt], out: &mut Vec<&'a ast::StmtReturn>) {
    for stmt in body {
        match stmt {
            ast::Stmt::Return(ret) => out.push(ret),
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            ast::Stmt::If(s) => {
                collect_returns(&s.body, out);
                for clause in &s.elif_else_clauses {
                    collect_returns(&clause.body, out);
                }
            }
            ast::Stmt::While(s) => {
                collect_returns(&s.body, out);
                collect_returns(&s.orelse, out);
            }
            ast::Stmt::For(s) => {
                collect_returns(&s.body, out);
                collect_returns(&s.orelse, out);
            }
            ast::Stmt::With(s) => collect_returns(&s.body, out),
            ast::Stmt::Try(s) => {
                collect_returns(&s.body, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_returns(&h.body, out);
                }
                collect_returns(&s.orelse, out);
                collect_returns(&s.finalbody, out);
            }
            ast::Stmt::Match(s) => {
                for case in &s.cases {
                    collect_returns(&case.body, out);
                }
            }
            _ => {}
        }
    }
}
