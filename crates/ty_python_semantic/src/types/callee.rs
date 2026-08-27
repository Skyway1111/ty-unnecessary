//! Fork delta (sightline oracle): the definition sites a call's callee type denotes,
//! for the batch protocol's `call_edges` dump. Reports definitions only — which
//! override a call may dispatch to at runtime is sightline's judgement.
//!
//! Two receivers ty leaves `Unknown` are read through the inferred-return patch
//! (`inferred_return.rs`): a call of a return-unannotated function (`make().m()`) and a
//! name bound exactly once, by a plain assignment, to such a call (`c = make(); c.m()`).
//! The receiver's type is that function's inferred return, and the callee is its member.

use ruff_db::files::{File, FileRange};
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast as ast;
use ty_python_core::definition::DefinitionKind;

use crate::Db;
use crate::semantic_model::{HasType, SemanticModel};
use crate::types::ide_support::{ImportAliasResolution, ResolvedDefinition, definitions_for_name};
use crate::types::inferred_return::with_inferred_return;
use crate::types::{ProgramEnvironment, Type};

/// One definition a callee denotes: a plain function, a bound method's function, or a
/// class (a constructor call).
pub struct CalleeDefinition {
    /// `(file, name range)` of the definition.
    pub definition: FileRange,
    /// A bound method's receiver class.
    pub receiver: ReceiverClass,
}

/// The class of the object a bound method is bound to. A root class that inherits a
/// library method runs the library body, whose template hooks may call back into the
/// root, so a callee outside the root counts as external only with a receiver outside it.
pub enum ReceiverClass {
    /// The callee is not a bound method.
    Unbound,
    /// The receiver's class is defined in this file.
    Defined(File),
    /// The receiver's class is not recoverable (a constrained type variable, an
    /// intersection, a dynamic class).
    Opaque,
}

/// Every definition the callee of `call` denotes; a union contributes all of its members
/// or nothing. `None` when any part is not a definition — callables, unknowns, instances
/// with `__call__`, dynamic classes.
pub fn callee_definitions<'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    call: &ast::ExprCall,
) -> Option<Vec<CalleeDefinition>> {
    let env = model.program_environment();
    let callee = callee_type(db, model, &env, &call.func)?;
    let mut out = Vec::new();
    collect(db, &env, callee, &mut out).then_some(out)
}

/// The callee's inferred type, or — where ty infers `Unknown` for an attribute on a
/// receiver the inferred-return patch can type — the receiver's member.
fn callee_type<'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    env: &ProgramEnvironment<'db>,
    func: &ast::Expr,
) -> Option<Type<'db>> {
    let own = func.inferred_type(model)?;
    let ast::Expr::Attribute(attribute) = func else {
        return Some(own);
    };
    if !own.is_unknown() {
        return Some(own);
    }
    let receiver = returned_receiver(db, model, &attribute.value)?;
    receiver
        .member(db, env, attribute.attr.as_str())
        .ignore_possibly_undefined()
}

/// The inferred return of the return-unannotated function a receiver expression calls,
/// directly or through the one plain assignment binding its name.
fn returned_receiver<'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    receiver: &ast::Expr,
) -> Option<Type<'db>> {
    match receiver {
        ast::Expr::Call(call) => inferred_call_return(db, model, call),
        ast::Expr::Name(name) => {
            let module = parsed_module(db, model.python_file()).load(db);
            let call = sole_call_binding(db, model, &module, name)?;
            inferred_call_return(db, model, call)
        }
        _ => None,
    }
}

/// `f(...)` for a return-unannotated `f` (or a bound method of one): the body-inferred
/// return the reveal patch shows; `None` where that patch does not apply.
fn inferred_call_return<'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    call: &ast::ExprCall,
) -> Option<Type<'db>> {
    let callee = call.func.inferred_type(model)?;
    let Type::Callable(callable) = with_inferred_return(db, callee) else {
        return None;
    };
    let [signature] = callable.signatures(db).overloads.as_slice() else {
        return None;
    };
    let returned = signature.return_ty;
    (!returned.is_unknown()).then_some(returned)
}

/// The call a name is bound to when its scope binds it exactly once, by a plain
/// `name = <call>` in this file.
fn sole_call_binding<'a, 'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    module: &'a ParsedModuleRef,
    name: &ast::ExprName,
) -> Option<&'a ast::ExprCall> {
    let definitions = definitions_for_name(
        model,
        name.id.as_str(),
        name.into(),
        ImportAliasResolution::PreserveAliases,
    );
    let [ResolvedDefinition::Definition(definition)] = definitions.as_slice() else {
        return None;
    };
    if definition.file(db) != model.file() {
        return None;
    }
    let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
        return None;
    };
    if assignment.unpack().is_some() || !assignment.target(module).is_name_expr() {
        return None;
    }
    match assignment.value(module) {
        ast::Expr::Call(call) => Some(call),
        _ => None,
    }
}

fn collect<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    out: &mut Vec<CalleeDefinition>,
) -> bool {
    let (definition, receiver) = match ty {
        Type::FunctionLiteral(function) => (function.definition(db), ReceiverClass::Unbound),
        Type::BoundMethod(method) => (
            method.function(db).definition(db),
            receiver_class(db, env, method.self_instance(db)),
        ),
        Type::ClassLiteral(class) => match class.definition(db) {
            Some(definition) => (definition, ReceiverClass::Unbound),
            None => return false,
        },
        Type::Union(union) => {
            return union
                .elements(db)
                .iter()
                .all(|element| collect(db, env, *element, out));
        }
        _ => return false,
    };
    let file = definition.file(db);
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    out.push(CalleeDefinition {
        definition: definition.focus_range(db, &module),
        receiver,
    });
    true
}

/// The file defining the class of a bound method's `self`: an instance's class, a class
/// literal's own (a classmethod), a literal's fallback class, or a type variable's upper
/// bound (`self` inside a method is `Self@C`, bounded by `C`).
fn receiver_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    self_instance: Type<'db>,
) -> ReceiverClass {
    let class = match self_instance {
        Type::NominalInstance(instance) => Some(instance.class_literal(db, env)),
        Type::ClassLiteral(_) | Type::GenericAlias(_) => self_instance
            .to_class_type(db)
            .map(|class| class.class_literal(db)),
        Type::TypeVar(typevar) => {
            return match typevar.typevar(db).upper_bound(db, env) {
                Some(bound) => receiver_class(db, env, bound),
                None => ReceiverClass::Opaque,
            };
        }
        // `"".join(...)`: the literal's class
        Type::LiteralValue(literal) => {
            return receiver_class(db, env, literal.fallback_instance(db, env));
        }
        _ => None,
    };
    match class.and_then(|class| class.definition(db)) {
        Some(definition) => ReceiverClass::Defined(definition.file(db)),
        None => ReceiverClass::Opaque,
    }
}
