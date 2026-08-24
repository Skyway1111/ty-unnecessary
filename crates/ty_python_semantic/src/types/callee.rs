//! Fork delta (sightline oracle): the definition sites a call's callee type denotes,
//! for the batch protocol's `call_edges` dump. Reports definitions only — which
//! override a call may dispatch to at runtime is sightline's judgement.

use ruff_db::files::FileRange;
use ruff_db::parsed::parsed_module;

use crate::Db;
use crate::types::Type;

/// `(file, name range)` of every definition `ty` denotes as a callee: a plain function,
/// a bound method (its function), a class literal (a constructor call); a union
/// contributes all of its members or nothing. `None` when any part is not a definition
/// — callables, unknowns, instances with `__call__`, dynamic classes.
pub fn callee_definitions<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Vec<FileRange>> {
    let mut out = Vec::new();
    collect(db, ty, &mut out).then_some(out)
}

fn collect<'db>(db: &'db dyn Db, ty: Type<'db>, out: &mut Vec<FileRange>) -> bool {
    let definition = match ty {
        Type::FunctionLiteral(function) => function.definition(db),
        Type::BoundMethod(method) => method.function(db).definition(db),
        Type::ClassLiteral(class) => match class.definition(db) {
            Some(definition) => definition,
            None => return false,
        },
        Type::Union(union) => {
            return union
                .elements(db)
                .iter()
                .all(|element| collect(db, *element, out));
        }
        _ => return false,
    };
    let file = definition.file(db);
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    out.push(definition.focus_range(db, &module));
    true
}
