//! Ports of pyright's `reportUnnecessaryIsInstance`, `reportUnnecessaryComparison` and
//! `reportUnnecessaryContains` checks onto ty's type API.
//!
//! pyright is © Microsoft Corporation, MIT-licensed; this module mirrors its published
//! algorithms onto ty's type API.
//!
//! Semantics source of truth: pyright `packages/pyright-internal/src/analyzer/checker.ts`
//! (`_validateIsInstanceCall`, `_validateComparisonTypes`, `_validateContainmentTypes`,
//! `_reportUnnecessaryConditionExpression`) and `typeEvaluator.ts` (`typesOverlap`,
//! `isTypeComparable`), as of pyright 1.1.412 (basedpyright 1.39.10). The `==`/`!=` arm
//! deliberately replicates pyright's `__eq__`-ignoring unsoundness for disjoint builtin
//! types: bit-identity with pyright is the acceptance gate here, soundness demotion is the
//! consumer's job.
//!
//! Like pyright's checker, this runs as a post-inference pass over the AST
//! (`check_unnecessary_diagnostics`, called from `check_types`), reading final expression
//! types from the per-scope inference results. Emitting from inside inference is unreliable
//! for these rules: speculative/bidirectional inference suppresses diagnostics and caches
//! results, silently losing reports for e.g. ternary conditions in assignments.
//!
//! All three checks are suppressed within `assert` statements, matching pyright's
//! `isWithinAssertExpression` gate.

use ruff_python_ast as ast;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::file_to_module;
use ty_python_core::{ProgramFile, semantic_index};

use crate::declare_lint;
use crate::lint::{Level, LintStatus};
use crate::types::context::InferContext;
use crate::types::infer::infer_complete_scope_types;
use crate::types::narrow::{ClassInfoConstraintFunction, filter_generic_narrowing_constraint};
use crate::types::set_theoretic::IntersectionType;
use crate::types::{
    ClassLiteral, ClassType, KnownClass, LiteralValueTypeKind, MemberLookupPolicy,
    ProgramEnvironment, Type, TypeCheckDiagnostics, TypeVarBoundOrConstraints, UnionType,
};
use crate::Db;

declare_lint! {
    #[doc = include_str!("../../resources/lint_docs/unnecessary-isinstance.md")]
    pub(crate) static UNNECESSARY_ISINSTANCE = {
        summary: "detects `isinstance`/`issubclass` calls whose result is statically known",
        status: LintStatus::stable("0.16.4"),
        default_level: Level::Ignore,
    }
}

declare_lint! {
    #[doc = include_str!("../../resources/lint_docs/unnecessary-comparison.md")]
    pub(crate) static UNNECESSARY_COMPARISON = {
        summary: "detects comparisons whose operand types have no overlap",
        status: LintStatus::stable("0.16.4"),
        default_level: Level::Ignore,
    }
}

declare_lint! {
    #[doc = include_str!("../../resources/lint_docs/unnecessary-contains.md")]
    pub(crate) static UNNECESSARY_CONTAINS = {
        summary: "detects `in`/`not in` tests that can never succeed",
        status: LintStatus::stable("0.16.4"),
        default_level: Level::Ignore,
    }
}

/// Run the pyright-ported `unnecessary-*` checks over a whole file.
///
/// Walks the module AST in source order (like pyright's checker pass) and reads final
/// expression types from the per-scope inference results.
pub(crate) fn check_unnecessary_diagnostics(
    db: &dyn Db,
    program_file: ProgramFile<'_>,
    module: &ruff_db::parsed::ParsedModuleRef,
) -> TypeCheckDiagnostics {
    // All three rules are off by default; skip the walk entirely unless one is enabled.
    let selection = db.rule_selection(program_file.file(db));
    if [
        &UNNECESSARY_ISINSTANCE,
        &UNNECESSARY_COMPARISON,
        &UNNECESSARY_CONTAINS,
    ]
    .iter()
    .all(|lint| selection.get(crate::lint::LintId::of(lint)).is_none())
    {
        return TypeCheckDiagnostics::default();
    }

    let env = ProgramEnvironment::from_file(program_file);
    let mut checker = UnnecessaryChecker {
        db,
        env: &env,
        program_file,
        module,
        assert_depth: 0,
        diagnostics: TypeCheckDiagnostics::default(),
    };
    checker.visit_body(&module.syntax().body);
    checker.diagnostics
}

struct UnnecessaryChecker<'db, 'ast> {
    db: &'db dyn Db,
    env: &'ast ProgramEnvironment<'db>,
    program_file: ProgramFile<'db>,
    module: &'ast ruff_db::parsed::ParsedModuleRef,
    assert_depth: u32,
    diagnostics: TypeCheckDiagnostics,
}

impl<'db> UnnecessaryChecker<'db, '_> {
    /// The final inferred type of an expression, from its scope's inference result.
    fn expression_type(&self, expr: &ast::Expr) -> Option<Type<'db>> {
        let index = semantic_index(self.db, self.program_file);
        let expr_ref = ast::ExprRef::from(expr);
        let file_scope = index.try_expression_scope_id(&expr_ref)?;
        let scope = file_scope.to_scope_id(self.db, self.program_file);
        infer_complete_scope_types(self.db, scope).try_expression_type(expr_ref)
    }

    /// The type of a leaf of a condition expression. Condition roots are standalone
    /// expressions whose sub-expression types in the scope-level map may reflect
    /// branch-narrowed re-inference (an always-truthy `f` narrows to `Never` on the false
    /// branch), so prefer the standalone expression region's own inference.
    fn condition_leaf_type(&self, root: &ast::Expr, leaf: &ast::Expr) -> Option<Type<'db>> {
        let index = semantic_index(self.db, self.program_file);
        if let Some(expression) = index.try_expression(root) {
            let inference = crate::types::infer::infer_expression_types(
                self.db,
                expression,
                crate::types::TypeContext::default(),
            );
            if let Some(ty) = inference.try_expression_type(ast::ExprRef::from(leaf)) {
                return Some(ty);
            }
        }
        self.expression_type(leaf)
    }

    /// Run `check` with a diagnostic context scoped to `node`'s enclosing scope, collecting
    /// whatever it reports.
    fn with_context(
        &mut self,
        node: &ast::Expr,
        check: impl FnOnce(&InferContext<'db, '_>, &Self),
    ) {
        let index = semantic_index(self.db, self.program_file);
        let Some(file_scope) = index.try_expression_scope_id(&ast::ExprRef::from(node)) else {
            return;
        };
        let scope = file_scope.to_scope_id(self.db, self.program_file);
        let context = InferContext::new(
            self.db,
            self.env,
            scope,
            self.program_file.file(self.db),
            self.program_file,
            self.module,
        );
        check(&context, self);
        self.diagnostics.extend(&context.finish());
    }
}

impl<'ast> SourceOrderVisitor<'ast> for UnnecessaryChecker<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::Assert(_) => {
                self.assert_depth += 1;
                source_order::walk_stmt(self, stmt);
                self.assert_depth -= 1;
            }
            ast::Stmt::If(if_stmt) => {
                self.check_condition(&if_stmt.test);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.check_condition(test);
                    }
                }
                source_order::walk_stmt(self, stmt);
            }
            ast::Stmt::While(while_stmt) => {
                self.check_condition(&while_stmt.test);
                source_order::walk_stmt(self, stmt);
            }
            _ => source_order::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::If(if_expr) => {
                self.check_condition(&if_expr.test);
            }
            ast::Expr::Compare(compare) => {
                self.check_compare(compare);
            }
            ast::Expr::Call(call) => {
                self.check_isinstance_call(call);
            }
            _ => {}
        }
        source_order::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, comprehension: &'ast ast::Comprehension) {
        for test in &comprehension.ifs {
            self.check_condition(test);
        }
        source_order::walk_comprehension(self, comprehension);
    }
}

impl<'db> UnnecessaryChecker<'db, '_> {
    /// Port of checker.ts `_reportUnnecessaryConditionExpression`: an `if` / `while` /
    /// ternary / comprehension-`if` condition that references a function or coroutine
    /// object is always truthy. Reported under `unnecessary-comparison`, like pyright.
    /// Recurses through `and` / `or` / `not`. Not gated on `assert` (pyright's isn't:
    /// `assert` statements simply never call it, but a ternary inside an assert is still
    /// checked via the ternary visit).
    fn check_condition(&mut self, test: &ast::Expr) {
        self.check_condition_leaf(test, test);
    }

    fn check_condition_leaf(&mut self, root: &ast::Expr, test: &ast::Expr) {
        match test {
            ast::Expr::BoolOp(bool_op) => {
                for value in &bool_op.values {
                    self.check_condition_leaf(root, value);
                }
                return;
            }
            ast::Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not => {
                self.check_condition_leaf(root, &unary.operand);
                return;
            }
            _ => {}
        }

        let Some(ty) = self.condition_leaf_type(root, test) else {
            return;
        };
        self.with_context(test, |context, checker| {
            let db = checker.db;
            let env = context.program_environment();
            let Some(builder) = context.report_lint(&UNNECESSARY_COMPARISON, test) else {
                return;
            };
            if ty.is_never() {
                return;
            }

            let subtypes = expand_subtypes(db, env, ty);
            let is_function = subtypes.iter().all(|sub| is_function_or_overloaded(*sub));
            let is_coroutine = subtypes.iter().all(|sub| {
                sub.as_nominal_instance().is_some_and(|instance| {
                    instance.known_class(db) == Some(KnownClass::CoroutineType)
                })
            });

            if is_function {
                builder.into_diagnostic(
                    "Conditional expression references function which always evaluates to True",
                );
            } else if is_coroutine {
                builder.into_diagnostic(
                    "Conditional expression references coroutine which always evaluates to True",
                );
            }
        });
    }

    /// Dispatch one (possibly chained) comparison expression to the `==`/`!=`/`is`/`is not`
    /// and `in`/`not in` checks, one operand pair at a time — which matches pyright's
    /// per-binary-node visitation of chained comparisons.
    fn check_compare(&mut self, compare: &ast::ExprCompare) {
        if self.assert_depth > 0 {
            return;
        }
        // pyright's parse nodes fold parentheses into the operand, so a diagnostic on
        // `(x := y) is not None` starts at the `(`. Mirror via `parenthesized_range`
        // (an empty CommentRanges only misses parens containing comments, which then
        // fall back to the bare operand range).
        let source = ruff_db::source::source_text(self.db, self.program_file.file(self.db));
        let comment_ranges = ruff_python_trivia::CommentRanges::default();
        let operand_range = |expr: &ast::Expr| -> TextRange {
            ruff_python_ast::parenthesize::parenthesized_range(
                expr.into(),
                compare.into(),
                &comment_ranges,
                source.as_str(),
            )
            .unwrap_or_else(|| expr.range())
        };
        let operands: Vec<&ast::Expr> = std::iter::once(&*compare.left)
            .chain(compare.comparators.iter())
            .collect();
        for (index, op) in compare.ops.iter().enumerate() {
            let left = operands[index];
            let right = operands[index + 1];
            let range =
                TextRange::new(operand_range(left).start(), operand_range(right).end());
            match op {
                ast::CmpOp::Eq | ast::CmpOp::NotEq | ast::CmpOp::Is | ast::CmpOp::IsNot => {
                    self.check_comparison_pair(left, *op, right, range);
                }
                ast::CmpOp::In | ast::CmpOp::NotIn => {
                    self.check_contains_pair(*op, left, right, range);
                }
                _ => {}
            }
        }
    }

    /// Port of checker.ts `_validateComparisonTypes`.
    fn check_comparison_pair(
        &mut self,
        left: &ast::Expr,
        op: ast::CmpOp,
        right: &ast::Expr,
        range: TextRange,
    ) {
        let (Some(left_ty), Some(right_ty)) =
            (self.expression_type(left), self.expression_type(right))
        else {
            return;
        };

        self.with_context(left, |context, checker| {
            let db = checker.db;
            let env = context.program_environment();
            let Some(builder) = context.report_lint(&UNNECESSARY_COMPARISON, range) else {
                return;
            };

            // pyright skips comparisons involving modules entirely.
            if matches!(left_ty, Type::ModuleLiteral(..))
                || matches!(right_ty, Type::ModuleLiteral(..))
            {
                return;
            }

            let assume_is_operator = matches!(op, ast::CmpOp::Is | ast::CmpOp::IsNot);

            // pyright replaces int/str/bytes-based enum literals with their literal values
            // before comparing (`replaceEnumTypeWithLiteralValue`).
            let left_ty = replace_enum_with_literal_value(db, env, left_ty);
            let right_ty = replace_enum_with_literal_value(db, env, right_ty);

            let report = if is_literal_type_or_union(db, left_ty)
                && is_literal_type_or_union(db, right_ty)
            {
                // Statically-evaluable platform / version conditions are exempt
                // (`evaluateStaticBoolExpression`).
                if is_static_platform_or_version_condition(left, right) {
                    return;
                }
                let possibly_true = union_elements(db, left_ty)
                    .iter()
                    .any(|left_sub| left_sub.is_assignable_to(db, env, right_ty))
                    || union_elements(db, right_ty)
                        .iter()
                        .any(|right_sub| right_sub.is_assignable_to(db, env, left_ty));
                !possibly_true
            } else {
                !types_overlap(db, env, left_ty, right_ty, assume_is_operator)
            };

            if report {
                let always_false = matches!(op, ast::CmpOp::Eq | ast::CmpOp::Is);
                let result = if always_false { "False" } else { "True" };
                let left_display = left_ty.display(db, env);
                let right_display = right_ty.display(db, env);
                builder.into_diagnostic(format!(
                    "Condition will always evaluate to {result} since the types \"{left_display}\" and \"{right_display}\" have no overlap"
                ));
            }
        });
    }

    /// Port of checker.ts `_validateContainmentTypes` plus typeGuards.ts
    /// `getElementTypeForContainerNarrowing` / `narrowTypeForContainerElementType`.
    fn check_contains_pair(
        &mut self,
        op: ast::CmpOp,
        left: &ast::Expr,
        right: &ast::Expr,
        range: TextRange,
    ) {
        let (Some(left_ty), Some(container_ty)) =
            (self.expression_type(left), self.expression_type(right))
        else {
            return;
        };

        self.with_context(left, |context, checker| {
            let db = checker.db;
            let env = context.program_environment();
            let Some(builder) = context.report_lint(&UNNECESSARY_CONTAINS, range) else {
                return;
            };

            if left_ty.is_never() || container_ty.is_never() {
                return;
            }

            let Some(element_ty) = container_element_type(db, env, container_ty) else {
                return;
            };
            let element_subtypes: Vec<Type<'db>> = union_elements(db, element_ty);

            // A reference subtype "survives" the narrowing if any element subtype is
            // comparable to it or one of the literal/class-object narrowing arms applies
            // (`narrowTypeForContainerElementType`).
            let survives = |reference: Type<'db>| -> bool {
                element_subtypes.iter().any(|element| {
                    if element.is_dynamic() {
                        return true;
                    }
                    if is_type_comparable(db, env, *element, reference, false) {
                        return true;
                    }
                    let element_is_literal_like = matches!(element, Type::LiteralValue(..))
                        || element.is_none(db)
                        || matches!(element, Type::ClassLiteral(..) | Type::GenericAlias(..));
                    if element_is_literal_like && element.is_assignable_to(db, env, reference) {
                        return true;
                    }
                    let reference_is_literal_like =
                        matches!(reference, Type::LiteralValue(..)) || reference.is_none(db);
                    reference_is_literal_like && reference.is_assignable_to(db, env, *element)
                })
            };

            let narrowed_to_never = expand_subtypes(db, env, left_ty)
                .into_iter()
                .all(|reference| !survives(reference));

            if narrowed_to_never {
                let result = if matches!(op, ast::CmpOp::In) {
                    "False"
                } else {
                    "True"
                };
                let left_display = left_ty.display(db, env);
                let element_display = element_ty.display(db, env);
                builder.into_diagnostic(format!(
                    "Expression will always evaluate to {result} since the types \"{left_display}\" and \"{element_display}\" have no overlap"
                ));
            }
        });
    }

    /// Port of checker.ts `_validateIsInstanceCall`: narrow the first argument by the
    /// classinfo argument in both polarities; report when the negative narrowing is `Never`
    /// (the call is always true) or the positive narrowing is `Never` (it can never be
    /// true). Like pyright, this gates on the bare names `isinstance` / `issubclass` with
    /// exactly two positional arguments — regardless of what the name resolves to.
    fn check_isinstance_call(&mut self, call: &ast::ExprCall) {
        if self.assert_depth > 0 {
            return;
        }
        let ast::Expr::Name(callee) = &*call.func else {
            return;
        };
        let is_instance_check = match callee.id.as_str() {
            "isinstance" => true,
            "issubclass" => false,
            _ => return,
        };
        if call.arguments.args.len() != 2 || !call.arguments.keywords.is_empty() {
            return;
        }
        let (Some(test_ty), Some(classinfo_ty)) = (
            self.expression_type(&call.arguments.args[0]),
            self.expression_type(&call.arguments.args[1]),
        ) else {
            return;
        };

        self.with_context(&call.arguments.args[0], |context, checker| {
            let db = checker.db;
            let env = context.program_environment();
            let Some(builder) = context.report_lint(&UNNECESSARY_ISINSTANCE, call) else {
                return;
            };

            let constraint_fn = if is_instance_check {
                ClassInfoConstraintFunction::IsInstance
            } else {
                ClassInfoConstraintFunction::IsSubclass
            };

            // Mirrors the condition-narrowing application in `narrow.rs`: this is what an
            // `if isinstance(...)` test would narrow the subject to, in each polarity. A
            // classinfo argument that yields no constraint corresponds to pyright's
            // `getIsInstanceClassTypes` returning undefined: the check is skipped.
            let use_generic_filtering = !db
                .analysis_settings(context.file())
                .strict_generic_narrowing;
            let Some(positive_constraint) =
                constraint_fn.generate_constraint(db, env, classinfo_ty, true, use_generic_filtering)
            else {
                return;
            };
            let Some(negative_constraint) =
                constraint_fn.generate_constraint(db, env, classinfo_ty, false, false)
            else {
                return;
            };

            let narrowed_positive = if use_generic_filtering {
                filter_generic_narrowing_constraint(db, env, test_ty, positive_constraint)
            } else {
                IntersectionType::from_two_elements(db, env, test_ty, positive_constraint)
            };
            let narrowed_negative = IntersectionType::from_two_elements(
                db,
                env,
                test_ty,
                negative_constraint.negate(db, env),
            );

            let always_true = narrowed_negative.is_never();
            let never_true = narrowed_positive.is_never();
            if !always_true && !never_true {
                return;
            }

            // pyright prints the classinfo filters converted to their instance form for
            // both isinstance and issubclass messages (unknown specialization, so a bare
            // `list` displays as `list[Unknown]` like pyright, not `Top[list[...]]`).
            let class_display_ty = ClassInfoConstraintFunction::IsInstance
                .generate_constraint(db, env, classinfo_ty, true, true)
                .unwrap_or(positive_constraint);

            let test_display = test_ty.display(db, env);
            let class_display = class_display_ty.display(db, env);
            let message = match (is_instance_check, always_true) {
                (true, true) => format!(
                    "Unnecessary isinstance call; \"{test_display}\" is always an instance of \"{class_display}\""
                ),
                (true, false) => format!(
                    "Unnecessary isinstance call; \"{test_display}\" is never an instance of \"{class_display}\""
                ),
                (false, true) => format!(
                    "Unnecessary issubclass call; \"{test_display}\" is always a subclass of \"{class_display}\""
                ),
                (false, false) => format!(
                    "Unnecessary issubclass call; \"{test_display}\" is never a subclass of \"{class_display}\""
                ),
            };
            builder.into_diagnostic(message);
        });
    }
}

/// Port of typeEvaluator.ts `typesOverlap`: the two types overlap if any pair of their
/// expanded subtypes is comparable.
fn types_overlap<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left: Type<'db>,
    right: Type<'db>,
    assume_is_operator: bool,
) -> bool {
    if left.is_never() || right.is_never() {
        return true;
    }
    let left_subtypes = expand_subtypes(db, env, left);
    let right_subtypes = expand_subtypes(db, env, right);
    left_subtypes.iter().any(|left_sub| {
        right_subtypes
            .iter()
            .any(|right_sub| is_type_comparable(db, env, *left_sub, *right_sub, assume_is_operator))
    })
}

/// pyright's `mapSubtypesExpandTypeVars` + `makeTopLevelTypeVarsConcrete`: union members,
/// with type variables replaced by their bounds and expanded into their constraints.
fn expand_subtypes<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Vec<Type<'db>> {
    fn expand<'db>(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
        into: &mut Vec<Type<'db>>,
        depth: usize,
    ) {
        if depth > 8 {
            into.push(ty);
            return;
        }
        match ty {
            Type::Union(union) => {
                for element in union.elements(db) {
                    expand(db, env, *element, into, depth + 1);
                }
            }
            Type::TypeAlias(alias) => expand(db, env, alias.value_type(db), into, depth + 1),
            Type::TypeVar(bound_typevar) => {
                match bound_typevar.typevar(db).bound_or_constraints(db, env) {
                    None => into.push(Type::object()),
                    Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
                        expand(db, env, bound, into, depth + 1);
                    }
                    Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                        for constraint in constraints.elements(db) {
                            expand(db, env, *constraint, into, depth + 1);
                        }
                    }
                }
            }
            _ => into.push(ty),
        }
    }
    let mut result = Vec::new();
    expand(db, env, ty, &mut result, 0);
    result
}

fn union_elements<'db>(db: &'db dyn Db, ty: Type<'db>) -> Vec<Type<'db>> {
    match ty {
        Type::Union(union) => union.elements(db).to_vec(),
        _ => vec![ty],
    }
}

/// A class-object-like type for the purposes of `isTypeComparable`'s despecialized
/// subclass check: pyright's `isInstantiableClass` plus instances of `type`.
/// `Some(None)` means "a class object of statically-unknown class", which pyright treats
/// as comparable to any other class object.
fn as_class_object_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Option<ClassLiteral<'db>>> {
    match ty {
        Type::ClassLiteral(class) => Some(Some(class)),
        Type::GenericAlias(alias) => Some(Some(ClassType::Generic(alias).class_literal(db))),
        Type::SubclassOf(subclass_of) => match subclass_of.subclass_of().into_class(db, env) {
            Some(class) => Some(Some(class.class_literal(db))),
            None => Some(None),
        },
        Type::NominalInstance(instance) if instance.known_class(db) == Some(KnownClass::Type) => {
            Some(None)
        }
        _ => None,
    }
}

/// pyright models literal types as class instances carrying a literal value; ty makes them a
/// separate `Type` variant. Normalize to the nominal instance for `isTypeComparable`'s
/// class-instance arms.
fn as_instance_for_comparison<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<crate::types::instance::NominalInstanceType<'db>> {
    if let Some(instance) = ty.as_nominal_instance() {
        return Some(instance);
    }
    match ty {
        Type::LiteralValue(..) => ty
            .literal_fallback_instance(db, env)
            .and_then(|fallback| fallback.as_nominal_instance()),
        _ => None,
    }
}

/// Does any class in the MRO other than `object` define this member?
fn defines_member_skipping_object<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    name: &'static str,
) -> bool {
    !ty.member_lookup_with_policy(db, env, name, MemberLookupPolicy::MRO_NO_OBJECT_FALLBACK)
        .place
        .is_undefined()
}

/// pyright's `ClassType.isBuiltIn` without a name filter: the class was declared in a
/// typeshed stdlib stub (which includes `builtins`, `typing`, `collections`, ...).
fn is_typeshed_stdlib_class(db: &dyn Db, class: ClassLiteral) -> bool {
    file_to_module(db, class.program_file(db).resolver_file(db)).is_some_and(|module| {
        module
            .search_path(db)
            .is_some_and(|path| path.is_standard_library())
    })
}

/// Port of typeEvaluator.ts `isTypeComparable` (with `checkEq` always true, matching both
/// callers we port). This deliberately replicates pyright's unsoundness: disjoint typeshed
/// classes are "not comparable" before the left class's `__eq__` override is ever consulted.
fn is_type_comparable<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left: Type<'db>,
    right: Type<'db>,
    assume_is_operator: bool,
) -> bool {
    is_type_comparable_impl(db, env, left, right, assume_is_operator, false)
}

/// `strict_numeric` is set when a side came out of a narrowing projection
/// (intersection / enum complement): pyright's isinstance-narrowed `float` is the strict
/// class, which it does NOT treat as overlapping an int *literal*, while a `float` from an
/// annotation is the promoted `int | float` form which does.
fn is_type_comparable_impl<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left: Type<'db>,
    right: Type<'db>,
    assume_is_operator: bool,
    strict_numeric: bool,
) -> bool {
    // pyright has no intersection types: where ty narrows to `T & ~X` (or keeps
    // `Unknown & ~X`), pyright sees the un-negated base. Project an intersection to its
    // concrete positive elements — pyright's positive `isinstance` narrowing *replaces*
    // `Any` with the class, so a dynamic positive only wins when nothing concrete is
    // left (`Unknown & ~X` behaves as `Unknown`; `Unknown & float` behaves as `float`).
    fn project_intersection<'db>(
        db: &'db dyn Db,
        intersection: crate::types::IntersectionType<'db>,
    ) -> Vec<Type<'db>> {
        let concrete: Vec<Type<'db>> = intersection
            .positive(db)
            .iter()
            .copied()
            .filter(|p| !p.is_dynamic())
            .collect();
        concrete
    }
    // ty narrows enums to `EnumComplement` (the remaining members); pyright sees a plain
    // literal union. Project through the equivalent intersection.
    if let Type::EnumComplement(complement) = left {
        return is_type_comparable_impl(
            db,
            env,
            complement.to_intersection(db, env),
            right,
            assume_is_operator,
            true,
        );
    }
    if let Type::EnumComplement(complement) = right {
        return is_type_comparable_impl(
            db,
            env,
            left,
            complement.to_intersection(db, env),
            assume_is_operator,
            true,
        );
    }

    if let Type::Intersection(intersection) = left {
        let concrete = project_intersection(db, intersection);
        return concrete.is_empty()
            || concrete.iter().any(|p| {
                is_type_comparable_impl(db, env, *p, right, assume_is_operator, true)
            });
    }
    if let Type::Intersection(intersection) = right {
        let concrete = project_intersection(db, intersection);
        return concrete.is_empty()
            || concrete.iter().any(|p| {
                is_type_comparable_impl(db, env, left, *p, assume_is_operator, true)
            });
    }

    if left.is_dynamic() || right.is_dynamic() {
        return true;
    }

    if left.is_never() || right.is_never() {
        return false;
    }

    if matches!(left, Type::ModuleLiteral(..)) || matches!(right, Type::ModuleLiteral(..)) {
        return match (left, right) {
            (Type::ModuleLiteral(a), Type::ModuleLiteral(b)) => a == b,
            _ => false,
        };
    }

    if is_function_or_overloaded(left) || is_function_or_overloaded(right) {
        return true;
    }

    // Class objects: comparable when one despecialized class is assignable to the other, or
    // when the left class's metaclass defines `__eq__`.
    if let Some(left_class) = as_class_object_class(db, env, left) {
        if let Some(right_class) = as_class_object_class(db, env, right) {
            match (left_class, right_class) {
                (None, _) | (_, None) => return true,
                (Some(left_class), Some(right_class)) => {
                    if left_class
                        .unknown_specialization(db)
                        .is_subtype_of_class_literal(db, right_class)
                        || right_class
                            .unknown_specialization(db)
                            .is_subtype_of_class_literal(db, left_class)
                    {
                        return true;
                    }
                }
            }
        }
        if let Some(left_class) = left_class {
            if defines_member_skipping_object(
                db,
                env,
                left_class.metaclass_instance_type(db, env),
                "__eq__",
            ) {
                return true;
            }
        }
        return false;
    }

    if let Some(left_instance) = as_instance_for_comparison(db, env, left) {
        let left_class = left_instance.class(db, env);
        let right_instance = as_instance_for_comparison(db, env, right);
        let right_class_object = as_class_object_class(db, env, right);
        if right_instance.is_some() || right_class_object.is_some() {
            // Despecialized assignability in either direction. pyright's `specialize`
            // strips type arguments but keeps literal values, so literals stay literal
            // here (`Literal[2]` is not assignable to `bool` in either direction even
            // though `int` would be).
            let left_generic = if matches!(left, Type::LiteralValue(..)) {
                left
            } else {
                Type::instance(
                    db,
                    env,
                    left_class.class_literal(db).unknown_specialization(db),
                )
            };
            let right_generic = if matches!(right, Type::LiteralValue(..)) {
                Some(right)
            } else {
                match (right_instance, right_class_object) {
                    (Some(instance), _) => Some(Type::instance(
                        db,
                        env,
                        instance
                            .class(db, env)
                            .class_literal(db)
                            .unknown_specialization(db),
                    )),
                    (None, Some(Some(class))) => Some(Type::ClassLiteral(class)),
                    (None, Some(None)) => Some(Type::unknown()),
                    (None, None) => None,
                }
            };
            if let Some(right_generic) = right_generic {
                if left_generic.is_assignable_to(db, env, right_generic)
                    || right_generic.is_assignable_to(db, env, left_generic)
                {
                    return true;
                }
            }

            // pyright's `assignType` bakes in the numeric promotions (int -> float ->
            // complex, with `bool` counting as `int`); ty's assignability does not, so
            // mirror them here: two numeric classes of different promotion rank are
            // comparable (`2.0 == 2` is True at runtime) — but, matching observed
            // pyright behavior, promotion never applies to a numeric *literal* operand
            // (strict `float == Literal[0]` is reported, `float == int` is not; a
            // declared `float` parameter is ty's `int | float*` union, whose `int`
            // member covers the literal comparisons without any promotion).
            let literal_involved = matches!(left, Type::LiteralValue(..))
                || matches!(right, Type::LiteralValue(..));
            if let Some(right_instance) = right_instance
                && !(strict_numeric && literal_involved)
            {
                let rank = |class: ClassType<'db>| -> Option<u8> {
                    let mut found: Option<u8> = None;
                    for base in class.class_literal(db).iter_mro(db) {
                        let known = base
                            .into_class()
                            .and_then(|base| base.class_literal(db).known(db));
                        let base_rank = match known {
                            Some(KnownClass::Int) => Some(0),
                            Some(KnownClass::Float) => Some(1),
                            Some(KnownClass::Complex) => Some(2),
                            _ => None,
                        };
                        if let Some(base_rank) = base_rank {
                            found = Some(found.map_or(base_rank, |f| f.min(base_rank)));
                        }
                    }
                    found
                };
                if let (Some(left_rank), Some(right_rank)) =
                    (rank(left_class), rank(right_instance.class(db, env)))
                {
                    if left_rank != right_rank {
                        return true;
                    }
                }
            }

            // `x is None` / `x is not None`: None is only comparable to types it is
            // assignable to (protocols, `object`).
            if assume_is_operator && right.is_none(db) {
                return right.is_assignable_to(db, env, left);
            }

            // Disjoint typeshed-stdlib classes are assumed never comparable, with a
            // carve-out for `bool` vs the int literals `0` and `1`.
            if let Some(right_instance) = right_instance {
                let right_class = right_instance.class(db, env);
                if is_typeshed_stdlib_class(db, left_class.class_literal(db))
                    && is_typeshed_stdlib_class(db, right_class.class_literal(db))
                {
                    let bool_int = if left_class.class_literal(db).is_known(db, KnownClass::Bool)
                        && right_class.class_literal(db).is_known(db, KnownClass::Int)
                    {
                        Some((left, right))
                    } else if right_class.class_literal(db).is_known(db, KnownClass::Bool)
                        && left_class.class_literal(db).is_known(db, KnownClass::Int)
                    {
                        Some((right, left))
                    } else {
                        None
                    };
                    if let Some((bool_side, int_side)) = bool_int {
                        let Some(int_value) = as_int_literal_value(int_side) else {
                            return true;
                        };
                        if int_value != 0 && int_value != 1 {
                            return false;
                        }
                        let Some(bool_value) = as_bool_literal_value(bool_side) else {
                            return true;
                        };
                        return bool_value == (int_value == 1);
                    }
                    return false;
                }
            }
        }

        // Does the left class define a custom `__eq__` (not `object`'s)? A synthesized
        // dataclass `__eq__` does not count.
        if defines_member_skipping_object(db, env, left, "__eq__") {
            let left_class_literal = left_class.class_literal(db);
            let is_dataclass = left_class_literal
                .as_static()
                .is_some_and(|class| class.dataclass_params(db).is_some());
            if is_dataclass && !defines_eq_in_body(db, left_class_literal) {
                return false;
            }
            return true;
        }

        return false;
    }

    true
}

fn is_function_or_overloaded(ty: Type) -> bool {
    matches!(
        ty,
        Type::FunctionLiteral(..)
            | Type::BoundMethod(..)
            | Type::KnownBoundMethod(..)
            | Type::WrapperDescriptor(..)
            | Type::Callable(..)
            | Type::DataclassDecorator(..)
            | Type::DataclassTransformer(..)
    )
}

fn as_int_literal_value(ty: Type) -> Option<i64> {
    match ty {
        Type::LiteralValue(value) => match value.kind() {
            LiteralValueTypeKind::Int(int) => Some(int.as_i64()),
            LiteralValueTypeKind::Bool(b) => Some(i64::from(b)),
            _ => None,
        },
        _ => None,
    }
}

fn as_bool_literal_value(ty: Type) -> Option<bool> {
    match ty {
        Type::LiteralValue(value) => match value.kind() {
            LiteralValueTypeKind::Bool(b) => Some(b),
            _ => None,
        },
        _ => None,
    }
}

/// Does the class body itself bind `__eq__`? (Used to distinguish an explicit `__eq__` on a
/// dataclass from the synthesized one.)
fn defines_eq_in_body(db: &dyn Db, class: ClassLiteral) -> bool {
    let ClassLiteral::Static(class) = class else {
        return false;
    };
    ty_python_core::place_table(db, class.body_scope(db))
        .symbol_id(&ast::name::Name::new_static("__eq__"))
        .is_some()
}

/// pyright's `isLiteralTypeOrUnion`: every subtype carries a concrete literal value
/// (`LiteralString` and `None` do not count).
fn is_literal_type_or_union<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    union_elements(db, ty).iter().all(|sub| match sub {
        Type::LiteralValue(value) => !matches!(value.kind(), LiteralValueTypeKind::LiteralString),
        _ => false,
    })
}

/// pyright's `replaceEnumTypeWithLiteralValue`: int/str/bytes-based enum literals become
/// their member value types; such enum instance types become the union of all member value
/// types.
fn replace_enum_with_literal_value<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Type<'db> {
    let is_int_str_bytes_enum = |class: ClassLiteral<'db>| -> bool {
        class.iter_mro(db).any(|base| {
            base.into_class().is_some_and(|base_class| {
                matches!(
                    base_class.class_literal(db).known(db),
                    Some(KnownClass::Int | KnownClass::Str | KnownClass::Bytes)
                )
            })
        })
    };
    let replace_one = |sub: Type<'db>| -> Type<'db> {
        match sub {
            Type::LiteralValue(value) => {
                if let LiteralValueTypeKind::Enum(enum_literal) = value.kind() {
                    if is_int_str_bytes_enum(enum_literal.enum_class(db)) {
                        let enum_class_literal = enum_literal.enum_class_literal(db);
                        if let Some((_, value_ty)) = enum_class_literal
                            .members(db)
                            .iter()
                            .find(|(name, _)| name == enum_literal.name(db))
                        {
                            return *value_ty;
                        }
                    }
                }
                sub
            }
            Type::NominalInstance(instance) => {
                let class = instance.class(db, env).class_literal(db);
                if is_int_str_bytes_enum(class) {
                    if let Some(enum_class_literal) = class.into_enum_class(db) {
                        let members = enum_class_literal.members(db);
                        if !members.is_empty() {
                            return UnionType::from_elements(
                                db,
                                env,
                                members.iter().map(|(_, value_ty)| *value_ty),
                            );
                        }
                    }
                }
                sub
            }
            _ => sub,
        }
    };
    match ty {
        Type::Union(union) => {
            UnionType::from_elements(db, env, union.elements(db).iter().map(|e| replace_one(*e)))
        }
        _ => replace_one(ty),
    }
}

/// Syntactic port of the `evaluateStaticBoolExpression` forms that suppress the
/// literal-vs-literal arm: `sys.version_info` / `sys.version_info[...]` / `sys.platform` /
/// `os.name` compared against a literal.
fn is_static_platform_or_version_condition(left: &ast::Expr, right: &ast::Expr) -> bool {
    fn is_module_attr(expr: &ast::Expr, module: &str, attr: &str) -> bool {
        match expr {
            ast::Expr::Attribute(attribute) => {
                attribute.attr.as_str() == attr
                    && matches!(
                        &*attribute.value,
                        ast::Expr::Name(name) if name.id.as_str() == module
                    )
            }
            _ => false,
        }
    }

    if is_module_attr(left, "sys", "version_info") && matches!(right, ast::Expr::Tuple(..)) {
        return true;
    }
    if let ast::Expr::Subscript(subscript) = left {
        if is_module_attr(&subscript.value, "sys", "version_info")
            && matches!(right, ast::Expr::NumberLiteral(..))
        {
            return true;
        }
    }
    if is_module_attr(left, "sys", "platform") && matches!(right, ast::Expr::StringLiteral(..)) {
        return true;
    }
    if is_module_attr(left, "os", "name") && matches!(right, ast::Expr::StringLiteral(..)) {
        return true;
    }
    false
}

/// pyright's `getElementTypeForContainerNarrowing`: specialized typeshed builtin containers
/// only. For `dict`-likes this is the key type; for `tuple` the union of its elements.
fn container_element_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    container: Type<'db>,
) -> Option<Type<'db>> {
    let instance = container.as_nominal_instance()?;
    let class = instance.class(db, env);
    let class_literal = class.class_literal(db);
    let name = class_literal.name(db).as_str();
    if !matches!(
        name,
        "list" | "set" | "frozenset" | "deque" | "tuple" | "dict" | "defaultdict" | "OrderedDict"
    ) {
        return None;
    }
    if !is_typeshed_stdlib_class(db, class_literal) {
        return None;
    }

    if let Some(spec) = container.tuple_instance_spec(db, env) {
        let mut elements: Vec<Type<'db>> = spec.fixed_elements().copied().collect();
        if let Some(variable) = spec.variable_element_type(db) {
            elements.push(variable);
        }
        return Some(UnionType::from_elements(db, env, elements));
    }

    match class {
        ClassType::Generic(alias) => alias.specialization(db).types(db).first().copied(),
        ClassType::NonGeneric(_) => None,
    }
}
