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
//! Async functions, generators, overloaded and signature-updated functions keep their
//! normal display: their return shape is wrapped (Coroutine/Generator) or already
//! author-declared, and pyright parity for them is out of the oracle's scope.

use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ty_python_core::semantic_index;

use crate::Db;
use crate::reachability::evaluate_reachability_constraint;
use crate::types::infer::infer_complete_scope_types;
use crate::types::{CallableType, ProgramEnvironment, Type, UnionBuilder};

/// The revealed form of `ty`: unannotated plain functions get their inferred return.
pub(crate) fn with_inferred_return<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    let Type::FunctionLiteral(function) = ty else {
        return ty;
    };
    if !function.is_plain_definition(db) {
        return ty;
    }
    let overload = function.literal(db).last_definition;
    if overload.has_explicit_return_annotation(db) {
        return ty;
    }
    let scope = overload.body_scope(db);
    let program_file = scope.program_file(db);
    let index = semantic_index(db, program_file);
    let file_scope = scope.file_scope_id(db);
    if file_scope.is_generator_function(index) {
        return ty;
    }
    let module = parsed_module(db, scope.python_file(db)).load(db);
    let node = scope.node(db).expect_function().node(&module);
    if node.is_async {
        return ty;
    }

    let inference = infer_complete_scope_types(db, scope);
    let env = ProgramEnvironment::from_file(program_file);
    let mut returns = Vec::new();
    collect_returns(&node.body, &mut returns);
    let mut builder = UnionBuilder::new(db, &env);
    for ret in returns {
        match ret.value.as_deref() {
            Some(expr) => {
                let Some(expr_ty) = inference.try_expression_type(ast::ExprRef::from(expr))
                else {
                    // missing inference for a return expression: stay honest,
                    // keep the stock `Unknown` display
                    return ty;
                };
                builder = builder.add(expr_ty);
            }
            None => builder = builder.add(Type::none(db, &env)),
        }
    }
    let use_def = index.use_def_map(file_scope);
    let end_reachable =
        !evaluate_reachability_constraint(db, scope, use_def.end_of_scope_reachability())
            .is_always_false();
    if end_reachable {
        builder = builder.add(Type::none(db, &env));
    }
    let inferred = builder.build();

    Type::Callable(CallableType::single(
        db,
        overload.signature(db).with_return_type(inferred),
    ))
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
