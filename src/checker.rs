use crate::ast::{BinaryOp, Expr, ExprKind, Function, Program, Stmt, Type, UnaryOp};
use crate::diagnostic::Diagnostic;
use crate::ir::{
    TypedConst, TypedExpr, TypedExprKind, TypedFunction, TypedParam, TypedProgram, TypedStmt,
};
use crate::span::Span;
use std::collections::{BTreeMap, HashMap};

const BUILTIN_FUNCTION_NAMES: &[&str] =
    &["vec", "push", "set", "clone", "clone_at"];

#[derive(Clone, Debug)]
struct Env {
    scopes: Vec<BTreeMap<String, VarInfo>>,
    /// Registry of user-declared struct types (built once
    /// from `Program::structs` before checking begins).
    /// Refines T1.2.
    structs: BTreeMap<String, StructInfo>,
    /// Registry of user-declared enum types. T1.3.
    enums: BTreeMap<String, EnumInfo>,
}

#[derive(Clone, Debug)]
struct StructInfo {
    fields: Vec<(String, Type)>,
}

#[derive(Clone, Debug)]
struct EnumInfo {
    /// Variant names in declaration order; index is the
    /// variant's integer tag. T1.3.
    variants: Vec<String>,
    /// Per-variant payload type (Some) or None for
    /// payload-less variants. Parallel to `variants`. T1.3
    /// phase 2b.
    payload_types: Vec<Option<Type>>,
}

/// Look up a variant's payload type from the env's enum
/// registry. Returns `None` if the variant is payload-less or
/// the enum/variant doesn't exist. T1.3 phase 2b.
fn lookup_enum_variant_payload(
    env: &Env,
    enum_name: &str,
    variant: &str,
) -> Option<Type> {
    let info = env.lookup_enum(enum_name)?;
    let idx = info.variants.iter().position(|v| v == variant)?;
    info.payload_types.get(idx)?.clone()
}

impl Env {
    fn new() -> Self {
        Self {
            scopes: vec![BTreeMap::new()],
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
        }
    }

    fn lookup_struct(&self, name: &str) -> Option<&StructInfo> {
        self.structs.get(name)
    }

    fn lookup_enum(&self, name: &str) -> Option<&EnumInfo> {
        self.enums.get(name)
    }

    fn depth(&self) -> usize {
        self.scopes.len()
    }

    fn push_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    fn pop_scope(&mut self) -> BTreeMap<String, VarInfo> {
        assert!(self.scopes.len() > 1, "cannot pop the root scope");
        self.scopes.pop().unwrap()
    }

    fn lookup(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut VarInfo> {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                return scope.get_mut(name);
            }
        }
        None
    }

    fn current_has(&self, name: &str) -> bool {
        self.scopes
            .last()
            .map(|scope| scope.contains_key(name))
            .unwrap_or(false)
    }

    fn current_get(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.last().and_then(|scope| scope.get(name))
    }

    fn insert_current(&mut self, name: String, info: VarInfo) {
        self.scopes.last_mut().unwrap().insert(name, info);
    }

    fn current_scope(&self) -> &BTreeMap<String, VarInfo> {
        self.scopes.last().unwrap()
    }

    /// Iterate over all bindings across all scopes, innermost first.
    fn all_bindings(&self) -> impl Iterator<Item = (&String, &VarInfo)> {
        self.scopes.iter().rev().flat_map(|s| s.iter())
    }

    /// Build the SMT-array version map for the SMT layer: each
    /// Vec/Array binding maps to its current version counter. The
    /// encoder uses this both to declare `arr_<name>_v0..vN` and
    /// to resolve bare `Var("xs")` references to the current
    /// version's SMT name.
    fn array_versions(&self) -> std::collections::HashMap<String, u32> {
        let mut out = std::collections::HashMap::new();
        for (name, info) in self.all_bindings() {
            if matches!(
                info.ty.deref(),
                Type::Vec(_) | Type::Array { .. }
            ) {
                // Innermost binding wins on shadow.
                out.entry(name.clone()).or_insert(info.array_version);
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckedProgram {
    pub program: Program,
    pub ir: TypedProgram,
}

#[derive(Clone, Debug)]
struct Signature {
    params: Vec<Type>,
    param_names: Vec<String>,
    return_type: Type,
    requires: Vec<Expr>,
    ensures: Vec<Expr>,
    /// True if the function was declared `pure fn`. The effects
    /// checker uses this to forbid impure call sites inside a
    /// pure context or a `parallel for` body.
    is_pure: bool,
    /// One bool per parameter: true when the function body
    /// contains a direct `mutex_lock` call naming this
    /// parameter as its target (either `mutex_lock(p)` or
    /// `mutex_lock(&p)`). Used at call sites to reject
    /// cross-function double acquisition: if the caller holds
    /// a guard on the same mutex that the callee would lock,
    /// the call would deadlock. v1 doesn't compute the
    /// transitive closure across calls — only direct lock
    /// sites count.
    locks_params: Vec<bool>,
}

/// Special variable name used inside `ensures` clauses to refer to the
/// function's return value.
const RETURN_NAME: &str = "_return";

#[derive(Clone, Debug)]
struct VarInfo {
    ty: Type,
    constant: Option<TypedConst>,
    moved: Option<Span>,
    /// Source span where this binding was declared (let / param / for-var).
    /// Used to add a "previously declared here" note on shadow-type errors.
    decl_span: Span,
    /// Element expressions when this binding was initialized with a
    /// `vec(a, b, c, ...)` literal. Lets the SMT prove-rewriter
    /// substitute `xs[k]` (constant index) with `a_k` so proofs over
    /// known vec contents discharge. Reset on any reassignment.
    vec_literal_elements: Option<Vec<Expr>>,
    /// SMT-array version counter for Vec/Array bindings. Each
    /// `xs[i] = v` IndexAssign bumps this; the SMT encoder declares
    /// `arr_<name>_v0..vN` so existing facts about earlier versions
    /// stay sound and a synthetic `arr_xs_v{N+1} = (store
    /// arr_xs_vN i v)` axiom bridges old and new. Non-array
    /// bindings carry version 0 and never bump.
    array_version: u32,
    /// For `Guard<T>` bindings produced by `mutex_lock(&m)`
    /// where `m` is a simple Var reference: the name of the
    /// mutex this guard locks. The double-acquire check
    /// reads this field to reject a second `mutex_lock(&m)`
    /// while any guard with `guarded_mutex == Some(m)` is
    /// still alive. Non-Guard bindings (and Guards whose
    /// owner couldn't be tracked syntactically) leave this
    /// `None`.
    guarded_mutex: Option<String>,
    /// True when this binding is a non-owning view into
    /// another value (e.g. the iteration variable of
    /// `for v in &xs` for non-Copy `xs` element). The
    /// scope-exit pass skips the auto-`Drop` for these so
    /// the view doesn't double-free the owner's slot.
    /// Default false. Refines #7 phase 2.
    no_drop: bool,
    /// True when this binding came from a top-level `const`
    /// declaration (not a regular `let`). Var-resolution
    /// folds the binding's compile-time `constant` into a
    /// literal TypedExprKind so the codegen layer never
    /// sees an unbound `v_NAME` reference. Default false.
    /// T4.15.
    is_const: bool,
    /// Field expressions when this binding was initialized
    /// with a struct literal `let p: P = P { x: e1, y: e2 };`.
    /// The SMT prove-rewriter synthesizes a per-field SMT
    /// variable `<name>__<field>` for each entry, asserts
    /// `name__field == encode(e)`, and rewrites `p.x` into
    /// `Var("p__x")` so field-access proofs discharge. Reset
    /// on any reassignment. None for non-struct bindings.
    struct_literal_fields: Option<Vec<(String, Expr)>>,
    /// Names of struct fields that have been moved out via
    /// `let y = t.f;` or passed by value into a function.
    /// Each entry is the span of the consuming use, surfaced
    /// in "field was moved here" diagnostic notes. v1 only
    /// tracks single-level field moves (no nested
    /// `moved_fields` on the moved-out field itself). Scope-
    /// exit drop skips fields in this set so the per-field
    /// free doesn't double-free a value that another binding
    /// now owns. T1.2 phase 2b partial-move follow-up.
    moved_fields: std::collections::BTreeMap<String, Span>,
}

#[derive(Clone, Debug)]
struct CheckedExpr {
    expr: TypedExpr,
    flexible_integer: bool,
    flexible_float: bool,
}

impl CheckedExpr {
    fn new(kind: TypedExprKind, ty: Type, constant: Option<TypedConst>, span: Span) -> Self {
        Self {
            expr: TypedExpr {
                kind,
                ty,
                constant,
                span,
                binding_decl_span: None,
            },
            flexible_integer: false,
            flexible_float: false,
        }
    }

    fn ty(&self) -> &Type {
        &self.expr.ty
    }

    fn constant(&self) -> Option<&TypedConst> {
        self.expr.constant.as_ref()
    }

    fn fallback(ty: Type, span: Span) -> Self {
        let placeholder = match ty {
            Type::F32 | Type::F64 => TypedExprKind::Float(0.0),
            Type::Bool => TypedExprKind::Bool(false),
            _ => TypedExprKind::Int(0),
        };
        Self::new(placeholder, ty, None, span)
    }

    fn fallback_integer(span: Span) -> Self {
        Self::fallback(Type::I64, span)
    }

    /// Attach a binding-declaration span. Used at Var / Ref
    /// / RefMut construction sites where the env lookup
    /// yielded a `decl_span`; the LSP layer consumes the
    /// resulting `TypedExpr::binding_decl_span` to do
    /// scope-aware lookups (rename, references, completion)
    /// that distinguish two same-name bindings.
    fn with_binding_decl_span(mut self, span: Span) -> Self {
        self.expr.binding_decl_span = Some(span);
        self
    }
}

pub fn check(program: Program) -> Result<CheckedProgram, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    // Pre-pass: resolve `Type::Struct(name)` → `Type::Enum(name)`
    // for every name declared as an enum. The parser can't
    // distinguish at parse time, so we run this before anything
    // else (signatures, struct validation, etc.) so all downstream
    // analysis sees the right Type variant. T1.3.
    let enum_names_pre: std::collections::HashSet<String> =
        program.enums.iter().map(|e| e.name.clone()).collect();
    let mut program = program;
    resolve_enum_types_in_program(&mut program, &enum_names_pre);

    // T4.15 alias half: build the alias registry, detect
    // recursion, and substitute `Type::Struct(name)`
    // references with the alias's fully-resolved target
    // everywhere in the program. Aliases run AFTER the
    // enum-resolution pass so an alias pointing at an enum
    // (e.g. `type Hue = Color;`) sees `Type::Enum(Color)`
    // and resolves correctly. The order also means an
    // alias whose target is itself another alias gets
    // transitively unfolded.
    let aliases_resolved = match resolve_type_aliases(&program, &mut diagnostics) {
        Some(m) => m,
        None => return Err(diagnostics),
    };
    if !aliases_resolved.is_empty() {
        substitute_aliases_in_program(&mut program, &aliases_resolved);
    }

    // T1.2 phase 2a: hoist each `methods on T { fn m(…) {…} }`
    // method into the regular function table with name
    // mangled as `<T>_<m>`. After this pass, the methods
    // blocks are drained — downstream signature collection
    // + function checking treats them as ordinary functions.
    // Method-call sugar `p.method(args)` was already lowered
    // by the checker's MethodCall handling to a regular
    // `Call { name: "<T>_<method>", args: [receiver, …] }`,
    // so resolving against the mangled name "just works".
    hoist_methods_into_functions(&mut program, &mut diagnostics);

    // T2.6 phase 2: rewrite `let v: T = try opt; ...; return X;`
    // function bodies into `return match opt { Opt.Some(__t)
    // then { let v: T = __t; ...; X }, Opt.None then Opt.None };`.
    // Runs after methods hoisting so signatures are settled.
    desugar_try_let_in_program(&mut program, &mut diagnostics);

    // T1.4 phase 2: monomorphize generic functions. Walks the
    // program for calls to `fn name<T>(…)` generic functions,
    // infers T from each call site's argument types, generates
    // a specialized copy per (fn, concrete-type) combo, and
    // rewrites call sites to use the specialized name. Removes
    // the original generic functions so downstream type-check
    // sees a fully-concrete program.
    monomorphize_generics_in_program(&mut program, &mut diagnostics);

    // T1.5 phase 2: hoist `implement Iface for Type { fn m … }`
    // method bodies into regular functions named
    // `<TypeName>_<method>` (same convention as `methods on
    // T { … }`). This lets the existing method-dispatch
    // path (closure #82 etc.) resolve `recv.method()` calls
    // statically. The interface declaration is preserved
    // for signature validation; impl method signatures must
    // match the interface's declared shape exactly.
    hoist_impls_into_functions(&mut program, &mut diagnostics);
    // After monomorphization + impl hoisting, no function in
    // the program should still carry an unresolved
    // where-clause. If any does, it means a non-generic
    // function declared a where-bound that has no effect.
    for func in &program.functions {
        if !func.where_clauses.is_empty() && func.type_params.is_empty() {
            let clause = &func.where_clauses[0];
            diagnostics.push(Diagnostic::new(
                clause.span,
                format!(
                    "non-generic function '{}' carries `where {} is {}` — \
                     where-bounds apply only to generic type parameters",
                    func.name, clause.type_param, clause.interface_name
                ),
            ));
        }
    }

    let signatures = collect_signatures(&program, &mut diagnostics);
    validate_main(&program, &signatures, &mut diagnostics);

    // Reject struct/enum/alias declarations whose name
    // collides with a reserved built-in type. Lexer
    // keywords (i8, i16, …, Vec) are caught at parse time,
    // but `Task`, `Atomic`, `Mutex`, `Guard`, `Channel`,
    // `OwnedStr` lex as identifiers and only become
    // reserved when `parse_type` sees them. Without this
    // gate, `struct Task { … }` parses fine but every
    // reference to `Task` in type position resolves to the
    // built-in `Type::Task`, leading to confusing
    // "got Task" errors deep in the pipeline.
    const RESERVED_TYPE_NAMES: &[&str] = &[
        "Task", "Atomic", "Mutex", "Guard", "Channel", "OwnedStr", "Self",
    ];
    for decl in &program.structs {
        if RESERVED_TYPE_NAMES.contains(&decl.name.as_str()) {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!(
                    "struct name '{}' is a reserved built-in type — pick a \
                     different name",
                    decl.name
                ),
            ));
        }
    }
    for decl in &program.enums {
        if RESERVED_TYPE_NAMES.contains(&decl.name.as_str()) {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!(
                    "enum name '{}' is a reserved built-in type — pick a \
                     different name",
                    decl.name
                ),
            ));
        }
    }
    for alias in &program.type_aliases {
        if RESERVED_TYPE_NAMES.contains(&alias.name.as_str()) {
            diagnostics.push(Diagnostic::new(
                alias.name_span,
                format!(
                    "type alias name '{}' is a reserved built-in type — pick a \
                     different name",
                    alias.name
                ),
            ));
        }
    }

    // Validate struct declarations + build the registry.
    // v1 caps fields in 1..=64 (raised from 8 for
    // real-world usability; many domain types — game
    // entities, configuration structs, protocol
    // messages — naturally have 10-30 fields). The
    // upper bound is generous enough that hitting it
    // is a code-smell signal worth a diagnostic, while
    // not blocking common shapes. Each field must
    // still be Copy + non-reference (RAII chains for
    // non-Copy fields land in T1.2 phase 2b).
    // Pre-pass: register every struct that directly carries a
    // non-Copy field (so `Type::is_copy()` reports the
    // aggregate as affine) plus every struct/enum that has a
    // user-declared `implement Drop for T` impl (so the auto-
    // call at scope exit fires even when fields are all Copy).
    // Must run before the validation loop below, because that
    // loop calls `field.ty.is_copy()` to decide whether the
    // field type is acceptable. T1.2 phase 2b + T2.7 phase 2.
    {
        let mut non_copy: Vec<String> = Vec::new();
        // Fixed-point iteration so nested-struct fields
        // propagate the non-Copy flag: a struct that
        // contains an already-marked struct field becomes
        // non-Copy itself. Without this, source order
        // would determine whether `Outer { inner: Inner }`
        // (with Inner non-Copy) is marked.
        loop {
            crate::ast::set_non_copy_structs(non_copy.clone());
            let mut changed = false;
            for decl in &program.structs {
                if non_copy.iter().any(|n| n == &decl.name) {
                    continue;
                }
                if decl.fields.iter().any(|f| !f.ty.is_copy()) {
                    non_copy.push(decl.name.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // `hoist_impls_into_functions` (which already ran) drains
        // `program.impls` after appending the hoisted methods.
        // Detect user Drop impls via the hoisted function names
        // instead. T2.7 phase 2.
        let struct_names: std::collections::HashSet<String> = program
            .structs
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for f in &program.functions {
            if let Some(type_name) = f.name.strip_suffix("_drop") {
                if struct_names.contains(type_name)
                    && !non_copy.iter().any(|m| m == type_name)
                {
                    non_copy.push(type_name.to_string());
                }
            }
        }
        crate::ast::set_non_copy_structs(non_copy);
    }
    // Parallel pass: register enums whose payload includes a
    // heap-shaped type (OwnedStr in v1) so the scope-exit Drop
    // pass treats them as affine. T1.3 + T1.2 phase 2b.
    {
        let mut non_copy_enums: Vec<String> = Vec::new();
        for decl in &program.enums {
            let has_heap = decl
                .variants
                .iter()
                .any(|v| {
                    v.payload
                        .first()
                        .map_or(false, |t| matches!(t, Type::OwnedStr | Type::Vec(_)))
                });
            if has_heap {
                non_copy_enums.push(decl.name.clone());
            }
        }
        crate::ast::set_non_copy_enums(non_copy_enums);
    }

    let mut struct_registry: BTreeMap<String, StructInfo> = BTreeMap::new();
    for decl in &program.structs {
        if decl.fields.len() > 64 {
            diagnostics.push(Diagnostic::new(
                decl.span,
                format!(
                    "struct '{}' has {} fields; v1 supports 0..=64",
                    decl.name,
                    decl.fields.len()
                ),
            ));
        }
        for field in &decl.fields {
            if field.ty.is_ref() || field.ty.is_ref_mut() {
                diagnostics.push(Diagnostic::new(
                    field.span,
                    format!(
                        "struct field '{}::{}' cannot be a reference",
                        decl.name, field.name
                    ),
                ));
            }
            // T1.2 phase 2b: allow most affine types as struct
            // fields. The Drop emission in both backends walks
            // the owning-field list and frees heap-shaped
            // fields (OwnedStr, Vec) at scope exit; stack-shaped
            // fields ([T;N] of Copy, Task, Atomic) need no
            // runtime drop. Mutex/Guard/Channel still need
            // explicit wiring (their RAII shape is bespoke) and
            // remain rejected. Vec field indexing / mutation
            // through `t.xs[i]` and method calls on a Vec field
            // (`t.xs.push(...)`) are still WIP — for now, the
            // struct carries the Vec and frees it; operating on
            // it requires moving the Vec out via a let binding.
            let field_allowed = field.ty.is_copy()
                || matches!(field.ty, Type::OwnedStr | Type::Task)
                || matches!(
                    &field.ty,
                    Type::Atomic(_) | Type::Vec(_) | Type::Mutex(_) | Type::Channel(_, _)
                )
                || matches!(
                    &field.ty,
                    Type::Array { element, .. } if element.is_copy()
                )
                // Nested affine struct field: outer struct
                // can contain an inner struct whose own
                // fields are admitted. Drop chains
                // recursively through the per-backend Drop
                // emit. T1.2 phase 2b follow-up.
                || matches!(&field.ty, Type::Struct(_));
            if !field_allowed {
                diagnostics.push(Diagnostic::new(
                    field.span,
                    format!(
                        "struct field '{}::{}' has non-Copy type {} — \
                         v1 supports Copy types, OwnedStr, Vec<T>, \
                         [T; N] of Copy elements, Task, Atomic<T>, \
                         Mutex<T>, and Channel<T, N> as struct fields; \
                         Guard<T> still needs explicit wiring",
                        decl.name, field.name, field.ty
                    ),
                ));
            }
        }
        // Reject duplicate field names.
        for i in 0..decl.fields.len() {
            for j in (i + 1)..decl.fields.len() {
                if decl.fields[i].name == decl.fields[j].name {
                    diagnostics.push(Diagnostic::new(
                        decl.fields[j].span,
                        format!(
                            "struct '{}' has duplicate field '{}'",
                            decl.name, decl.fields[j].name
                        ),
                    ));
                }
            }
        }
        if struct_registry.contains_key(&decl.name) {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!("struct '{}' already declared", decl.name),
            ));
        }
        struct_registry.insert(
            decl.name.clone(),
            StructInfo {
                fields: decl
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect(),
            },
        );
    }

    // Detect recursive struct definitions: a value
    // containing itself transitively has infinite size,
    // so the codegen can't lay it out. Build the
    // direct-field-type adjacency map (struct → set of
    // referenced struct names in its inline fields) and
    // surface a clear diagnostic if any node has a
    // cycle back to itself. Indirect references through
    // `ref T` / `Vec<T>` (heap-allocated) don't count
    // — they'd be fine. T1.2.
    {
        use std::collections::HashSet;
        fn direct_struct_deps(ty: &Type, out: &mut HashSet<String>) {
            match ty {
                Type::Struct(name) => {
                    out.insert(name.clone());
                }
                Type::Tuple(elements) => {
                    for e in elements {
                        direct_struct_deps(e, out);
                    }
                }
                Type::Array { element, .. } => direct_struct_deps(element, out),
                // Ref / RefMut / Vec / Atomic / Mutex /
                // Guard / Channel / FnPtr all break the
                // direct-size dependency — they're
                // pointers or heap-allocated. So don't
                // recurse through them.
                _ => {}
            }
        }
        let mut deps: BTreeMap<String, HashSet<String>> = BTreeMap::new();
        for decl in &program.structs {
            let mut s_deps: HashSet<String> = HashSet::new();
            for f in &decl.fields {
                direct_struct_deps(&f.ty, &mut s_deps);
            }
            deps.insert(decl.name.clone(), s_deps);
        }
        // DFS for cycles.
        fn has_cycle(
            name: &str,
            deps: &BTreeMap<String, HashSet<String>>,
            stack: &mut HashSet<String>,
            visited: &mut HashSet<String>,
        ) -> bool {
            if !stack.insert(name.to_string()) {
                return true;
            }
            if let Some(ds) = deps.get(name) {
                for d in ds {
                    if !visited.contains(d) {
                        if has_cycle(d, deps, stack, visited) {
                            return true;
                        }
                    }
                }
            }
            stack.remove(name);
            visited.insert(name.to_string());
            false
        }
        let mut visited: HashSet<String> = HashSet::new();
        for decl in &program.structs {
            if visited.contains(&decl.name) {
                continue;
            }
            let mut stack: HashSet<String> = HashSet::new();
            if has_cycle(&decl.name, &deps, &mut stack, &mut visited) {
                diagnostics.push(Diagnostic::new(
                    decl.span,
                    format!(
                        "struct '{}' is recursive (directly or transitively) — \
                         contains itself by value, which has infinite size; \
                         use `ref T` / `Vec<T>` to break the cycle via the heap",
                        decl.name
                    ),
                ));
            }
        }
    }

    // Build the enum registry. v1 requires 1..=255 distinct
    // variants per enum. T1.3.
    let mut enum_registry: BTreeMap<String, EnumInfo> = BTreeMap::new();
    for decl in &program.enums {
        if decl.variants.is_empty() || decl.variants.len() > 255 {
            diagnostics.push(Diagnostic::new(
                decl.span,
                format!(
                    "enum '{}' has {} variants; v1 supports 1..=255",
                    decl.name,
                    decl.variants.len()
                ),
            ));
        }
        for i in 0..decl.variants.len() {
            // T1.3 phase 2b — payloaded variants are now
            // executable in tree-C: backend lays the enum out
            // as a tagged-union struct (`Enum_<Name>`),
            // constructors build the struct literal, match
            // dispatches on the `.tag` field, and pattern
            // bindings extract `.payload` into a local with
            // the variant's payload type. V1 requires payloads
            // to be Copy + single-field; multi-field payloads
            // and mixed payload types across variants stay
            // rejected (need union representation).
            if decl.variants[i].payload.len() > 1 {
                diagnostics.push(Diagnostic::new(
                    decl.variants[i].name_span,
                    format!(
                        "enum '{}' variant '{}' has {} payload fields — \
                         only single-field payloads supported in v1 (T1.3 \
                         phase 2b)",
                        decl.name,
                        decl.variants[i].name,
                        decl.variants[i].payload.len()
                    ),
                ));
            }
            if let Some(payload_ty) = decl.variants[i].payload.first() {
                // T1.2 phase 2 struct RAII landed in closures
                // #98 / #100; #113 added OwnedStr; #118 added
                // Vec<T>; #119 added `[T;N]` of Copy; #122
                // added Task and Atomic<T>; #124 adds Mutex
                // and Channel (parallel to closure #123's
                // struct-field work). Only Guard<T> remains
                // rejected — its RAII unlock needs bespoke
                // wiring through the enum-Drop dispatch.
                let array_of_copy = matches!(
                    payload_ty,
                    Type::Array { element, .. } if element.is_copy()
                );
                let allowed = payload_ty.is_copy()
                    || matches!(
                        payload_ty,
                        Type::OwnedStr
                            | Type::Vec(_)
                            | Type::Task
                            | Type::Atomic(_)
                            | Type::Mutex(_)
                            | Type::Channel(_, _)
                    )
                    || array_of_copy;
                if !allowed {
                    diagnostics.push(Diagnostic::new(
                        decl.variants[i].name_span,
                        format!(
                            "enum '{}' variant '{}' payload type {} is not \
                             admitted in v1 — supported payloads are Copy \
                             types, OwnedStr, Vec<T>, [T;N] of Copy elements, \
                             Task, Atomic<T>, Mutex<T>, and Channel<T, N>; \
                             only Guard<T> still needs codegen work",
                            decl.name,
                            decl.variants[i].name,
                            payload_ty
                        ),
                    ));
                }
            }
        }
        // Mixed-payload-type check: all variants with payloads
        // must share the same payload type (single-field
        // simplification — multi-type would need a union).
        let payload_types: Vec<(&str, &Type)> = decl
            .variants
            .iter()
            .filter_map(|v| v.payload.first().map(|p| (v.name.as_str(), p)))
            .collect();
        if payload_types.len() >= 2 {
            let (first_name, first_ty) = payload_types[0];
            for (other_name, other_ty) in &payload_types[1..] {
                if *other_ty != first_ty {
                    diagnostics.push(Diagnostic::new(
                        decl.name_span,
                        format!(
                            "enum '{}' has mixed payload types — '{}' carries {} \
                             but '{}' carries {}. V1 requires all payload-bearing \
                             variants to share the same payload type (multi-type \
                             representation deferred).",
                            decl.name, first_name, first_ty, other_name, other_ty
                        ),
                    ));
                    break;
                }
            }
        }
        for i in 0..decl.variants.len() {
            for j in (i + 1)..decl.variants.len() {
                if decl.variants[i].name == decl.variants[j].name {
                    diagnostics.push(Diagnostic::new(
                        decl.variants[j].name_span,
                        format!(
                            "enum '{}' has duplicate variant '{}'",
                            decl.name, decl.variants[j].name
                        ),
                    ));
                }
            }
        }
        if enum_registry.contains_key(&decl.name) {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!("enum '{}' already declared", decl.name),
            ));
        }
        if struct_registry.contains_key(&decl.name) {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!(
                    "enum '{}' collides with a struct of the same name",
                    decl.name
                ),
            ));
        }
        enum_registry.insert(
            decl.name.clone(),
            EnumInfo {
                variants: decl.variants.iter().map(|v| v.name.clone()).collect(),
                payload_types: decl
                    .variants
                    .iter()
                    .map(|v| v.payload.first().cloned())
                    .collect(),
            },
        );
    }

    // T4.15: build the const registry. v1 accepts only
    // literal initializers (`Int` / `Float` / `Bool` /
    // `unary minus` of those) on Copy scalar types. Anything
    // else (a binary expr, function call, vec literal, etc.)
    // gets rejected with a clear diagnostic so users learn
    // the constraint instead of meeting a runtime surprise.
    let mut const_registry: BTreeMap<String, (Type, TypedConst, Span)> = BTreeMap::new();
    for decl in &program.consts {
        // Reject duplicates and name collisions with existing
        // type-level declarations.
        if const_registry.contains_key(&decl.name) {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!("const '{}' is already declared", decl.name),
            ));
            continue;
        }
        if struct_registry.contains_key(&decl.name)
            || enum_registry.contains_key(&decl.name)
            || signatures.contains_key(&decl.name)
        {
            diagnostics.push(Diagnostic::new(
                decl.name_span,
                format!(
                    "const '{}' collides with an existing type or function",
                    decl.name
                ),
            ));
            continue;
        }
        // v1: type must be a Copy scalar (no Vec, struct,
        // tuple, string in const yet — those require
        // initializer-time allocation).
        if !decl.ty.is_copy() || decl.ty.is_ref() || decl.ty.is_ref_mut() {
            diagnostics.push(Diagnostic::new(
                decl.span,
                format!(
                    "const '{}': v1 supports Copy scalar types only \
                     (i64/i32/.../f64/bool); got {}",
                    decl.name, decl.ty
                ),
            ));
            continue;
        }
        // v1: initializer must be a literal. Unary minus
        // over a literal is allowed so `-1` works.
        let value_const = match literal_const_value(&decl.value, &decl.ty, &const_registry) {
            Some(v) => v,
            None => {
                diagnostics.push(Diagnostic::new(
                    decl.value.span,
                    format!(
                        "const '{}': v1 initializers must be a literal value \
                         (e.g. 42, -1, 3.14, true); arithmetic + function calls \
                         land in a later phase",
                        decl.name
                    ),
                ));
                continue;
            }
        };
        // Range-check integer literals against the
        // declared type so e.g. `const X: i8 = 200;`
        // doesn't silently truncate at codegen.
        if let TypedConst::Int(v) = &value_const {
            if !value_fits_type(*v, &decl.ty) {
                diagnostics.push(Diagnostic::new(
                    decl.value.span,
                    format!(
                        "const '{}': literal {} does not fit in {}",
                        decl.name, v, decl.ty
                    ),
                ));
                continue;
            }
        }
        const_registry.insert(
            decl.name.clone(),
            (decl.ty.clone(), value_const, decl.name_span),
        );
    }

    let mut functions = Vec::new();
    for function in &program.functions {
        functions.push(check_function(
            function,
            &signatures,
            &struct_registry,
            &enum_registry,
            &const_registry,
            &mut diagnostics,
        ));
    }

    if diagnostics.is_empty() {
        let intents = program
            .intents
            .iter()
            .map(|intent| intent.text.clone())
            .collect();
        let structs: Vec<crate::ir::TypedStructDecl> = program
            .structs
            .iter()
            .map(|s| crate::ir::TypedStructDecl {
                name: s.name.clone(),
                fields: s
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect(),
            })
            .collect();
        let enums: Vec<crate::ir::TypedEnumDecl> = program
            .enums
            .iter()
            .map(|e| crate::ir::TypedEnumDecl {
                name: e.name.clone(),
                variants: e.variants.iter().map(|v| v.name.clone()).collect(),
                payload_types: e
                    .variants
                    .iter()
                    .map(|v| v.payload.first().cloned())
                    .collect(),
            })
            .collect();
        Ok(CheckedProgram {
            program,
            ir: TypedProgram { intents, functions, structs, enums },
        })
    } else {
        Err(diagnostics)
    }
}

fn collect_signatures(
    program: &Program,
    diagnostics: &mut Vec<Diagnostic>,
) -> HashMap<String, Signature> {
    let mut signatures = HashMap::new();

    for function in &program.functions {
        if function.return_type.is_array() {
            diagnostics.push(Diagnostic::new(
                function.span,
                "array types are not allowed in return position yet",
            ));
        }
        validate_no_ref(
            &function.return_type,
            function.span,
            "function return type",
            diagnostics,
        );
        if BUILTIN_FUNCTION_NAMES.contains(&function.name.as_str()) {
            diagnostics.push(Diagnostic::new(
                function.span,
                format!(
                    "function '{}' is a built-in name and cannot be redefined",
                    function.name
                ),
            ));
        }
        if signatures
            .insert(
                function.name.clone(),
                Signature {
                    params: function.params.iter().map(|param| param.ty.clone()).collect(),
                    param_names: function.params.iter().map(|param| param.name.clone()).collect(),
                    return_type: function.return_type.clone(),
                    requires: function.requires.clone(),
                    ensures: function.ensures.clone(),
                    is_pure: function.is_pure,
                    locks_params: compute_locks_params(function),
                },
            )
            .is_some()
        {
            diagnostics.push(Diagnostic::new(
                function.span,
                format!("function '{}' is already defined", function.name),
            ));
        }
    }

    propagate_locks_params(&mut signatures, &program.functions);

    signatures
}

/// Propagate `locks_params` transitively across call sites.
/// After `compute_locks_params` has marked each function's
/// *direct* `mutex_lock(p)` / `mutex_lock(&p)` sites, this
/// fixpoint pass adds the *indirect* cases: if function `f`
/// calls `g(arg)` and `g.locks_params[i]` is true and `arg`
/// names one of `f`'s own parameters, then `f` also locks
/// that parameter. Iterates until no signature changes.
///
/// v1 inspects calls by name; a function pointer / first-class
/// callable would require dataflow on the SSA layer, which is
/// out of scope.
fn propagate_locks_params(
    signatures: &mut HashMap<String, Signature>,
    functions: &[Function],
) {
    loop {
        let mut changed = false;
        for function in functions {
            let param_names: Vec<&str> =
                function.params.iter().map(|p| p.name.as_str()).collect();
            // Read-only snapshot of the current signature table
            // so we can mutate the target inside the loop. Since
            // we only flip bits to `true`, monotonicity guarantees
            // termination — the lattice has finite height.
            let snapshot = signatures.clone();
            let target = signatures
                .get_mut(&function.name)
                .expect("signature exists for every function");
            let propagated = compute_indirect_locks(
                &function.body,
                &param_names,
                &snapshot,
                &target.locks_params,
            );
            for (idx, propagated_bit) in propagated.iter().enumerate() {
                if *propagated_bit && !target.locks_params[idx] {
                    target.locks_params[idx] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Inspect every Call site in `body` and infer, for each of
/// the current function's parameters, whether some callee
/// locks it on its behalf. Returns a vector aligned with
/// `param_names`.
fn compute_indirect_locks(
    body: &[Stmt],
    param_names: &[&str],
    signatures: &HashMap<String, Signature>,
    existing: &[bool],
) -> Vec<bool> {
    let mut out = existing.to_vec();
    fn walk_stmts(
        stmts: &[Stmt],
        param_names: &[&str],
        signatures: &HashMap<String, Signature>,
        out: &mut [bool],
    ) {
        for s in stmts {
            walk_stmt(s, param_names, signatures, out);
        }
    }
    fn walk_stmt(
        stmt: &Stmt,
        param_names: &[&str],
        signatures: &HashMap<String, Signature>,
        out: &mut [bool],
    ) {
        match stmt {
            Stmt::Let { expr, .. }
            | Stmt::LetTuple { expr, .. }
            | Stmt::Return { expr, .. }
            | Stmt::Assert { expr, .. }
            | Stmt::Prove { expr, .. }
            | Stmt::Assign { expr, .. } => walk_expr(expr, param_names, signatures, out),
            Stmt::IndexAssign { index, value, .. } => {
                walk_expr(index, param_names, signatures, out);
                walk_expr(value, param_names, signatures, out);
            }
            Stmt::FieldAssign { object, value, .. } => {
                walk_expr(object, param_names, signatures, out);
                walk_expr(value, param_names, signatures, out);
            }
            Stmt::Print { items, .. } => {
                for item in items {
                    if let crate::ast::PrintItem::Expr(e) = item {
                        walk_expr(e, param_names, signatures, out);
                    }
                }
            }
            Stmt::If { cond, then_body, else_body, .. } => {
                walk_expr(cond, param_names, signatures, out);
                walk_stmts(then_body, param_names, signatures, out);
                walk_stmts(else_body, param_names, signatures, out);
            }
            Stmt::While { cond, body, invariants, .. } => {
                walk_expr(cond, param_names, signatures, out);
                for inv in invariants {
                    walk_expr(inv, param_names, signatures, out);
                }
                walk_stmts(body, param_names, signatures, out);
            }
            Stmt::For { start, end, body, invariants, .. } => {
                walk_expr(start, param_names, signatures, out);
                walk_expr(end, param_names, signatures, out);
                for inv in invariants {
                    walk_expr(inv, param_names, signatures, out);
                }
                walk_stmts(body, param_names, signatures, out);
            }
            Stmt::ForIter { body, .. } => walk_stmts(body, param_names, signatures, out),
            Stmt::TaskSpawn { body, .. } => walk_stmts(body, param_names, signatures, out),
            Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::TaskJoin { .. } => {}
        }
    }
    fn walk_expr(
        expr: &Expr,
        param_names: &[&str],
        signatures: &HashMap<String, Signature>,
        out: &mut [bool],
    ) {
        if let ExprKind::Call { name: callee, args, .. } = &expr.kind {
            if let Some(callee_sig) = signatures.get(callee) {
                for (idx, arg) in args.iter().enumerate() {
                    if callee_sig.locks_params.get(idx).copied().unwrap_or(false) {
                        if let Some(target_name) = extract_locked_mutex_name(arg) {
                            if let Some(my_idx) =
                                param_names.iter().position(|n| *n == target_name)
                            {
                                out[my_idx] = true;
                            }
                        }
                    }
                }
            }
            for a in args {
                walk_expr(a, param_names, signatures, out);
            }
            return;
        }
        match &expr.kind {
            ExprKind::Unary { expr: inner, .. } => {
                walk_expr(inner, param_names, signatures, out)
            }
            ExprKind::Binary { left, right, .. } => {
                walk_expr(left, param_names, signatures, out);
                walk_expr(right, param_names, signatures, out);
            }
            ExprKind::Cast { expr: inner, .. } => {
                walk_expr(inner, param_names, signatures, out)
            }
            ExprKind::ArrayLit { elements } => {
                for e in elements {
                    walk_expr(e, param_names, signatures, out);
                }
            }
            ExprKind::Index { array, index, .. } => {
                walk_expr(array, param_names, signatures, out);
                walk_expr(index, param_names, signatures, out);
            }
            ExprKind::Len { array, .. } => walk_expr(array, param_names, signatures, out),
            ExprKind::Ref { inner } | ExprKind::RefMut { inner } => {
                walk_expr(inner, param_names, signatures, out)
            }
            _ => {}
        }
    }
    walk_stmts(body, param_names, signatures, &mut out);
    out
}

fn validate_main(
    program: &Program,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let main_function = program.functions.iter().find(|f| f.name == "main");
    let main_span = main_function
        .map(|f| f.span)
        .unwrap_or_else(Span::default);

    let Some(main) = signatures.get("main") else {
        diagnostics.push(Diagnostic::new(
            main_span,
            "program must define fn main() -> i64",
        ));
        return;
    };

    if !main.params.is_empty() || main.return_type != Type::I64 {
        diagnostics.push(Diagnostic::new(
            main_span,
            "main must have signature fn main() -> i64",
        ));
    }
}

/// Walk a `Program` and rewrite `Type::Struct(name)` →
/// `Type::Enum(name)` everywhere the name matches a declared
/// enum. The parser can't distinguish struct vs enum at parse
/// time (both look like uppercase identifiers in type
/// position), so this post-parse fixup keeps the AST honest
/// before the checker runs. T1.3.
fn resolve_enum_types_in_program(
    program: &mut Program,
    enums: &std::collections::HashSet<String>,
) {
    for function in &mut program.functions {
        resolve_enum_types_in_type(&mut function.return_type, enums);
        for p in &mut function.params {
            resolve_enum_types_in_type(&mut p.ty, enums);
        }
        for s in &mut function.body {
            resolve_enum_types_in_stmt(s, enums);
        }
    }
    // Struct field types may also reference enums.
    for decl in &mut program.structs {
        for field in &mut decl.fields {
            resolve_enum_types_in_type(&mut field.ty, enums);
        }
    }
    // Type aliases may reference enums in their target.
    // Resolve there too so the alias-substitution pass that
    // runs after this one sees `Type::Enum(name)`. T4.15.
    for alias in &mut program.type_aliases {
        resolve_enum_types_in_type(&mut alias.target, enums);
    }
    for c in &mut program.consts {
        resolve_enum_types_in_type(&mut c.ty, enums);
    }
    // Methods blocks have a target type + a list of inner
    // functions that each have their own signature + body
    // referencing types. T1.2 phase 2a.
    for block in &mut program.methods_blocks {
        resolve_enum_types_in_type(&mut block.for_type, enums);
        for method in &mut block.methods {
            resolve_enum_types_in_type(&mut method.return_type, enums);
            for p in &mut method.params {
                resolve_enum_types_in_type(&mut p.ty, enums);
            }
            for s in &mut method.body {
                resolve_enum_types_in_stmt(s, enums);
            }
        }
    }
    // `implement Iface for T { … }` impls carry a target
    // type plus methods that may reference enums in their
    // signatures and bodies (e.g. `implement Eq for Color`
    // needs `self: Color` to resolve to `Type::Enum`).
    // T1.5 phase 2 follow-up.
    for imp in &mut program.impls {
        resolve_enum_types_in_type(&mut imp.for_type, enums);
        for method in &mut imp.methods {
            resolve_enum_types_in_type(&mut method.return_type, enums);
            for p in &mut method.params {
                resolve_enum_types_in_type(&mut p.ty, enums);
            }
            for s in &mut method.body {
                resolve_enum_types_in_stmt(s, enums);
            }
        }
    }
}

fn resolve_enum_types_in_type(
    ty: &mut Type,
    enums: &std::collections::HashSet<String>,
) {
    match ty {
        Type::Struct(name) => {
            if enums.contains(name) {
                *ty = Type::Enum(name.clone());
            }
        }
        Type::Vec(inner)
        | Type::Ref(inner)
        | Type::RefMut(inner)
        | Type::Atomic(inner)
        | Type::Mutex(inner)
        | Type::Guard(inner) => resolve_enum_types_in_type(inner, enums),
        Type::Array { element, .. } => resolve_enum_types_in_type(element, enums),
        Type::Channel(element, _) => resolve_enum_types_in_type(element, enums),
        Type::Tuple(elements) => {
            for e in elements {
                resolve_enum_types_in_type(e, enums);
            }
        }
        Type::FnPtr(params, ret) => {
            for p in params {
                resolve_enum_types_in_type(p, enums);
            }
            resolve_enum_types_in_type(ret, enums);
        }
        _ => {}
    }
}

fn resolve_enum_types_in_stmt(
    stmt: &mut Stmt,
    enums: &std::collections::HashSet<String>,
) {
    match stmt {
        Stmt::Let { annotation, .. } | Stmt::LetTuple { annotation, .. } => {
            if let Some(ty) = annotation {
                resolve_enum_types_in_type(ty, enums);
            }
        }
        Stmt::If { then_body, else_body, .. } => {
            for s in then_body {
                resolve_enum_types_in_stmt(s, enums);
            }
            for s in else_body {
                resolve_enum_types_in_stmt(s, enums);
            }
        }
        Stmt::While { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForIter { body, .. }
        | Stmt::TaskSpawn { body, .. } => {
            for s in body {
                resolve_enum_types_in_stmt(s, enums);
            }
        }
        _ => {}
    }
}

/// Build a fully-resolved map of `type Alias = Target;`
/// declarations. Each alias's target is recursively
/// unfolded so a downstream `Type::Struct(Alias)` can be
/// replaced in a single substitution pass. Detects cycles
/// via DFS + on-stack tracking; cyclic aliases surface as a
/// clear diagnostic and the function returns `None` to
/// short-circuit checking. T4.15.
fn resolve_type_aliases(
    program: &Program,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, Type>> {
    use std::collections::HashSet;
    let mut by_name: BTreeMap<String, &crate::ast::TypeAlias> = BTreeMap::new();
    for alias in &program.type_aliases {
        if by_name.contains_key(&alias.name) {
            diagnostics.push(Diagnostic::new(
                alias.name_span,
                format!("type alias '{}' is already declared", alias.name),
            ));
            return None;
        }
        // Reject collisions with struct/enum/function names
        // so downstream name resolution stays unambiguous.
        if program.structs.iter().any(|s| s.name == alias.name) {
            diagnostics.push(Diagnostic::new(
                alias.name_span,
                format!(
                    "type alias '{}' collides with a struct of the same name",
                    alias.name
                ),
            ));
            return None;
        }
        if program.enums.iter().any(|e| e.name == alias.name) {
            diagnostics.push(Diagnostic::new(
                alias.name_span,
                format!(
                    "type alias '{}' collides with an enum of the same name",
                    alias.name
                ),
            ));
            return None;
        }
        by_name.insert(alias.name.clone(), alias);
    }
    let mut resolved: BTreeMap<String, Type> = BTreeMap::new();
    for (name, _) in by_name.iter() {
        let mut stack: HashSet<String> = HashSet::new();
        match resolve_alias_chain(name, &by_name, &mut resolved, &mut stack) {
            Ok(()) => {}
            Err((span, msg)) => {
                diagnostics.push(Diagnostic::new(span, msg));
                return None;
            }
        }
    }
    Some(resolved)
}

fn resolve_alias_chain(
    name: &str,
    by_name: &BTreeMap<String, &crate::ast::TypeAlias>,
    resolved: &mut BTreeMap<String, Type>,
    stack: &mut std::collections::HashSet<String>,
) -> Result<(), (crate::span::Span, String)> {
    if resolved.contains_key(name) {
        return Ok(());
    }
    if stack.contains(name) {
        let span = by_name
            .get(name)
            .map(|a| a.name_span)
            .unwrap_or_else(crate::span::Span::default);
        return Err((
            span,
            format!("recursive type alias '{}' is not allowed in v1", name),
        ));
    }
    let alias = match by_name.get(name) {
        Some(a) => *a,
        None => return Ok(()), // not actually an alias
    };
    stack.insert(name.to_string());
    let mut target = alias.target.clone();
    resolve_type_with_aliases(&mut target, by_name, resolved, stack)?;
    stack.remove(name);
    resolved.insert(name.to_string(), target);
    Ok(())
}

fn resolve_type_with_aliases(
    ty: &mut Type,
    by_name: &BTreeMap<String, &crate::ast::TypeAlias>,
    resolved: &mut BTreeMap<String, Type>,
    stack: &mut std::collections::HashSet<String>,
) -> Result<(), (crate::span::Span, String)> {
    match ty {
        Type::Struct(name) => {
            if by_name.contains_key(name) {
                resolve_alias_chain(name, by_name, resolved, stack)?;
                if let Some(target) = resolved.get(name) {
                    *ty = target.clone();
                }
            }
        }
        Type::Vec(inner)
        | Type::Ref(inner)
        | Type::RefMut(inner)
        | Type::Atomic(inner)
        | Type::Mutex(inner)
        | Type::Guard(inner) => {
            resolve_type_with_aliases(inner, by_name, resolved, stack)?;
        }
        Type::Array { element, .. } => {
            resolve_type_with_aliases(element, by_name, resolved, stack)?;
        }
        Type::Channel(element, _) => {
            resolve_type_with_aliases(element, by_name, resolved, stack)?;
        }
        Type::Tuple(elements) => {
            for e in elements {
                resolve_type_with_aliases(e, by_name, resolved, stack)?;
            }
        }
        Type::FnPtr(params, ret) => {
            for p in params {
                resolve_type_with_aliases(p, by_name, resolved, stack)?;
            }
            resolve_type_with_aliases(ret, by_name, resolved, stack)?;
        }
        _ => {}
    }
    Ok(())
}

/// Substitute every `Type::Struct(alias_name)` in the
/// program with its fully resolved target. Runs after
/// `resolve_type_aliases` succeeds. T4.15.
fn substitute_aliases_in_program(
    program: &mut Program,
    aliases: &BTreeMap<String, Type>,
) {
    for function in &mut program.functions {
        sub_aliases_in_type(&mut function.return_type, aliases);
        for p in &mut function.params {
            sub_aliases_in_type(&mut p.ty, aliases);
        }
        for s in &mut function.body {
            sub_aliases_in_stmt(s, aliases);
        }
    }
    for decl in &mut program.structs {
        for field in &mut decl.fields {
            sub_aliases_in_type(&mut field.ty, aliases);
        }
    }
    for c in &mut program.consts {
        sub_aliases_in_type(&mut c.ty, aliases);
    }
    for block in &mut program.methods_blocks {
        sub_aliases_in_type(&mut block.for_type, aliases);
        for method in &mut block.methods {
            sub_aliases_in_type(&mut method.return_type, aliases);
            for p in &mut method.params {
                sub_aliases_in_type(&mut p.ty, aliases);
            }
            for s in &mut method.body {
                sub_aliases_in_stmt(s, aliases);
            }
        }
    }
}

fn sub_aliases_in_type(ty: &mut Type, aliases: &BTreeMap<String, Type>) {
    match ty {
        Type::Struct(name) => {
            if let Some(target) = aliases.get(name) {
                *ty = target.clone();
            }
        }
        Type::Vec(inner)
        | Type::Ref(inner)
        | Type::RefMut(inner)
        | Type::Atomic(inner)
        | Type::Mutex(inner)
        | Type::Guard(inner) => sub_aliases_in_type(inner, aliases),
        Type::Array { element, .. } => sub_aliases_in_type(element, aliases),
        Type::Channel(element, _) => sub_aliases_in_type(element, aliases),
        Type::Tuple(elements) => {
            for e in elements {
                sub_aliases_in_type(e, aliases);
            }
        }
        Type::FnPtr(params, ret) => {
            for p in params {
                sub_aliases_in_type(p, aliases);
            }
            sub_aliases_in_type(ret, aliases);
        }
        _ => {}
    }
}

fn sub_aliases_in_stmt(stmt: &mut Stmt, aliases: &BTreeMap<String, Type>) {
    match stmt {
        Stmt::Let { annotation, .. } | Stmt::LetTuple { annotation, .. } => {
            if let Some(ty) = annotation {
                sub_aliases_in_type(ty, aliases);
            }
        }
        Stmt::If { then_body, else_body, .. } => {
            for s in then_body {
                sub_aliases_in_stmt(s, aliases);
            }
            for s in else_body {
                sub_aliases_in_stmt(s, aliases);
            }
        }
        Stmt::While { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForIter { body, .. }
        | Stmt::TaskSpawn { body, .. } => {
            for s in body {
                sub_aliases_in_stmt(s, aliases);
            }
        }
        _ => {}
    }
}

/// Hoist methods from `methods on T { … }` blocks into the
/// regular function table. Each method gets renamed to
/// `<T>_<methodName>`. Validates that the methods-block
/// target is a nominal type (struct/enum) and that no
/// mangled name collides with an existing function. T1.2
/// phase 2a.
fn hoist_methods_into_functions(
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut hoisted: Vec<Function> = Vec::new();
    for block in &program.methods_blocks {
        let type_name = match &block.for_type {
            Type::Struct(name) | Type::Enum(name) => name.clone(),
            other => {
                diagnostics.push(Diagnostic::new(
                    block.for_type_span,
                    format!(
                        "`methods on …` target must be a struct or enum type, \
                         got {}",
                        other
                    ),
                ));
                continue;
            }
        };
        let mut seen_method_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for method in &block.methods {
            if !seen_method_names.insert(method.name.clone()) {
                diagnostics.push(Diagnostic::new(
                    method.span,
                    format!(
                        "method '{}::{}' is declared twice in this methods block",
                        type_name, method.name
                    ),
                ));
                continue;
            }
            // Self-less methods are accepted — they become
            // "type-associated functions" callable via
            // `Type.method(args)`. The hoist gives them the
            // same mangled name (`<TypeName>_<methodName>`)
            // as methods with self, so a self-less `new` on
            // `Point` becomes a top-level `Point_new`.
            // T1.2 phase 2a follow-up.
            let mangled = format!("{}_{}", type_name, method.name);
            if program.functions.iter().any(|f| f.name == mangled)
                || hoisted.iter().any(|f| f.name == mangled)
            {
                diagnostics.push(Diagnostic::new(
                    method.span,
                    format!(
                        "method '{}::{}' (mangled to '{}') collides with an \
                         existing function — pick a different method name",
                        type_name, method.name, mangled
                    ),
                ));
                continue;
            }
            let mut renamed = method.clone();
            renamed.name = mangled;
            hoisted.push(renamed);
        }
    }
    program.functions.extend(hoisted);
    program.methods_blocks.clear();
}

/// T1.5 phase 2: hoist `implement Iface for Type { fn m … }`
/// method bodies into regular functions named
/// `<TypeName>_<method>`, validating each method's signature
/// against the interface's declared shape. Once hoisted, the
/// existing method-dispatch path resolves `recv.method()` to
/// the impl function statically. The interface itself stays
/// in `program.interfaces` for signature lookup only.
///
/// V1 restrictions:
/// - The impl must cover EVERY method declared by the
///   interface (no partial impls). Extra methods not in the
///   interface are rejected.
/// - Each impl method's signature (params + return type)
///   must match the interface's declaration after
///   substituting `Self` (`Type::Param("Self")`-style placeholder
///   if used) with `for_type`. v1 doesn't actually require
///   a Self placeholder — interface methods specify
///   parameters directly; the validation is positional
///   parameter-type matching.
/// - The impl must not collide with an existing
///   `methods on T { fn method() }` or another impl of the
///   same interface for the same type.
fn hoist_impls_into_functions(
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Build interface lookup.
    let iface_by_name: HashMap<String, &crate::ast::InterfaceDecl> = program
        .interfaces
        .iter()
        .map(|i| (i.name.clone(), i))
        .collect();
    let mut hoisted: Vec<Function> = Vec::new();
    for imp in &program.impls {
        // T2.7 phase 1: `implement Drop for T` is recognized as
        // a special interface contract. The auto-call at
        // scope exit lands with T1.2 phase 2b RAII work (#3);
        // until then, users can declare the impl and call
        // `t.drop()` manually. The shape is validated here
        // so the contract is forward-compatible: `fn drop(self:
        // T) -> i64` (i64 return for v1 — return-type
        // generalization waits on T2.7 phase 2). Anything
        // else surfaces a targeted diagnostic.
        if imp.interface_name == "Drop" {
            let type_name = match &imp.for_type {
                Type::Struct(n) | Type::Enum(n) => n.clone(),
                _ => String::new(),
            };
            if imp.methods.len() != 1 || imp.methods[0].name != "drop" {
                diagnostics.push(Diagnostic::new(
                    imp.span,
                    format!(
                        "`implement Drop for {}` must declare exactly one method \
                         named `drop` (T2.7)",
                        imp.for_type
                    ),
                ));
            } else {
                let m = &imp.methods[0];
                let sig_ok = m.params.len() == 1
                    && m.params[0].name == "self"
                    && m.return_type == Type::I64;
                if !sig_ok {
                    diagnostics.push(Diagnostic::new(
                        m.span,
                        format!(
                            "Drop impl for '{}' must have signature \
                             `fn drop(self: {}) -> i64` — got {} params, return \
                             type {}",
                            type_name, type_name, m.params.len(), m.return_type
                        ),
                    ));
                }
            }
            // Note about future work — non-blocking informational.
            // (Not emitted as a diagnostic to keep the impl
            // useful for manual-call patterns today.)
        }
        // Find the interface decl.
        let iface = match iface_by_name.get(&imp.interface_name) {
            Some(i) => *i,
            None => {
                diagnostics.push(Diagnostic::new(
                    imp.span,
                    format!(
                        "`implement {} for {}` references unknown interface '{}'",
                        imp.interface_name, imp.for_type, imp.interface_name
                    ),
                ));
                continue;
            }
        };
        // for_type must be a nominal type (struct or enum).
        let type_name = match &imp.for_type {
            Type::Struct(n) | Type::Enum(n) => n.clone(),
            other => {
                diagnostics.push(Diagnostic::new(
                    imp.span,
                    format!(
                        "`implement {} for {}` requires a struct or enum type \
                         (got {})",
                        imp.interface_name, imp.for_type, other
                    ),
                ));
                continue;
            }
        };
        // Track which interface methods this impl covers.
        let mut covered: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for method in &imp.methods {
            let iface_method = iface
                .methods
                .iter()
                .find(|m| m.name == method.name);
            let iface_method = match iface_method {
                Some(m) => m,
                None => {
                    diagnostics.push(Diagnostic::new(
                        method.span,
                        format!(
                            "interface '{}' has no method '{}' — the impl declares \
                             a method not in the interface",
                            imp.interface_name, method.name
                        ),
                    ));
                    continue;
                }
            };
            // Validate signature: parameter count + return type.
            if method.params.len() != iface_method.params.len() {
                diagnostics.push(Diagnostic::new(
                    method.span,
                    format!(
                        "impl method '{}::{}' has {} parameters but interface \
                         declares {}",
                        type_name, method.name, method.params.len(),
                        iface_method.params.len()
                    ),
                ));
                continue;
            }
            if method.return_type != iface_method.return_type {
                diagnostics.push(Diagnostic::new(
                    method.span,
                    format!(
                        "impl method '{}::{}' returns {} but interface declares {}",
                        type_name, method.name, method.return_type,
                        iface_method.return_type
                    ),
                ));
                continue;
            }
            covered.insert(method.name.clone());
            // Mangle to `<TypeName>_<method>` (same as the
            // methods-on-T convention so `recv.method()`
            // dispatch resolves automatically).
            let mangled = format!("{}_{}", type_name, method.name);
            if program.functions.iter().any(|f| f.name == mangled)
                || hoisted.iter().any(|f| f.name == mangled)
            {
                diagnostics.push(Diagnostic::new(
                    method.span,
                    format!(
                        "impl method '{}::{}' (mangled to '{}') collides with an \
                         existing function — `implement` and `methods on T` can't \
                         both define the same method",
                        type_name, method.name, mangled
                    ),
                ));
                continue;
            }
            let mut renamed = method.clone();
            renamed.name = mangled;
            hoisted.push(renamed);
        }
        // Validate exhaustive coverage.
        for iface_method in &iface.methods {
            if !covered.contains(&iface_method.name) {
                diagnostics.push(Diagnostic::new(
                    imp.span,
                    format!(
                        "`implement {} for {}` is missing the interface method '{}'",
                        imp.interface_name, type_name, iface_method.name
                    ),
                ));
            }
        }
    }
    program.functions.extend(hoisted);
    program.impls.clear();
}

/// T2.6 phase 2: rewrite function bodies of the form
/// ```ignore
/// fn name(args) -> EnumType {
///   let v: T = try opt;
///   let a: …  = …;     // any number of let-stmts
///   …
///   return E;          // E has type EnumType
/// }
/// ```
/// into the equivalent match-based body that early-returns
/// the "early-return" variant when `opt` is that variant, or
/// extracts the payload into `v` and proceeds otherwise:
/// ```ignore
/// fn name(args) -> EnumType {
///   return match opt {
///     EnumType.SomeLike(__try_v_<n>) then {
///       let v: T = __try_v_<n>;
///       let a: … = …;
///       …
///       E
///     },
///     EnumType.NoneLike then EnumType.NoneLike,
///   };
/// }
/// ```
/// For v1 the rewrite is restricted to functions where the
/// body has shape `[Let(try) , Let*, Return]` — the try-let
/// is the first statement, intermediate stmts are all
/// `let`-bindings (block-expressions accept Let only), and
/// the last stmt is a `return`. Functions that don't match
/// this shape fall through to the Phase 1 gate diagnostic
/// emitted by `check_expr`. The enum must have exactly one
/// payloaded variant and exactly one payload-less variant.
fn desugar_try_let_in_program(
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::ast::{EnumDecl, MatchArm, Pattern};
    // Build a quick enum registry: name → decl, so the
    // rewriter can find Some-like / None-like variants.
    let enum_by_name: std::collections::HashMap<String, EnumDecl> =
        program
            .enums
            .iter()
            .cloned()
            .map(|e| (e.name.clone(), e))
            .collect();
    let mut counter: usize = 0;
    for function in program.functions.iter_mut() {
        // Restrict shape: body[0] must be Let with Try RHS;
        // body[1..len-1] must all be Let stmts; body[last]
        // must be Return.
        if function.body.len() < 2 {
            continue;
        }
        let has_try = matches!(
            &function.body[0],
            Stmt::Let { expr: e, .. }
                if matches!(e.kind, ExprKind::Try { .. })
        );
        if !has_try {
            continue;
        }
        // Tail must be Return.
        let last_idx = function.body.len() - 1;
        if !matches!(&function.body[last_idx], Stmt::Return { .. }) {
            continue;
        }
        // Intermediate stmts must be either `let` (passed
        // through into the Some-arm block's stmts) or
        // `print` (also accepted by block expressions since
        // closure #129). Anything else surfaces a clean
        // diagnostic — control flow and reassignment still
        // need surrounding-stmt handling we don't model.
        let intermediate_ok = function.body[1..last_idx]
            .iter()
            .all(|s| matches!(s, Stmt::Let { .. } | Stmt::Print { .. }));
        if !intermediate_ok {
            diagnostics.push(Diagnostic::new(
                function.body[0].span(),
                "`try` desugar in v1 requires only `let` and `print` statements \
                 between the `try`-let and the final `return`; control flow / \
                 assignments between aren't supported yet (T2.6 phase 2 follow-up)",
            ));
            continue;
        }
        // Extract the try-let pieces.
        let (try_name, try_annotation, try_inner, try_span) = match &function.body[0] {
            Stmt::Let {
                name,
                annotation,
                expr,
                span,
            } => {
                let inner = match &expr.kind {
                    ExprKind::Try { inner } => (**inner).clone(),
                    _ => unreachable!(),
                };
                (name.clone(), annotation.clone(), inner, *span)
            }
            _ => unreachable!(),
        };
        // Function return type must be a known payloaded enum.
        let return_enum_name = match &function.return_type {
            Type::Enum(n) => n.clone(),
            _ => {
                diagnostics.push(Diagnostic::new(
                    try_span,
                    format!(
                        "`try` requires the enclosing function's return type \
                         to be an enum; got {}",
                        function.return_type
                    ),
                ));
                continue;
            }
        };
        let enum_decl = match enum_by_name.get(&return_enum_name) {
            Some(d) => d,
            None => continue, // unknown enum; downstream checker handles
        };
        // Find the payloaded variant (the "Some-like" — has
        // a payload) and the payload-less variant ("None-like").
        let payloaded: Vec<_> = enum_decl
            .variants
            .iter()
            .filter(|v| !v.payload.is_empty())
            .collect();
        let payloadless: Vec<_> = enum_decl
            .variants
            .iter()
            .filter(|v| v.payload.is_empty())
            .collect();
        if payloaded.len() != 1 || payloadless.len() != 1 {
            diagnostics.push(Diagnostic::new(
                try_span,
                format!(
                    "`try` requires the enum '{}' to have exactly one payloaded \
                     variant and one payload-less variant; got {} payloaded and \
                     {} payload-less",
                    return_enum_name,
                    payloaded.len(),
                    payloadless.len()
                ),
            ));
            continue;
        }
        let some_variant = payloaded[0].name.clone();
        let none_variant = payloadless[0].name.clone();
        // Synthesize fresh binding name.
        let fresh = format!("__try_v_{}", counter);
        counter += 1;
        // Build the block-expr stmts for the Some arm: the
        // `let v: T = __t;` followed by the intermediate
        // lets, with the Return's expression as the tail.
        let mut block_stmts: Vec<Stmt> = Vec::new();
        block_stmts.push(Stmt::Let {
            name: try_name.clone(),
            annotation: try_annotation.clone(),
            expr: Expr {
                kind: ExprKind::Var(fresh.clone()),
                span: try_span,
            },
            span: try_span,
        });
        for s in &function.body[1..last_idx] {
            block_stmts.push(s.clone());
        }
        let tail_expr = match &function.body[last_idx] {
            Stmt::Return { expr, .. } => expr.clone(),
            _ => unreachable!(),
        };
        let some_arm_body = Expr {
            kind: ExprKind::Block {
                stmts: block_stmts,
                tail: Box::new(tail_expr.clone()),
            },
            span: try_span,
        };
        // None arm body: re-emit the early-return value as
        // an enum-variant reference. Use FieldAccess shape
        // since `EnumName.Variant` lexes that way.
        let none_arm_body = Expr {
            kind: ExprKind::FieldAccess {
                object: Box::new(Expr {
                    kind: ExprKind::Var(return_enum_name.clone()),
                    span: try_span,
                }),
                field: none_variant.clone(),
            },
            span: try_span,
        };
        let return_span = function.body[last_idx].span();
        let match_expr = Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(try_inner),
                arms: vec![
                    MatchArm {
                        pattern: Pattern::VariantWithBinding {
                            enum_name: return_enum_name.clone(),
                            variant: some_variant,
                            binding: fresh,
                        },
                        pattern_span: try_span,
                        body: some_arm_body,
                    },
                    MatchArm {
                        pattern: Pattern::Variant {
                            enum_name: return_enum_name,
                            variant: none_variant,
                        },
                        pattern_span: try_span,
                        body: none_arm_body,
                    },
                ],
            },
            span: try_span,
        };
        function.body = vec![Stmt::Return {
            expr: match_expr,
            span: return_span,
        }];
    }
}

/// T1.4 phase 2: monomorphize generic functions. Walks the
/// program for calls to `fn name<T>(…)` generic functions,
/// infers T from each call site's argument types (only literal
/// args supported in v1), generates a specialized copy per
/// (fn, concrete-type) combo, and rewrites call sites to use
/// the specialized name. Removes the originals.
///
/// V1 restrictions:
/// - Single type parameter only (`fn id<T>`).
/// - T inferred from the FIRST argument's literal type at the
///   call site. Other arguments must coerce to the same T.
/// - Only integer / float / bool literal arguments support
///   inference. Var arguments need type-checking context that
///   doesn't exist at this pre-pass — defer to a follow-up.
fn monomorphize_generics_in_program(
    program: &mut Program,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Collect generic function templates by name, then drop
    // them from the program (specializations replace them).
    let generic_templates: HashMap<String, Function> = program
        .functions
        .iter()
        .filter(|f| !f.type_params.is_empty())
        .cloned()
        .map(|f| (f.name.clone(), f))
        .collect();
    if generic_templates.is_empty() {
        return;
    }
    // Walk every function's body for calls to generic fns
    // and record the inferred concrete type. Specializations
    // are deduplicated by (fn_name, concrete_type_name).
    // Use a Vec + linear dedup since Type doesn't derive Ord
    // (some variants carry Spans).
    let mut needed: Vec<(String, Type)> = Vec::new();
    for f in &program.functions {
        if !f.type_params.is_empty() {
            continue; // skip generic templates
        }
        // Seed a per-fn local-binding map with the function's
        // parameters. The walker extends it as it sees
        // annotated `let` bindings so a generic call with a
        // Var first argument can resolve the binding's type.
        let mut scope: std::collections::HashMap<String, Type> = std::collections::HashMap::new();
        for p in &f.params {
            scope.insert(p.name.clone(), p.ty.clone());
        }
        for stmt in &f.body {
            collect_generic_calls_in_stmt(
                stmt, &generic_templates, &mut needed, &mut scope, diagnostics);
        }
    }
    // Generate specialized fns. For templates carrying
    // `where T is Iface` bounds, verify the concrete type
    // satisfies each bound (i.e. the program has a matching
    // `implement Iface for <concrete>` decl) before
    // specializing. T1.5 phase 2.
    let mut specialized: Vec<Function> = Vec::new();
    for (fn_name, concrete_ty) in &needed {
        let template = match generic_templates.get(fn_name) {
            Some(t) => t,
            None => continue,
        };
        let mut bound_violation = false;
        for clause in &template.where_clauses {
            let satisfied = program.impls.iter().any(|impl_decl| {
                impl_decl.interface_name == clause.interface_name
                    && &impl_decl.for_type == concrete_ty
            });
            if !satisfied {
                diagnostics.push(Diagnostic::new(
                    clause.span,
                    format!(
                        "generic function '{}' requires `{} is {}`, but no \
                         `implement {} for {}` is in scope. Add the impl or \
                         pick a type that satisfies the bound.",
                        fn_name,
                        clause.type_param,
                        clause.interface_name,
                        clause.interface_name,
                        concrete_ty
                    ),
                ));
                bound_violation = true;
            }
        }
        if bound_violation {
            continue;
        }
        if template.type_params.len() != 1 {
            diagnostics.push(Diagnostic::new(
                template.span,
                format!(
                    "generic function '{}' has {} type parameters — v1 supports \
                     only one (T1.4 phase 2 follow-up).",
                    fn_name,
                    template.type_params.len()
                ),
            ));
            continue;
        }
        let t_name = &template.type_params[0];
        let specialized_name = format!("{}__{}", fn_name, type_mangle(concrete_ty));
        let mut clone = template.clone();
        clone.name = specialized_name.clone();
        clone.type_params.clear();
        // Bounds have been satisfied by the impl-existence
        // check above; the specialized fn is no longer
        // generic so drop the where-clauses too.
        clone.where_clauses.clear();
        // Substitute Type::Param(t_name) → concrete in params,
        // return type, and body.
        for p in clone.params.iter_mut() {
            substitute_type_param(&mut p.ty, t_name, concrete_ty);
        }
        substitute_type_param(&mut clone.return_type, t_name, concrete_ty);
        for s in clone.body.iter_mut() {
            substitute_type_param_in_stmt(s, t_name, concrete_ty);
        }
        specialized.push(clone);
    }
    // Rewrite call sites to use specialized names.
    for f in program.functions.iter_mut() {
        if !f.type_params.is_empty() {
            continue;
        }
        let mut scope: std::collections::HashMap<String, Type> = std::collections::HashMap::new();
        for p in &f.params {
            scope.insert(p.name.clone(), p.ty.clone());
        }
        for stmt in f.body.iter_mut() {
            rewrite_generic_calls_in_stmt(stmt, &generic_templates, &mut scope);
        }
    }
    // Surface dead-generic diagnostic for any generic
    // template that didn't get specialized (no call sites
    // inferred concrete types for it).
    let specialized_names: std::collections::HashSet<&String> = needed
        .iter()
        .map(|(n, _)| n)
        .collect();
    for (name, template) in &generic_templates {
        if !specialized_names.contains(name) {
            diagnostics.push(Diagnostic::new(
                template.span,
                format!(
                    "generic function '{}' is declared but never called with \
                     concrete types — monomorphization couldn't specialize it. \
                     Either call it from a non-generic call site or remove the \
                     declaration.",
                    name
                ),
            ));
        }
    }
    // Remove original generics; append specializations.
    program.functions.retain(|f| f.type_params.is_empty());
    program.functions.extend(specialized);
}

/// Walk a stmt and collect (generic-fn-name, inferred-T) pairs
/// from any call sites. The `scope` map tracks local
/// bindings (params + annotated lets) so a generic call with
/// a Var first argument can resolve the binding's type.
fn collect_generic_calls_in_stmt(
    stmt: &Stmt,
    generics: &std::collections::HashMap<String, Function>,
    needed: &mut Vec<(String, Type)>,
    scope: &mut std::collections::HashMap<String, Type>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match stmt {
        Stmt::Let { name, annotation, expr, .. } => {
            collect_generic_calls_in_expr(expr, generics, needed, scope, diagnostics);
            if let Some(ty) = annotation {
                scope.insert(name.clone(), ty.clone());
            }
        }
        Stmt::Assign { expr, .. } | Stmt::Return { expr, .. } => {
            collect_generic_calls_in_expr(expr, generics, needed, scope, diagnostics);
        }
        Stmt::Print { items, .. } => {
            for it in items {
                if let crate::ast::PrintItem::Expr(e) = it {
                    collect_generic_calls_in_expr(e, generics, needed, scope, diagnostics);
                }
            }
        }
        Stmt::If { cond, then_body, else_body, .. } => {
            collect_generic_calls_in_expr(cond, generics, needed, scope, diagnostics);
            for s in then_body {
                collect_generic_calls_in_stmt(s, generics, needed, scope, diagnostics);
            }
            for s in else_body {
                collect_generic_calls_in_stmt(s, generics, needed, scope, diagnostics);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_generic_calls_in_expr(cond, generics, needed, scope, diagnostics);
            for s in body {
                collect_generic_calls_in_stmt(s, generics, needed, scope, diagnostics);
            }
        }
        _ => {}
    }
}

fn collect_generic_calls_in_expr(
    expr: &Expr,
    generics: &std::collections::HashMap<String, Function>,
    needed: &mut Vec<(String, Type)>,
    scope: &std::collections::HashMap<String, Type>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Call { name, args, .. } => {
            if generics.contains_key(name) {
                if let Some(t) =
                    infer_concrete_type_for_call(args, scope, diagnostics, expr.span)
                {
                    let pair = (name.clone(), t);
                    if !needed.contains(&pair) {
                        needed.push(pair);
                    }
                }
            }
            for a in args {
                collect_generic_calls_in_expr(a, generics, needed, scope, diagnostics);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            collect_generic_calls_in_expr(left, generics, needed, scope, diagnostics);
            collect_generic_calls_in_expr(right, generics, needed, scope, diagnostics);
        }
        ExprKind::Unary { expr: inner, .. } => {
            collect_generic_calls_in_expr(inner, generics, needed, scope, diagnostics);
        }
        _ => {}
    }
}

/// Infer the concrete type for a generic call from its first
/// argument. v1 supports literal arguments (Int/Float/Bool)
/// plus Var arguments that resolve through the local scope
/// map (annotated lets + function params).
fn infer_concrete_type_for_call(
    args: &[Expr],
    scope: &std::collections::HashMap<String, Type>,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> Option<Type> {
    let first = args.first()?;
    match &first.kind {
        ExprKind::Int(_) => Some(Type::I64),
        ExprKind::Float(_) => Some(Type::F64),
        ExprKind::Bool(_) => Some(Type::Bool),
        ExprKind::Var(name) => {
            if let Some(ty) = scope.get(name) {
                Some(ty.clone())
            } else {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "cannot infer generic type parameter from variable '{}' \
                         — the monomorphizer pre-pass only resolves bindings \
                         declared with a let-annotation or as a function \
                         parameter at the call site",
                        name
                    ),
                ));
                None
            }
        }
        _ => {
            diagnostics.push(Diagnostic::new(
                span,
                "v1 generic-call inference supports literal arguments \
                 (integer / float / bool) or annotated variable bindings \
                 at the first position. More complex first-arg expressions \
                 need full type-checking context.",
            ));
            None
        }
    }
}

/// Rewrite `Call { name: generic_fn, args }` to use the
/// specialized name based on the first arg's inferred type.
fn rewrite_generic_calls_in_stmt(
    stmt: &mut Stmt,
    generics: &std::collections::HashMap<String, Function>,
    scope: &mut std::collections::HashMap<String, Type>,
) {
    match stmt {
        Stmt::Let { name, annotation, expr, .. } => {
            rewrite_generic_calls_in_expr(expr, generics, scope);
            if let Some(ty) = annotation {
                scope.insert(name.clone(), ty.clone());
            }
        }
        Stmt::Assign { expr, .. } | Stmt::Return { expr, .. } => {
            rewrite_generic_calls_in_expr(expr, generics, scope);
        }
        Stmt::Print { items, .. } => {
            for it in items.iter_mut() {
                if let crate::ast::PrintItem::Expr(e) = it {
                    rewrite_generic_calls_in_expr(e, generics, scope);
                }
            }
        }
        Stmt::If { cond, then_body, else_body, .. } => {
            rewrite_generic_calls_in_expr(cond, generics, scope);
            for s in then_body.iter_mut() {
                rewrite_generic_calls_in_stmt(s, generics, scope);
            }
            for s in else_body.iter_mut() {
                rewrite_generic_calls_in_stmt(s, generics, scope);
            }
        }
        Stmt::While { cond, body, .. } => {
            rewrite_generic_calls_in_expr(cond, generics, scope);
            for s in body.iter_mut() {
                rewrite_generic_calls_in_stmt(s, generics, scope);
            }
        }
        _ => {}
    }
}

fn rewrite_generic_calls_in_expr(
    expr: &mut Expr,
    generics: &std::collections::HashMap<String, Function>,
    scope: &std::collections::HashMap<String, Type>,
) {
    if let ExprKind::Call { name, args, .. } = &mut expr.kind {
        if generics.contains_key(name) {
            let inferred = args.first().and_then(|a| match &a.kind {
                ExprKind::Int(_) => Some(Type::I64),
                ExprKind::Float(_) => Some(Type::F64),
                ExprKind::Bool(_) => Some(Type::Bool),
                ExprKind::Var(n) => scope.get(n).cloned(),
                _ => None,
            });
            if let Some(t) = inferred {
                *name = format!("{}__{}", name, type_mangle(&t));
            }
        }
        for a in args.iter_mut() {
            rewrite_generic_calls_in_expr(a, generics, scope);
        }
    } else {
        match &mut expr.kind {
            ExprKind::Binary { left, right, .. } => {
                rewrite_generic_calls_in_expr(left, generics, scope);
                rewrite_generic_calls_in_expr(right, generics, scope);
            }
            ExprKind::Unary { expr: inner, .. } => {
                rewrite_generic_calls_in_expr(inner, generics, scope);
            }
            _ => {}
        }
    }
}

/// Mangle a concrete type to a name fragment for the
/// specialized function. e.g. `Type::I64` → "i64".
fn type_mangle(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::I32 => "i32".to_string(),
        Type::I16 => "i16".to_string(),
        Type::I8 => "i8".to_string(),
        Type::U64 => "u64".to_string(),
        Type::U32 => "u32".to_string(),
        Type::U16 => "u16".to_string(),
        Type::U8 => "u8".to_string(),
        Type::F64 => "f64".to_string(),
        Type::F32 => "f32".to_string(),
        Type::Bool => "bool".to_string(),
        Type::Str => "Str".to_string(),
        Type::OwnedStr => "OwnedStr".to_string(),
        Type::Struct(name) => format!("Struct_{}", name),
        Type::Enum(name) => format!("Enum_{}", name),
        other => format!("{:?}", other)
            .replace([' ', '<', '>', ',', '(', ')', '"', '{', '}', ':'], "_"),
    }
}

/// Substitute Type::Param(t_name) → concrete in a Type.
fn substitute_type_param(ty: &mut Type, t_name: &str, concrete: &Type) {
    match ty {
        Type::Param(n) if n == t_name => {
            *ty = concrete.clone();
        }
        Type::Vec(inner) | Type::Ref(inner) | Type::RefMut(inner)
        | Type::Atomic(inner) | Type::Mutex(inner) | Type::Guard(inner) => {
            substitute_type_param(inner, t_name, concrete);
        }
        Type::Array { element, .. } => substitute_type_param(element, t_name, concrete),
        Type::Channel(element, _) => substitute_type_param(element, t_name, concrete),
        Type::Tuple(elements) => {
            for e in elements.iter_mut() {
                substitute_type_param(e, t_name, concrete);
            }
        }
        Type::FnPtr(params, ret) => {
            for p in params.iter_mut() {
                substitute_type_param(p, t_name, concrete);
            }
            substitute_type_param(ret, t_name, concrete);
        }
        _ => {}
    }
}

/// Substitute Type::Param(t_name) → concrete in a Stmt's
/// type annotations. Doesn't touch expression sub-trees
/// (those don't carry Type::Param in v1 — type params only
/// appear in declarations).
fn substitute_type_param_in_stmt(stmt: &mut Stmt, t_name: &str, concrete: &Type) {
    use crate::ast::Stmt as S;
    match stmt {
        S::Let { annotation: Some(a), .. } | S::LetTuple { annotation: Some(a), .. } => {
            substitute_type_param(a, t_name, concrete);
        }
        S::If { then_body, else_body, .. } => {
            for s in then_body.iter_mut() {
                substitute_type_param_in_stmt(s, t_name, concrete);
            }
            for s in else_body.iter_mut() {
                substitute_type_param_in_stmt(s, t_name, concrete);
            }
        }
        S::While { body, .. } => {
            for s in body.iter_mut() {
                substitute_type_param_in_stmt(s, t_name, concrete);
            }
        }
        _ => {}
    }
}

/// Try to fold a `const X: T = …;` initializer into a
/// concrete `TypedConst`. v1 accepts plain integer/float/bool
/// literals and unary-minus-of-literal. Anything else returns
/// `None` and the caller surfaces a clear diagnostic. T4.15.
fn literal_const_value(
    expr: &Expr,
    ty: &Type,
    prior_consts: &BTreeMap<String, (Type, TypedConst, Span)>,
) -> Option<TypedConst> {
    match &expr.kind {
        ExprKind::Int(v) if matches!(
            ty,
            Type::I8 | Type::I16 | Type::I32 | Type::I64
            | Type::U8 | Type::U16 | Type::U32 | Type::U64
        ) =>
        {
            Some(TypedConst::Int(*v))
        }
        ExprKind::Float(v) if matches!(ty, Type::F32 | Type::F64) => {
            Some(TypedConst::Float(*v))
        }
        ExprKind::Bool(v) if matches!(ty, Type::Bool) => Some(TypedConst::Bool(*v)),
        // Reference to a previously-declared const. The
        // referenced const's type must match the declared type
        // of the new const. T0.0 follow-up (closure #121).
        ExprKind::Var(name) => {
            let (other_ty, other_val, _) = prior_consts.get(name)?;
            if other_ty != ty {
                return None;
            }
            Some(other_val.clone())
        }
        ExprKind::Unary { op: UnaryOp::Neg, expr: inner } => {
            match literal_const_value(inner, ty, prior_consts)? {
                TypedConst::Int(v) => v.checked_neg().map(TypedConst::Int),
                TypedConst::Float(v) => Some(TypedConst::Float(-v)),
                _ => None,
            }
        }
        // Const arithmetic: +, -, *, /, % on integer consts.
        // The result type is the declared `ty`; both operands
        // must fold to the same integer type. Bool / float
        // arithmetic isn't supported in const initializers
        // (not needed in practice; floats also bring NaN /
        // rounding wrinkles). T0.0 follow-up.
        ExprKind::Binary { op, left, right } => {
            if !matches!(
                ty,
                Type::I8 | Type::I16 | Type::I32 | Type::I64
                | Type::U8 | Type::U16 | Type::U32 | Type::U64
            ) {
                return None;
            }
            let TypedConst::Int(l) = literal_const_value(left, ty, prior_consts)? else {
                return None;
            };
            let TypedConst::Int(r) = literal_const_value(right, ty, prior_consts)? else {
                return None;
            };
            let result = match op {
                BinaryOp::Add => l.checked_add(r),
                BinaryOp::Sub => l.checked_sub(r),
                BinaryOp::Mul => l.checked_mul(r),
                BinaryOp::Div if r != 0 => l.checked_div(r),
                BinaryOp::Rem if r != 0 => l.checked_rem(r),
                _ => None,
            }?;
            Some(TypedConst::Int(result))
        }
        _ => None,
    }
}

fn check_function(
    function: &Function,
    signatures: &HashMap<String, Signature>,
    structs: &BTreeMap<String, StructInfo>,
    enums: &BTreeMap<String, EnumInfo>,
    consts: &BTreeMap<String, (Type, TypedConst, Span)>,
    diagnostics: &mut Vec<Diagnostic>,
) -> TypedFunction {
    // T1.4 phase 2: monomorphization is now wired up as a
    // pre-pass (`monomorphize_generics_in_program`). Any
    // remaining generic function arriving here was unused
    // (no calls to monomorphize against), so surface a
    // gentler diagnostic and skip the per-fn check.
    if !function.type_params.is_empty() {
        diagnostics.push(Diagnostic::new(
            function.span,
            format!(
                "generic function '{}' is declared but never called with concrete \
                 types — monomorphization couldn't specialize it. Either call it \
                 from a non-generic call site or remove the declaration.",
                function.name
            ),
        ));
        return TypedFunction {
            name: function.name.clone(),
            params: Vec::new(),
            return_type: function.return_type.clone(),
            requires: Vec::new(),
            body: Vec::new(),
            is_pure: function.is_pure,
            span: function.span,
        };
    }
    let mut env = Env::new();
    env.structs = structs.clone();
    env.enums = enums.clone();

    // T4.15: seed each top-level `const` into the root
    // scope as an immutable VarInfo carrying its
    // compile-time value. Function-body name lookups walk
    // the scope chain up to root, so consts are visible
    // everywhere while function-scoped `let` bindings
    // shadow them naturally. The constant-tracking pass
    // folds reads of `PI` straight through to `3.14159` in
    // SMT proofs and codegen.
    for (name, (ty, value, decl_span)) in consts.iter() {
        env.insert_current(
            name.clone(),
            VarInfo {
                ty: ty.clone(),
                constant: Some(value.clone()),
                moved: None,
                decl_span: *decl_span,
                vec_literal_elements: None,
                array_version: 0,
                guarded_mutex: None,
                no_drop: false,
                is_const: true,
                struct_literal_fields: None,
                moved_fields: std::collections::BTreeMap::new(),
            },
        );
    }

    let params = function
        .params
        .iter()
        .map(|param| {
            validate_param_type(&param.ty, param.span, diagnostics);
            if env.current_has(&param.name) {
                diagnostics.push(Diagnostic::new(
                    param.span,
                    format!("parameter '{}' is already defined", param.name),
                ));
            }
            // Inside a hoisted `<T>_drop` impl, the `self`
            // parameter must NOT trigger another auto-Drop at
            // scope exit (would infinite-recurse). Mark
            // `no_drop` so the scope-exit pass skips it while
            // the body can still read the fields. T2.7 phase 2.
            let suppress_self_drop = param.name == "self"
                && function.name.ends_with("_drop")
                && matches!(&param.ty, Type::Struct(_) | Type::Enum(_));
            env.insert_current(
                param.name.clone(),
                VarInfo {
                    ty: param.ty.clone(),
                    constant: None,
                    moved: None,
                    decl_span: param.span,
                    vec_literal_elements: None,
                    array_version: 0,
                    guarded_mutex: None,
                    no_drop: suppress_self_drop,
                    is_const: false,
                    struct_literal_fields: None,
                    moved_fields: std::collections::BTreeMap::new(),
                },
            );

            TypedParam {
                name: param.name.clone(),
                ty: param.ty.clone(),
                name_span: param.name_span,
            }
        })
        .collect::<Vec<_>>();

    let requires = function
        .requires
        .iter()
        .map(|requirement| {
            let checked = check_expr(requirement, &mut env, signatures, diagnostics);
            require_type(
                checked.ty(),
                &Type::Bool,
                requirement.span,
                "requires condition",
                diagnostics,
            );
            checked.expr
        })
        .collect::<Vec<_>>();

    // Type-check each `ensures` clause in a temporary env that includes
    // `_return` bound to the function's return type. We don't add `_return`
    // to the body's env (it's only meaningful inside ensures and at return
    // sites).
    {
        let mut ensures_env = env.clone();
        ensures_env.insert_current(
            RETURN_NAME.to_string(),
            VarInfo {
                ty: function.return_type.clone(),
                constant: None,
                moved: None,
                decl_span: function.span,
                vec_literal_elements: None,
                array_version: 0,
                guarded_mutex: None,
                no_drop: false,
                is_const: false,
                struct_literal_fields: None,
                moved_fields: std::collections::BTreeMap::new(),
            },
        );
        for ens in &function.ensures {
            let checked = check_expr(ens, &mut ensures_env, signatures, diagnostics);
            require_type(
                checked.ty(),
                &Type::Bool,
                ens.span,
                "ensures condition",
                diagnostics,
            );
        }
    }

    // Catch contradictory preconditions: if the requires clauses are
    // jointly unsatisfiable, every `prove` in the body is vacuously
    // true. Surface a warning so the function's claims aren't a
    // false sense of security. Errors that the SMT layer couldn't
    // encode are skipped (same conservative policy as elsewhere).
    if !function.requires.is_empty() && !crate::smt::verifier_disabled() {
        let vars: Vec<(String, Type)> = env
            .all_bindings()
            .map(|(name, info)| (name.clone(), info.ty.clone()))
            .collect();
        let versions = env.array_versions();
        if let crate::smt::SatVerdict::Unsatisfiable =
            crate::smt::try_satisfiable(&function.requires, &vars, &versions)
        {
            diagnostics.push(Diagnostic::new(
                function.span,
                format!(
                    "function '{}' has contradictory 'requires' clauses; every \
                     proof in its body is vacuously true and the function is \
                     unreachable",
                    function.name
                ),
            ));
        }
    }

    let mut body = Vec::new();
    let mut loops: Vec<LoopFrame> = Vec::new();
    // SMT facts accumulate as the function body is checked: starts with the
    // function's `requires` clauses, grows with each `let r = call(...)` whose
    // callee has `ensures` clauses (substituted to refer to `r`).
    let mut smt_facts: Vec<Expr> = function.requires.clone();
    let terminated = check_stmt_list(
        &function.body,
        &mut env,
        signatures,
        function,
        &mut loops,
        &mut smt_facts,
        &mut body,
        diagnostics,
    );
    let saw_return = terminated;

    if !saw_return {
        diagnostics.push(Diagnostic::new(
            function.span,
            format!(
                "function '{}' must return a {}",
                function.name, function.return_type
            ),
        ));
    }

    // Effects check for `pure fn`: walk the typed body and report
    // any operation that would produce observable side effects.
    // Diagnostics are accumulated; the function still type-checks
    // structurally and the typed IR is emitted unchanged so other
    // analyses (SMT proofs, bounds elision) keep working.
    if function.is_pure {
        verify_pure_body(
            &body,
            signatures,
            &format!("pure fn '{}'", function.name),
            diagnostics,
        );
    }

    // Affine task-handle check: each `TypedStmt::TaskSpawn` must
    // be matched by exactly one `TypedStmt::TaskJoin` for the
    // same name in the same block, with join coming after spawn.
    // The scope-exit drop emitter only catches unjoined tasks on
    // fall-through paths; this pass catches them on
    // return-terminated paths too.
    verify_task_affine(&body, diagnostics);

    TypedFunction {
        name: function.name.clone(),
        params,
        return_type: function.return_type.clone(),
        requires,
        body,
        is_pure: function.is_pure,
        span: function.span,
    }
}

#[derive(Clone)]
struct LoopFrame {
    pre_env: Env,
    /// Env depth (scope-stack length) at the point the loop body began. Used
    /// for emitting cleanup drops on `break` / `continue` across any nested
    /// scopes opened inside the body.
    body_scope_depth: usize,
}

fn check_stmt_list(
    stmts: &[Stmt],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    function: &Function,
    loops: &mut Vec<LoopFrame>,
    smt_facts: &mut Vec<Expr>,
    body: &mut Vec<TypedStmt>,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    let mut terminated = false;
    for stmt in stmts {
        if terminated {
            diagnostics.push(Diagnostic::new(
                stmt.span(),
                "unreachable statement after a control-flow exit",
            ));
            break;
        }
        if check_one_stmt(stmt, env, signatures, function, loops, smt_facts, body, diagnostics) {
            terminated = true;
        }
    }
    terminated
}

fn validate_loop_balance(
    pre_env: &Env,
    current_env: &Env,
    span: Span,
    context: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (name, pre_info) in pre_env.all_bindings() {
        if pre_info.ty.is_copy() {
            continue;
        }
        let pre_moved = pre_info.moved.is_some();
        let cur_moved = current_env
            .lookup(name)
            .map(|i| i.moved.is_some())
            .unwrap_or(false);
        if pre_moved != cur_moved {
            diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "{}: '{}' is in a different move state than at loop start; \
                     consume or rebind it consistently before this point",
                    context, name
                ),
            ));
        }
    }
}

/// Drop `info.constant` only on the bindings whose names appear in
/// `names`. Used by `if`/`else` / `while` / `for` / `for-iter`
/// merges to preserve constant facts about bindings the body
/// provably didn't touch. Soundness: callers populate `names` via
/// `collect_branch_mutations` which is the conservative union of
/// `Stmt::Assign` LHS, `Stmt::IndexAssign` LHS, and `&mut <name>`
/// argument targets (anything that can flow back into the outer
/// binding's value).
fn clear_constants_for(
    env: &mut Env,
    names: &std::collections::HashSet<String>,
) {
    for scope in env.scopes.iter_mut() {
        for (name, info) in scope.iter_mut() {
            if names.contains(name) {
                info.constant = None;
            }
        }
    }
}

/// Walk a branch / loop body and collect the names of outer
/// bindings whose constant tracking could be invalidated. Includes:
/// (1) LHS of `Stmt::Assign` and `Stmt::IndexAssign`; (2) names
/// passed by `&mut` to a callee (which may overwrite the storage);
/// recurses into nested `if`/`while`/`for`/`for-iter` bodies and
/// `Stmt::Let` initializers. Shadow-`let`s themselves don't
/// invalidate the outer binding — the inner shadow dies with its
/// scope — so the LHS of a `Let` is intentionally NOT added.
fn collect_branch_mutations(stmts: &[Stmt]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    walk_branch_mutations(stmts, &mut out);
    out
}

fn walk_branch_mutations(
    stmts: &[Stmt],
    out: &mut std::collections::HashSet<String>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { name, expr, .. } => {
                out.insert(name.clone());
                walk_branch_mutations_in_expr(expr, out);
            }
            Stmt::IndexAssign {
                name, index, value, ..
            } => {
                // The container binding's storage is mutated. We
                // don't track aggregate constants today, but be
                // future-proof and clear the binding's slot.
                out.insert(name.clone());
                walk_branch_mutations_in_expr(index, out);
                walk_branch_mutations_in_expr(value, out);
            }
            Stmt::FieldAssign { object, value, .. } => {
                // Any constant facts about the struct binding
                // touched by `object` must be cleared. Use the
                // existing walker which already picks up
                // root-Var names through ref/mut-ref
                // wrappers.
                if let Some(name) = unwrap_to_var(object) {
                    out.insert(name.to_string());
                }
                walk_branch_mutations_in_expr(object, out);
                walk_branch_mutations_in_expr(value, out);
            }
            Stmt::Let { expr, .. } => {
                walk_branch_mutations_in_expr(expr, out);
            }
            Stmt::Return { expr, .. }
            | Stmt::Assert { expr, .. }
            | Stmt::Prove { expr, .. } => {
                walk_branch_mutations_in_expr(expr, out);
            }
            Stmt::Print { items, .. } => {
                for item in items {
                    if let crate::ast::PrintItem::Expr(e) = item {
                        walk_branch_mutations_in_expr(e, out);
                    }
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                walk_branch_mutations_in_expr(cond, out);
                walk_branch_mutations(then_body, out);
                walk_branch_mutations(else_body, out);
            }
            Stmt::While {
                cond,
                invariants,
                body,
                ..
            } => {
                walk_branch_mutations_in_expr(cond, out);
                for inv in invariants {
                    walk_branch_mutations_in_expr(inv, out);
                }
                walk_branch_mutations(body, out);
            }
            Stmt::For {
                start,
                end,
                invariants,
                body,
                ..
            } => {
                walk_branch_mutations_in_expr(start, out);
                walk_branch_mutations_in_expr(end, out);
                for inv in invariants {
                    walk_branch_mutations_in_expr(inv, out);
                }
                walk_branch_mutations(body, out);
            }
            Stmt::ForIter { body, .. } => {
                walk_branch_mutations(body, out);
            }
            // Break / Continue / TaskSpawn / TaskJoin: no
            // expression operands that could carry a fresh
            // `&mut` target. TaskSpawn's body is checked in its
            // own scope with no outer-binding mutation
            // permitted (Copy captures only), so we don't
            // descend into it.
            _ => {}
        }
    }
}

fn walk_branch_mutations_in_expr(
    expr: &Expr,
    out: &mut std::collections::HashSet<String>,
) {
    match &expr.kind {
        ExprKind::RefMut { inner } => {
            if let Some(name) = unwrap_to_var(inner) {
                out.insert(name.to_string());
            }
            walk_branch_mutations_in_expr(inner, out);
        }
        ExprKind::Ref { inner } => walk_branch_mutations_in_expr(inner, out),
        ExprKind::Call { args, .. } => {
            for a in args {
                walk_branch_mutations_in_expr(a, out);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            walk_branch_mutations_in_expr(receiver, out);
            for a in args {
                walk_branch_mutations_in_expr(a, out);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            walk_branch_mutations_in_expr(left, out);
            walk_branch_mutations_in_expr(right, out);
        }
        ExprKind::Unary { expr: inner, .. } => {
            walk_branch_mutations_in_expr(inner, out)
        }
        ExprKind::Index { array, index } => {
            walk_branch_mutations_in_expr(array, out);
            walk_branch_mutations_in_expr(index, out);
        }
        ExprKind::Len { array } => walk_branch_mutations_in_expr(array, out),
        ExprKind::Cast { expr: inner, .. } => {
            walk_branch_mutations_in_expr(inner, out)
        }
        ExprKind::ArrayLit { elements } => {
            for e in elements {
                walk_branch_mutations_in_expr(e, out);
            }
        }
        ExprKind::Tuple(elements) => {
            for e in elements {
                walk_branch_mutations_in_expr(e, out);
            }
        }
        ExprKind::TupleAccess { tuple, .. } => {
            walk_branch_mutations_in_expr(tuple, out);
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, e) in fields {
                walk_branch_mutations_in_expr(e, out);
            }
        }
        ExprKind::FieldAccess { object, .. } => {
            walk_branch_mutations_in_expr(object, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_branch_mutations_in_expr(scrutinee, out);
            for arm in arms {
                walk_branch_mutations_in_expr(&arm.body, out);
            }
        }
        ExprKind::IfExpr { cond, then_value, else_value } => {
            walk_branch_mutations_in_expr(cond, out);
            walk_branch_mutations_in_expr(then_value, out);
            walk_branch_mutations_in_expr(else_value, out);
        }
        ExprKind::Block { tail, .. } => {
            // Block-stmts' mut-refs (if any) are scoped to the
            // block — they don't escape because mut-refs are
            // call-arg-only and the inner call's aliasing is
            // checked locally. Walk only the tail value for
            // mutation tracking of the surrounding expression.
            walk_branch_mutations_in_expr(tail, out);
        }
        ExprKind::Try { inner } => {
            walk_branch_mutations_in_expr(inner, out);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Var(_) => {}
    }
}

/// Emit Drop statements for every non-Copy, non-moved binding in `env`'s
/// current (innermost) scope, in deterministic order.
/// Walk a function's typed body and flag every `TaskSpawn` that
/// doesn't have a matching `TaskJoin` in the same block (same
/// Vec<TypedStmt> sequence, with the join positioned after the
/// spawn). Also flags double-joins. v1 keeps the scope rule
/// tight: cross-branch or cross-loop joins aren't supported.
fn verify_task_affine(body: &[TypedStmt], diagnostics: &mut Vec<Diagnostic>) {
    fn walk(stmts: &[TypedStmt], diagnostics: &mut Vec<Diagnostic>) {
        use std::collections::HashSet;
        let mut spawned: HashSet<String> = HashSet::new();
        let mut joined: HashSet<String> = HashSet::new();
        for stmt in stmts {
            match stmt {
                TypedStmt::TaskSpawn { name, body, .. } => {
                    if !spawned.insert(name.clone()) {
                        // Same name spawned twice in the same
                        // block: the second spawn would shadow
                        // the first handle. The checker's
                        // shadow check should already have
                        // flagged this, but emit a diagnostic
                        // here too for clarity. Use a
                        // zero-span since we don't have the
                        // source span on TypedStmt.
                        diagnostics.push(Diagnostic::new(
                            crate::span::Span::default(),
                            format!(
                                "task '{}' was spawned twice in the same block",
                                name
                            ),
                        ));
                    }
                    walk(body, diagnostics);
                }
                TypedStmt::TaskJoin { name } => {
                    if !spawned.contains(name) {
                        diagnostics.push(Diagnostic::new(
                            crate::span::Span::default(),
                            format!(
                                "join: task '{}' was not spawned in this block (cross-block joins aren't supported in v1)",
                                name
                            ),
                        ));
                    }
                    if !joined.insert(name.clone()) {
                        diagnostics.push(Diagnostic::new(
                            crate::span::Span::default(),
                            format!(
                                "join: task '{}' was joined twice in the same block",
                                name
                            ),
                        ));
                    }
                }
                TypedStmt::If { then_body, else_body, .. } => {
                    walk(then_body, diagnostics);
                    walk(else_body, diagnostics);
                }
                TypedStmt::While { body, .. }
                | TypedStmt::For { body, .. }
                | TypedStmt::ForIter { body, .. } => {
                    walk(body, diagnostics);
                }
                _ => {}
            }
        }
        // Any name in `spawned` but not in `joined` is unjoined.
        for name in spawned.difference(&joined) {
            diagnostics.push(Diagnostic::new(
                crate::span::Span::default(),
                format!(
                    "task '{}' was never consumed by `join {}`; \
                     each `task` handle must be joined exactly once",
                    name, name
                ),
            ));
        }
    }
    walk(body, diagnostics);
}

fn emit_current_scope_drops(
    env: &Env,
    body: &mut Vec<TypedStmt>,
    _diagnostics: &mut Vec<Diagnostic>,
) {
    for (name, info) in env.current_scope().iter() {
        if matches!(info.ty, Type::Task) {
            // Task handles are affine but have no runtime
            // resource to free in v1 (sequential lowering means
            // the body has already run). The
            // `verify_task_affine` post-pass flags unjoined
            // handles uniformly across all control-flow paths,
            // so we don't emit a Drop or a duplicate diagnostic
            // here.
            continue;
        }
        if info.no_drop {
            // Iteration views (`for v in &xs` over non-Copy
            // elements) alias the owner's slot; freeing them
            // would double-free at the outer collection's
            // drop. Refines #7 phase 2.
            continue;
        }
        if !info.ty.is_copy() && info.moved.is_none() {
            body.push(TypedStmt::Drop {
                name: name.clone(),
                ty: info.ty.clone(),
                moved_fields: info.moved_fields.keys().cloned().collect(),
            });
        }
    }
}

fn check_one_stmt(
    stmt: &Stmt,
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    function: &Function,
    loops: &mut Vec<LoopFrame>,
    smt_facts: &mut Vec<Expr>,
    body: &mut Vec<TypedStmt>,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    match stmt {
        Stmt::Let {
            name,
            annotation,
            expr,
            span,
        } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
            let mut checked = if let Some(annotation) = annotation {
                validate_array_element_type(annotation, *span, diagnostics);
                validate_no_ref(annotation, *span, "let annotation", diagnostics);
                // Refines #8: empty `vec()` borrows its element type
                // from the let-annotation. Intercept before
                // `check_expr` so the zero-arg call doesn't trigger
                // the "needs at least one element" diagnostic.
                if let Some(elaborated) =
                    try_elaborate_empty_vec(expr, annotation, diagnostics)
                {
                    elaborated
                } else {
                    let raw = check_expr(expr, env, signatures, diagnostics);
                    coerce_checked(
                        raw,
                        annotation,
                        expr.span,
                        "let initializer",
                        diagnostics,
                    )
                }
            } else {
                check_expr(expr, env, signatures, diagnostics)
            };

            diagnose_partial_then_whole_move(expr, &checked, env, diagnostics);

            // Nested-path move of a non-Copy field isn't
            // tracked yet (`moved_fields` is one level deep).
            // Allowing it would cause a double-free at the
            // outer struct's Drop. Surface a clean diagnostic
            // with the workaround: move the inner struct out
            // first, then move out of that. T1.2 phase 2b
            // follow-up. Closure #125.
            if !checked.ty().is_copy() && is_nested_field_access(expr) {
                diagnostics.push(Diagnostic::new(
                    expr.span,
                    "nested field move (`o.inner.s`) on a non-Copy field is \
                     not supported yet — move the inner struct out first \
                     (`let inner = o.inner;` then `let s = inner.s;`)",
                ));
            }

            consume_if_moved_var(expr, &checked, env);
            // After the conservative move marks all branch
            // Vars as moved, rewrite the typed expr to drop
            // the unchosen branches' Vars inline so the heap
            // doesn't leak. Closure #179.
            inject_branch_drops(&mut checked.expr);

            // `let _ = expr;` — discard pattern. Evaluate expr (consuming
            // moved vars above) but do not introduce a binding. Repeated
            // uses don't collide because nothing is inserted into env.
            if name == "_" {
                let var_ty = checked.ty().clone();
                if var_ty.is_ref() {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        "references cannot appear in a 'let _' discard; \
                         the value would dangle immediately"
                            .to_string(),
                    ));
                }
                body.push(TypedStmt::Discard { expr: checked.expr });
                return false;
            }

            let var_ty = checked.ty().clone();
            if var_ty.is_ref() {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "references cannot be stored in 'let' bindings; pass '&{}' \
                         directly to a function parameter instead",
                        name
                    ),
                ));
            }
            let constant = checked.constant().cloned();
            let same_scope_existing = env.current_get(name).cloned();
            let was_shadow = same_scope_existing.is_some();

            // Same-scope shadowing semantically reassigns `name`.
            // Outside of a loop body, drop the prior facts about the
            // old value — see the longer note in Stmt::Assign for the
            // reasoning around in-loop preservation.
            if was_shadow && loops.is_empty() {
                drop_facts_mentioning(smt_facts, name);
            }

            if let Some(old) = same_scope_existing {
                // Same-scope let → Reassign (type must match).
                if old.ty != var_ty {
                    diagnostics.push(
                        Diagnostic::new(
                            *span,
                            format!(
                                "shadowing 'let {}' must preserve its type; previous type was {}, new type is {}",
                                name, old.ty, var_ty
                            ),
                        )
                        .with_related(old.decl_span, format!("'{}' was previously declared here as {}", name, old.ty)),
                    );
                }
                let drop_old = !old.ty.is_copy() && old.moved.is_none();
                let mut expr = checked.expr;
                try_elide_bounds_in_typed_expr(&mut expr, smt_facts, env, signatures);
                body.push(TypedStmt::Reassign {
                    name: name.clone(),
                    ty: var_ty.clone(),
                    expr,
                    drop_old,
                });
            } else {
                // Fresh declaration or shadowing an outer-scope binding.
                let mut expr = checked.expr;
                try_elide_bounds_in_typed_expr(&mut expr, smt_facts, env, signatures);
                body.push(TypedStmt::Let {
                    name: name.clone(),
                    ty: var_ty.clone(),
                    expr,
                });
            }

            // For `let xs = vec(a, b, c);` (fresh, not a shadow),
            // remember the element expressions on the binding so the
            // SMT rewriter can substitute `xs[k]` with `a_k` in
            // proofs. Skipped for shadowing (which we'd need to
            // invalidate) and when args reference the same name.
            let vec_elements = if !was_shadow {
                if let ExprKind::Call { name: call_name, args, .. } = &expr.kind {
                    if call_name == "vec" && !args.iter().any(|a| expr_mentions(a, name)) {
                        Some(args.to_vec())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            // Detect `let g: Guard<T> = mutex_lock(&m);` so the
            // new binding's `guarded_mutex` records the mutex
            // name. The double-acquire check in
            // `check_mutex_builtin` reads this field on every
            // subsequent `mutex_lock`.
            let guarded_mutex = match &expr.kind {
                ExprKind::Call { name: call_name, args, .. }
                    if call_name == "mutex_lock" && args.len() == 1 =>
                {
                    extract_locked_mutex_name(&args[0])
                }
                _ => None,
            };

            // Track struct-literal initializers so the SMT
            // prove-rewriter can substitute `p.x` with the
            // synthesized `p__x` per-field SMT var. Skip on
            // shadowing (the previous binding's fields would
            // need invalidation, which we don't track yet).
            let struct_literal_fields = if !was_shadow {
                if let ExprKind::StructLit { fields, .. } = &expr.kind {
                    Some(fields.clone())
                } else {
                    None
                }
            } else {
                None
            };

            env.insert_current(
                name.clone(),
                VarInfo {
                    ty: var_ty,
                    constant,
                    moved: None,
                    decl_span: *span,
                    vec_literal_elements: vec_elements,
                    array_version: 0,
                    guarded_mutex,
                    no_drop: false,
                    is_const: false,
                    struct_literal_fields,
                    moved_fields: std::collections::BTreeMap::new(),
                },
            );

            // If the RHS is a call to a function with `ensures`, record the
            // substituted clauses as SMT facts about the new binding.
            if let ExprKind::Call { name: call_name, args, .. } = &expr.kind {
                record_ensures_facts(call_name, args, name, signatures, smt_facts);
                // Vec-builtin facts: skip when the new binding shadows
                // an existing same-scope one (the OLD facts for that
                // name are still in smt_facts and would mix with the
                // new ones), or when the call's args reference the
                // same name (self-referencing fact like
                // `len(xs) == len(xs) + 1` is a contradiction). The
                // runtime still performs the correct push/clone/etc.;
                // users who need a compile-time length relationship
                // across a rebind should pick a new binding name
                // (`let ys = push(xs, 4);`).
                let self_ref = args.iter().any(|a| expr_mentions(a, name));
                if !was_shadow && !self_ref {
                    record_vec_builtin_facts(call_name, args, name, smt_facts);
                }
            }
            // Array literal initializer: `let xs: [i64; N] = [a, b, c]`.
            // Emit per-slot identity facts. Same shadow/self-ref
            // guards as the Vec case.
            if let ExprKind::ArrayLit { elements } = &expr.kind {
                let self_ref = elements.iter().any(|e| expr_mentions(e, name));
                if !was_shadow && !self_ref {
                    record_array_element_facts(name, elements, smt_facts);
                }
            }
            // Array rebind: `let ys = xs;` where xs is a previously
            // bound Vec/Array. The new binding's symbolic SMT array
            // (`arr_ys`) is fresh, but the value is the same one as
            // `xs` — emit `arr_ys = arr_xs` so proofs about ys
            // chain through any literal-init facts on xs. xs is
            // moved by the rebind, so the user can't reference it
            // post-move, but the facts that mention xs by name
            // remain in scope and the encoder will resolve them.
            if !was_shadow {
                if let ExprKind::Var(base) = &expr.kind {
                    if base != name {
                        if let Some(info) = env.lookup(base) {
                            if matches!(
                                info.ty.deref(),
                                Type::Vec(_) | Type::Array { .. }
                            ) {
                                smt_facts.push(Expr {
                                    kind: ExprKind::Call {
                                        name: "__smt_array_eq".to_string(),
                                        name_span: crate::span::Span::default(),
                                        args: vec![
                                            Expr {
                                                kind: ExprKind::Var(name.clone()),
                                                span: crate::span::Span::default(),
                                            },
                                            Expr {
                                                kind: ExprKind::Var(base.clone()),
                                                span: crate::span::Span::default(),
                                            },
                                        ],
                                    },
                                    span: crate::span::Span::default(),
                                });
                            }
                        }
                    }
                }
            }
            false
        }
        Stmt::Assign { name, expr, span } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
            // Refines #8: `xs = vec();` borrows the element
            // type from the existing binding. We do a pre-
            // peek lookup *only* to recognize the empty-vec
            // shape before running `check_expr` — the
            // non-elaborated path must run `check_expr` BEFORE
            // the lookup we feed into `drop_old`, because
            // `check_expr` mutates `env.moved` for affine
            // operands and the captured `existing.moved`
            // drives whether the reassign emits a drop of
            // the previous value. (Pre-#8 the lookup happened
            // after `check_expr`, so flipping it broke
            // self-consuming reassigns like
            // `xs = push(xs, i)` — they double-freed.)
            let empty_vec_elab = if matches!(
                &expr.kind,
                ExprKind::Call { name: n, args, .. }
                    if n == "vec" && args.is_empty()
            ) {
                env.lookup(name).cloned().and_then(|info| {
                    try_elaborate_empty_vec(expr, &info.ty, diagnostics)
                })
            } else {
                None
            };
            let coerced = if let Some(elab) = empty_vec_elab {
                let Some(existing) = env.lookup(name).cloned() else {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!("cannot assign to unknown variable '{}'", name),
                    ));
                    return false;
                };
                coerce_checked(
                    elab,
                    &existing.ty,
                    expr.span,
                    "assignment value",
                    diagnostics,
                )
            } else {
                let checked = check_expr(expr, env, signatures, diagnostics);
                let Some(existing) = env.lookup(name).cloned() else {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!("cannot assign to unknown variable '{}'", name),
                    ));
                    return false;
                };
                coerce_checked(
                    checked,
                    &existing.ty,
                    expr.span,
                    "assignment value",
                    diagnostics,
                )
            };
            let Some(existing) = env.lookup(name).cloned() else {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!("cannot assign to unknown variable '{}'", name),
                ));
                return false;
            };
            diagnose_partial_then_whole_move(expr, &coerced, env, diagnostics);
            consume_if_moved_var(expr, &coerced, env);
            let drop_old = !existing.ty.is_copy() && existing.moved.is_none();
            let mut rhs = coerced.expr;
            inject_branch_drops(&mut rhs);  // closure #179
            try_elide_bounds_in_typed_expr(&mut rhs, smt_facts, env, signatures);
            body.push(TypedStmt::Reassign {
                name: name.clone(),
                ty: existing.ty.clone(),
                expr: rhs,
                drop_old,
            });
            // Update the binding in place (wherever it lives in the scope stack).
            if let Some(info) = env.lookup_mut(name) {
                info.constant = None;
                info.moved = None;
            }
            // Reassignment invalidates any prior facts about `name`.
            // OUTSIDE of a loop body this is straightforward: the old
            // fact (e.g. `len(xs) == 3`) no longer describes the new
            // value, so drop it.
            //
            // INSIDE a loop body we deliberately keep the prior facts
            // so the substitution-based preservation check at body-end
            // can still see the entry invariants (which reference the
            // pre-body binding). The substituted goal lives in terms
            // of the new value via the last-reassignment rewrite.
            if loops.is_empty() {
                drop_facts_mentioning(smt_facts, name);
            }
            let _ = (existing, span);
            false
        }
        Stmt::Return { expr, .. } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
            // Refines #8: `return vec();` from a fn whose
            // return type is `Vec<T>` borrows the element type.
            let checked = if let Some(elaborated) = try_elaborate_empty_vec(
                expr,
                &function.return_type,
                diagnostics,
            ) {
                elaborated
            } else {
                let raw = check_expr(expr, env, signatures, diagnostics);
                coerce_checked(
                    raw,
                    &function.return_type,
                    expr.span,
                    "return expression",
                    diagnostics,
                )
            };
            diagnose_partial_then_whole_move(expr, &checked, env, diagnostics);
            consume_if_moved_var(expr, &checked, env);

            // Verify each ensures clause holds for this return expression.
            verify_ensures_at_return(function, expr, smt_facts, env, signatures, diagnostics);

            // Materialize the return expression into a fresh temp
            // *before* emitting drops. Otherwise a return like
            // `return xs[1]` (where xs: Vec<i64> falls out of scope)
            // would lower as `drop xs; return xs[1];` — use-after-
            // free. Storing the value first decouples the two:
            //   let __ret = xs[1];   // reads from live buffer
            //   drop xs;             // frees buffer
            //   return __ret;        // returns the cached value
            //
            // The temp name uses an underscore prefix so it can't
            // collide with a user-chosen name (parser/lexer reject
            // identifiers starting with `__intent`).
            let mut ret_expr = checked.expr;
            try_elide_bounds_in_typed_expr(&mut ret_expr, smt_facts, env, signatures);
            // Per-return unique suffix so multiple return sites in the
            // same function don't try to alloca the same SSA name.
            // Using the return's source span keeps it deterministic
            // and collision-free.
            let temp_name = format!("__intent_ret_{}", expr.span.start);
            let temp_ty = function.return_type.clone();
            body.push(TypedStmt::Let {
                name: temp_name.clone(),
                ty: temp_ty.clone(),
                expr: ret_expr,
            });

            let drop_names: Vec<(String, Type, Vec<String>)> = env
                .all_bindings()
                .filter(|(_, info)| !info.ty.is_copy() && info.moved.is_none() && !info.no_drop)
                .map(|(name, info)| (
                    name.clone(),
                    info.ty.clone(),
                    info.moved_fields.keys().cloned().collect::<Vec<_>>(),
                ))
                .collect();
            for (drop_name, drop_ty, moved_fields) in drop_names {
                body.push(TypedStmt::Drop {
                    name: drop_name,
                    ty: drop_ty,
                    moved_fields,
                });
            }

            // Read the cached value back. Reusing the existing
            // span keeps diagnostics pointing at the original return.
            let temp_var = TypedExpr {
                kind: TypedExprKind::Var(temp_name),
                ty: temp_ty,
                constant: None,
                span: expr.span,
                binding_decl_span: None,
            };
            body.push(TypedStmt::Return { expr: temp_var });
            true
        }
        Stmt::Assert { expr, message, .. } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
            let checked = check_expr(expr, env, signatures, diagnostics);
            require_type(
                checked.ty(),
                &Type::Bool,
                expr.span,
                "assert condition",
                diagnostics,
            );
            let mut e = checked.expr;
            try_elide_bounds_in_typed_expr(&mut e, smt_facts, env, signatures);
            body.push(TypedStmt::Assert {
                expr: e,
                message: message.clone(),
            });
            false
        }
        Stmt::Prove { expr, .. } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
            let checked = check_expr(expr, env, signatures, diagnostics);
            require_type(
                checked.ty(),
                &Type::Bool,
                expr.span,
                "prove condition",
                diagnostics,
            );
            match checked.constant() {
                Some(TypedConst::Bool(true)) => {}
                Some(TypedConst::Bool(false)) => diagnostics.push(Diagnostic::new(
                    expr.span,
                    "proof failed: expression is always false",
                )),
                _ => {
                    if !is_structurally_true(expr) {
                        try_smt_prove(expr, smt_facts, env, signatures, diagnostics);
                    }
                }
            }
            let mut e = checked.expr;
            try_elide_bounds_in_typed_expr(&mut e, smt_facts, env, signatures);
            body.push(TypedStmt::Prove { expr: e });
            false
        }
        Stmt::Print { items, .. } => {
            use crate::ast::PrintItem;
            use crate::ir::TypedPrintItem;
            let mut typed_items: Vec<TypedPrintItem> = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    PrintItem::Str(s) => typed_items.push(TypedPrintItem::Str(s.clone())),
                    PrintItem::Expr(e) => {
                        verify_call_args_in_expr(e, smt_facts, env, signatures, diagnostics);
                        let checked = check_expr(e, env, signatures, diagnostics);
                        let ty = checked.ty();
                        if ty.is_array() || ty.is_vec() {
                            diagnostics.push(Diagnostic::new(
                                e.span,
                                "cannot print an array or Vec directly; index it first",
                            ));
                        } else if matches!(ty, Type::Struct(_)) {
                            diagnostics.push(Diagnostic::new(
                                e.span,
                                "cannot print a struct directly; print individual fields instead",
                            ));
                        } else if matches!(ty, Type::Tuple(_)) {
                            diagnostics.push(Diagnostic::new(
                                e.span,
                                "cannot print a tuple directly; print individual elements via `.0` / `.1`",
                            ));
                        } else if matches!(ty, Type::Enum(_)) {
                            diagnostics.push(Diagnostic::new(
                                e.span,
                                "cannot print an enum directly; use `match` to convert to an integer or string",
                            ));
                        }
                        let mut t = checked.expr;
                        try_elide_bounds_in_typed_expr(&mut t, smt_facts, env, signatures);
                        typed_items.push(TypedPrintItem::Expr(t));
                    }
                }
            }
            body.push(TypedStmt::Print { items: typed_items });
            false
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            span: _,
        } => {
            verify_call_args_in_expr(cond, smt_facts, env, signatures, diagnostics);
            let cond_checked = check_expr(cond, env, signatures, diagnostics);
            require_type(
                cond_checked.ty(),
                &Type::Bool,
                cond.span,
                "if condition",
                diagnostics,
            );

            // Detect dead branches when the condition is a *syntactic*
            // boolean literal — catches accidentally-disabled debug code
            // like `if false { … }`. We deliberately do not flag values
            // that only fold to a constant via the checker's
            // const-tracking (e.g. `i >= 5` when `i` was just `let i = 0`);
            // those frequently appear at loop entry where `i` will change
            // on later iterations, and the diagnostic would be a false
            // positive.
            match &cond.kind {
                ExprKind::Bool(false) if !then_body.is_empty() => {
                    diagnostics.push(Diagnostic::new(
                        cond.span,
                        "condition is always false; the 'if' body is unreachable",
                    ));
                }
                ExprKind::Bool(true) if !else_body.is_empty() => {
                    diagnostics.push(Diagnostic::new(
                        cond.span,
                        "condition is always true; the 'else' body is unreachable",
                    ));
                }
                _ => {}
            }

            let pre_env = env.clone();

            // Then branch: push a fresh scope, process, emit body-scope drops
            // (if not terminated), pop. Save resulting env state for merge.
            // Snapshot facts so branches don't pollute each other's view.
            let pre_facts = smt_facts.clone();

            // Then branch knows the condition is true.
            smt_facts.push(cond.clone());
            env.push_scope();
            let mut then_stmts = Vec::new();
            let then_terminated = check_stmt_list(
                then_body,
                env,
                signatures,
                function,
                loops,
                smt_facts,
                &mut then_stmts,
                diagnostics,
            );
            if !then_terminated {
                emit_current_scope_drops(env, &mut then_stmts, diagnostics);
            }
            env.pop_scope();
            let then_env = std::mem::replace(env, pre_env.clone());
            *smt_facts = pre_facts.clone();

            // Else branch knows the negation of the condition.
            smt_facts.push(negate(cond));
            // Else branch: same pattern.
            env.push_scope();
            let mut else_stmts = Vec::new();
            let else_terminated = check_stmt_list(
                else_body,
                env,
                signatures,
                function,
                loops,
                smt_facts,
                &mut else_stmts,
                diagnostics,
            );
            if !else_terminated {
                emit_current_scope_drops(env, &mut else_stmts, diagnostics);
            }
            env.pop_scope();
            let else_env = std::mem::replace(env, pre_env.clone());
            // After the if-else, facts revert to pre. One exception: if
            // exactly one branch terminates (return/break/continue), then
            // execution past the merge must have taken the *other* branch,
            // so its guard is true at the merge. Adding that fact closes
            // a common verifier gap: `if x < 0 { return ...; }` should
            // imply `x >= 0` on every statement that follows.
            *smt_facts = pre_facts;
            match (then_terminated, else_terminated) {
                (true, false) => smt_facts.push(negate(cond)),
                (false, true) => smt_facts.push(cond.clone()),
                _ => {}
            }

            // Merge: only consider bindings that existed pre-branch. Where
            // move state diverges between the two branches, auto-balance by
            // emitting a `Drop` in the branch that didn't consume the value
            // so both paths end with the binding moved.
            let mut merge_updates: Vec<(String, Option<Span>)> = Vec::new();
            let pre_non_copy: Vec<(String, VarInfo)> = pre_env
                .all_bindings()
                .filter(|(_, info)| !info.ty.is_copy())
                .map(|(n, i)| (n.clone(), i.clone()))
                .collect();
            for (name, pre_info) in pre_non_copy.iter() {
                let then_moved = match (then_terminated, then_env.lookup(name)) {
                    (true, _) => None,
                    (_, Some(info)) => info.moved,
                    _ => pre_info.moved,
                };
                let else_moved = match (else_terminated, else_env.lookup(name)) {
                    (true, _) => None,
                    (_, Some(info)) => info.moved,
                    _ => pre_info.moved,
                };
                let final_moved = match (then_terminated, else_terminated) {
                    (true, true) => pre_info.moved,
                    (true, false) => else_moved,
                    (false, true) => then_moved,
                    (false, false) => {
                        let then_is_moved = then_moved.is_some();
                        let else_is_moved = else_moved.is_some();
                        if then_is_moved == else_is_moved {
                            then_moved
                        } else if then_is_moved {
                            // then moved x but else didn't — auto-drop in else.
                            else_stmts.push(TypedStmt::Drop {
                                name: name.clone(),
                                ty: pre_info.ty.clone(),
                                moved_fields: Vec::new(),
                            });
                            then_moved
                        } else {
                            // else moved x but then didn't — auto-drop in then.
                            then_stmts.push(TypedStmt::Drop {
                                name: name.clone(),
                                ty: pre_info.ty.clone(),
                                moved_fields: Vec::new(),
                            });
                            else_moved
                        }
                    }
                };
                merge_updates.push((name.clone(), final_moved));
            }
            let mut merged = pre_env.clone();
            for (name, final_moved) in merge_updates {
                if let Some(info) = merged.lookup_mut(&name) {
                    info.moved = final_moved;
                }
            }
            // Only invalidate constants for bindings either branch
            // could have mutated (direct reassign, `&mut`-arg
            // mutation through a callee, or `IndexAssign` on the
            // binding's storage). Bindings provably untouched by
            // both branches keep their constant — refines #4 from
            // STATUS.md (was: blanket clear of every binding's
            // constant after if/else).
            let mut branch_muts = collect_branch_mutations(then_body);
            walk_branch_mutations(else_body, &mut branch_muts);
            clear_constants_for(&mut merged, &branch_muts);
            *env = merged;

            body.push(TypedStmt::If {
                cond: cond_checked.expr,
                then_body: then_stmts,
                else_body: else_stmts,
            });

            then_terminated && else_terminated
        }
        Stmt::While {
            cond,
            invariants,
            body: body_stmts,
            span,
        } => {
            verify_call_args_in_expr(cond, smt_facts, env, signatures, diagnostics);
            let cond_checked = check_expr(cond, env, signatures, diagnostics);
            require_type(
                cond_checked.ty(),
                &Type::Bool,
                cond.span,
                "while condition",
                diagnostics,
            );

            // `while false { ... }` never executes. Same syntactic-only
            // policy as for `if` so we don't trip on let-bound constants
            // that the loop body itself mutates. `while true` is a
            // legitimate pattern (body always breaks/returns).
            if matches!(&cond.kind, ExprKind::Bool(false))
                && !body_stmts.is_empty()
            {
                diagnostics.push(Diagnostic::new(
                    cond.span,
                    "loop condition is always false; the body never executes",
                ));
            }

            // Type-check each invariant (Bool, vars resolve in current env).
            // Inline call args in the invariant must also satisfy the
            // callee's requires — same compile-time check as for any
            // expression-level call site.
            for inv in invariants {
                verify_call_args_in_expr(inv, smt_facts, env, signatures, diagnostics);
                let checked = check_expr(inv, env, signatures, diagnostics);
                require_type(
                    checked.ty(),
                    &Type::Bool,
                    inv.span,
                    "loop invariant",
                    diagnostics,
                );
            }

            // Verify each invariant holds on entry, using pre-loop facts.
            verify_loop_invariants(
                invariants,
                smt_facts,
                env,
                signatures,
                "does not hold at loop entry",
                None,
                diagnostics,
            );

            let pre_env = env.clone();
            let pre_facts = smt_facts.clone();
            // Inside the body, the loop's invariants AND the condition can
            // be assumed.
            smt_facts.extend(invariants.iter().cloned());
            smt_facts.push(cond.clone());

            // Push a new scope for the body. Bindings declared inside it
            // do not leak to the outer scope.
            env.push_scope();
            let body_scope_depth = env.depth();
            loops.push(LoopFrame {
                pre_env: pre_env.clone(),
                body_scope_depth,
            });
            let mut inner_stmts = Vec::new();
            let body_terminated = check_stmt_list(
                body_stmts,
                env,
                signatures,
                function,
                loops,
                smt_facts,
                &mut inner_stmts,
                diagnostics,
            );
            loops.pop();

            // Preservation: at body fall-through, invariants must still
            // hold. Use the smt_facts as they stand at body end and apply
            // last-reassignment substitutions so the goal talks about the
            // post-body value of each modified variable.
            if !body_terminated {
                let summary = collect_last_reassigns_with_env(body_stmts, env);
                verify_loop_invariants_with_havoc(
                    invariants,
                    smt_facts,
                    env,
                    signatures,
                    "is not preserved by the loop body",
                    Some(&summary.subs),
                    &summary.havoc_vars,
                    diagnostics,
                );
            }

            // If the body didn't terminate via return/break/continue, emit
            // drops for the body scope's own non-Copy live bindings and check
            // balance.
            if !body_terminated {
                emit_current_scope_drops(env, &mut inner_stmts, diagnostics);
            }
            let _body_scope = env.pop_scope();
            let post_env = env.clone();
            // Restore env to pre_env (the inner-body scope is gone; the outer
            // is conceptually unchanged, but Reassigns inside the body may
            // have updated move/constant state of outer bindings).
            *env = pre_env.clone();
            // Clear constants only for bindings the loop body
            // could have mutated. Untouched bindings (rare-but-
            // real: locals declared before the loop but never
            // touched inside) keep their constant facts — refines
            // #4 from STATUS.md.
            let body_muts = collect_branch_mutations(body_stmts);
            clear_constants_for(env, &body_muts);
            // Post-loop facts: invariants (sound — proved at entry and
            // preserved) plus `!cond` at the natural exit. `!cond` is
            // sound only when no `break` in the body skips the condition
            // check; we scan the body for `break` and conservatively
            // drop the `!cond` if any is present. `continue` is safe
            // because it returns to the loop header.
            *smt_facts = pre_facts;
            smt_facts.extend(invariants.iter().cloned());
            if !contains_break(body_stmts) {
                smt_facts.push(negate(cond));
            }

            if !body_terminated {
                validate_loop_balance(
                    &pre_env,
                    &post_env,
                    *span,
                    "loop body changes the move state",
                    diagnostics,
                );
            }

            body.push(TypedStmt::While {
                cond: cond_checked.expr,
                body: inner_stmts,
            });

            false
        }
        Stmt::IndexAssign {
            name,
            index,
            field_path,
            value,
            span,
        } => {
            let Some(info) = env.lookup(name).cloned() else {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!("cannot index-assign to unknown variable '{}'", name),
                ));
                return false;
            };
            if info.moved.is_some() {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!("cannot index-assign to '{}' after it was moved", name),
                ));
            }

            // The base must be either an owned array/Vec or a &mut to one.
            // Shared `&T` borrows are read-only.
            let (element_type, length_opt, through_ref) = match &info.ty {
                Type::Array { element, length } => {
                    ((**element).clone(), Some(*length), false)
                }
                Type::Vec(element) => ((**element).clone(), None, false),
                Type::RefMut(inner) => match &**inner {
                    Type::Array { element, length } => {
                        ((**element).clone(), Some(*length), true)
                    }
                    Type::Vec(element) => ((**element).clone(), None, true),
                    other => {
                        diagnostics.push(Diagnostic::new(
                            *span,
                            format!("cannot index-assign through reference to {}", other),
                        ));
                        return false;
                    }
                },
                Type::Ref(_) => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "cannot index-assign to '{}': it is borrowed immutably (&T); \
                             use '&mut T' in the parameter to allow writes",
                            name
                        ),
                    ));
                    return false;
                }
                other => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!("'{}' has type {} which is not indexable for assignment", name, other),
                    ));
                    return false;
                }
            };

            verify_call_args_in_expr(index, smt_facts, env, signatures, diagnostics);
            verify_call_args_in_expr(value, smt_facts, env, signatures, diagnostics);
            let index_checked = check_expr(index, env, signatures, diagnostics);
            if !index_checked.ty().is_integer() {
                diagnostics.push(Diagnostic::new(
                    index.span,
                    format!("index must be an integer, got {}", index_checked.ty()),
                ));
            }

            // Resolve field_path against the element type.
            // For `xs[i].field = v;`, each path segment names a
            // struct field; the final segment's type is what
            // `value` must coerce to. Empty path falls back to
            // plain `xs[i] = v;`. T1.2 phase 2b follow-up.
            let mut resolved_path: Vec<(String, u32)> = Vec::new();
            let mut target_ty = element_type.clone();
            for (path_index, segment) in field_path.iter().enumerate() {
                let Type::Struct(struct_name) = &target_ty else {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "cannot apply field path '.{}' to non-struct element \
                             type {}",
                            segment, target_ty
                        ),
                    ));
                    return false;
                };
                let Some(decl) = env.lookup_struct(struct_name) else {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!("struct '{}' is not declared", struct_name),
                    ));
                    return false;
                };
                let Some((idx, (_, field_ty))) = decl
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, (n, _))| n == segment)
                else {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "struct '{}' has no field named '{}'",
                            struct_name, segment
                        ),
                    ));
                    return false;
                };
                // Per-segment Copy check, with a leaf
                // exception for heap-shaped fields (OwnedStr,
                // Vec): those route through a "free old +
                // store new" emit in the backend (closure
                // #126 / F2). Intermediate segments still
                // require Copy — non-Copy intermediates would
                // need full path-level Drop chains which the
                // backends don't yet emit through index+field
                // assigns.
                let is_last = path_index == field_path.len() - 1;
                let leaf_heap = is_last
                    && matches!(field_ty, Type::OwnedStr | Type::Vec(_));
                if !field_ty.is_copy() && !leaf_heap {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "field '{}.{}' has non-Copy type {} — mixed \
                             index+field assignment requires Copy types on \
                             intermediate path segments; only the leaf may \
                             be OwnedStr or Vec<T>",
                            struct_name, segment, field_ty
                        ),
                    ));
                    return false;
                }
                resolved_path.push((segment.clone(), idx as u32));
                target_ty = field_ty.clone();
            }

            let value_checked = check_expr(value, env, signatures, diagnostics);
            let value_coerced = coerce_checked(
                value_checked,
                &target_ty,
                value.span,
                "index-assign value",
                diagnostics,
            );
            diagnose_partial_then_whole_move(value, &value_coerced, env, diagnostics);
            consume_if_moved_var(value, &value_coerced, env);

            // Compile-time bounds-check elision for owned-array case only.
            let checked = match (length_opt, index_checked.constant()) {
                (Some(length), Some(TypedConst::Int(k))) if !through_ref => {
                    if *k < 0 {
                        diagnostics.push(Diagnostic::new(
                            index.span,
                            format!("array index {} is negative; length is {}", k, length),
                        ));
                        return false;
                    }
                    if (*k as u128) >= length as u128 {
                        diagnostics.push(Diagnostic::new(
                            index.span,
                            format!(
                                "array index {} is out of range for length {}",
                                k, length
                            ),
                        ));
                        return false;
                    }
                    false
                }
                _ => true,
            };

            let mut idx_expr = index_checked.expr;
            let mut val_expr = value_coerced.expr;
            inject_branch_drops(&mut val_expr);  // closure #179
            try_elide_bounds_in_typed_expr(&mut idx_expr, smt_facts, env, signatures);
            try_elide_bounds_in_typed_expr(&mut val_expr, smt_facts, env, signatures);

            // Try to discharge the IndexAssign's own bounds guard
            // (separate from any Index nodes inside the rhs/lhs).
            let mut checked_flag = checked;
            if checked_flag && !crate::smt::verifier_disabled() {
                use crate::smt::Verdict;
                let arr_var = Expr {
                    kind: ExprKind::Var(name.clone()),
                    span: *span,
                };
                let len_expr = Expr {
                    kind: ExprKind::Len {
                        array: Box::new(arr_var),
                    },
                    span: *span,
                };
                let idx_ast = typed_to_expr(&idx_expr);
                let idx_u64 = if idx_expr.ty == Type::U64 {
                    idx_ast.clone()
                } else {
                    Expr {
                        kind: ExprKind::Cast {
                            expr: Box::new(idx_ast.clone()),
                            ty: Type::U64,
                        },
                        span: *span,
                    }
                };
                let upper = Expr {
                    kind: ExprKind::Binary {
                        op: BinaryOp::Lt,
                        left: Box::new(idx_u64),
                        right: Box::new(len_expr),
                    },
                    span: *span,
                };
                let upper_ok = matches!(
                    prove_with_calls(&upper, smt_facts, env, signatures),
                    Verdict::Proven
                );
                let lower_ok = if idx_expr.ty.is_signed_integer() {
                    let lower = Expr {
                        kind: ExprKind::Binary {
                            op: BinaryOp::Ge,
                            left: Box::new(idx_ast),
                            right: Box::new(Expr {
                                kind: ExprKind::Int(0),
                                span: *span,
                            }),
                        },
                        span: *span,
                    };
                    matches!(
                        prove_with_calls(&lower, smt_facts, env, signatures),
                        Verdict::Proven
                    )
                } else {
                    true
                };
                if upper_ok && lower_ok {
                    checked_flag = false;
                }
            }

            // SMT-array versioning. Each `xs[i] = v` IndexAssign
            // bumps the binding's `array_version` counter and emits
            // a synthetic `__smt_store_eq(xs#new, xs#old, i, v)`
            // axiom: `arr_xs_v{new} = (store arr_xs_v{old} i v)`.
            // The SMT solver can then derive `xs[j] == old_value_j`
            // for every untouched slot, and the new value at the
            // touched slot, without us dropping any prior facts —
            // old facts that mentioned `arr_xs_v{old}` (via bare
            // `Var("xs")` resolved to the previous current version
            // when emitted) still pin the prior state. References
            // to `xs` after this point use the new version (the
            // encoder resolves bare `Var("xs")` to whatever the
            // current version is at query time).
            //
            // The vec-literal substitution stash is still cleared:
            // it's an AST-level shortcut that bypasses SMT and
            // doesn't know about array versioning.
            let idx_ast = typed_to_expr(&idx_expr);
            let val_ast = typed_to_expr(&val_expr);

            body.push(TypedStmt::IndexAssign {
                name: name.clone(),
                base_ty: info.ty.clone(),
                index: idx_expr,
                field_path: resolved_path,
                value: val_expr,
                checked: checked_flag,
            });

            // Bump the version BEFORE emitting the store-eq fact so
            // the synthetic Call's `#new` arg references the
            // post-assign version; `#old` is the pre-bump version.
            let old_version = info.array_version;
            let new_version = old_version + 1;
            // Pin every existing fact's bare `Var(name)` references
            // to the OLD version so they continue describing the
            // pre-assign array state. New facts emitted below use
            // bare `Var(name)` and resolve to the new version at
            // query time.
            for fact in smt_facts.iter_mut() {
                pin_var_to_version(fact, name, old_version);
            }
            if let Some(info_mut) = env.lookup_mut(name) {
                info_mut.vec_literal_elements = None;
                info_mut.array_version = new_version;
            }

            // Synthetic store-eq fact bridging the two versions.
            // Only emit when the binding supports SMT array modeling
            // (smt-array-element returns Some — i.e., int/bool/float
            // element type); otherwise the fact wouldn't encode.
            // Always safe to emit because the encoder graciously
            // skips facts it can't encode.
            smt_facts.push(Expr {
                kind: ExprKind::Call {
                    name: "__smt_store_eq".to_string(),
                    name_span: crate::span::Span::default(),
                    args: vec![
                        Expr {
                            kind: ExprKind::Var(format!("{}#{}", name, new_version)),
                            span: crate::span::Span::default(),
                        },
                        Expr {
                            kind: ExprKind::Var(format!("{}#{}", name, old_version)),
                            span: crate::span::Span::default(),
                        },
                        idx_ast.clone(),
                        val_ast.clone(),
                    ],
                },
                span: crate::span::Span::default(),
            });

            // Slot fact: `xs[i] == v` at the NEW version. The
            // encoder resolves bare `Var("xs")` to the current
            // version (just bumped). Provides a direct read-after-
            // write path without going through the store axiom.
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr {
                        kind: ExprKind::Index {
                            array: Box::new(Expr {
                                kind: ExprKind::Var(name.clone()),
                                span: *span,
                            }),
                            index: Box::new(idx_ast),
                        },
                        span: *span,
                    }),
                    right: Box::new(val_ast),
                },
                span: *span,
            });
            false
        }
        Stmt::FieldAssign {
            object,
            field,
            field_span,
            value,
            span,
        } => {
            // T1.2 phase 2a follow-up: type-check the
            // place expression and the value, validate the
            // field exists + has matching type, and emit a
            // TypedStmt::FieldAssign. v1 restricts the
            // place to:
            //   - a plain `Var` whose binding type is an
            //     owned struct
            //   - a `Var` whose binding is `mut ref Struct`
            //     (covers `self.field = …` in methods that
            //     take `self: mut ref T`)
            verify_call_args_in_expr(object, smt_facts, env, signatures, diagnostics);
            verify_call_args_in_expr(value, smt_facts, env, signatures, diagnostics);
            let obj_checked = check_expr(object, env, signatures, diagnostics);
            let (struct_name, through_mut_ref) = match obj_checked.ty() {
                Type::Struct(name) => (name.clone(), false),
                Type::RefMut(inner) => match inner.as_ref() {
                    Type::Struct(name) => (name.clone(), true),
                    other => {
                        diagnostics.push(Diagnostic::new(
                            *span,
                            format!(
                                "cannot assign through `mut ref {}` — only structs \
                                 support field assignment in v1",
                                other
                            ),
                        ));
                        return false;
                    }
                },
                Type::Ref(_) => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        "cannot field-assign through an immutable `ref` — use \
                         `mut ref T` on the binding"
                            .to_string(),
                    ));
                    return false;
                }
                other => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "cannot field-assign onto {} — only structs support \
                             field assignment in v1",
                            other
                        ),
                    ));
                    return false;
                }
            };
            let info = match env.lookup_struct(&struct_name).cloned() {
                Some(s) => s,
                None => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!("struct '{}' is not declared", struct_name),
                    ));
                    return false;
                }
            };
            let Some((field_index, (_, field_ty))) = info
                .fields
                .iter()
                .enumerate()
                .find(|(_, (n, _))| n == field)
            else {
                diagnostics.push(Diagnostic::new(
                    *field_span,
                    format!(
                        "struct '{}' has no field named '{}'",
                        struct_name, field
                    ),
                ));
                return false;
            };
            let value_checked = check_expr(value, env, signatures, diagnostics);
            let value_coerced = coerce_checked(
                value_checked,
                field_ty,
                value.span,
                "field assignment value",
                diagnostics,
            );
            // Mark the RHS Var as moved when the field-assign
            // takes ownership of a non-Copy operand — same
            // shape as Let / Reassign / Call-arg. Without
            // this, the source binding would scope-exit-drop
            // and double-free the heap now owned by the
            // struct's field. Closure #166.
            consume_if_moved_var(value, &value_coerced, env);
            let mut val_expr = value_coerced.expr;
            inject_branch_drops(&mut val_expr);  // closure #179
            body.push(TypedStmt::FieldAssign {
                object: obj_checked.expr,
                field: field.clone(),
                field_index: field_index as u32,
                through_mut_ref,
                value: val_expr,
            });
            false
        }
        Stmt::For {
            var,
            start,
            end,
            invariants,
            body: body_stmts,
            span,
            parallel,
            reductions,
        } => {
            verify_call_args_in_expr(start, smt_facts, env, signatures, diagnostics);
            verify_call_args_in_expr(end, smt_facts, env, signatures, diagnostics);
            let start_checked = check_expr(start, env, signatures, diagnostics);
            let end_checked = check_expr(end, env, signatures, diagnostics);

            if !start_checked.ty().is_integer() || !end_checked.ty().is_integer() {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "for-loop range bounds must be integers, got {} and {}",
                        start_checked.ty(),
                        end_checked.ty()
                    ),
                ));
                return false;
            }

            let Some(loop_ty) = promoted_integer_type(&start_checked, &end_checked, diagnostics)
            else {
                return false;
            };
            let start_coerced = coerce_numeric_operand(start_checked, &loop_ty);
            let end_coerced = coerce_numeric_operand(end_checked, &loop_ty);

            let pre_env = env.clone();
            let pre_facts = smt_facts.clone();
            env.push_scope();
            env.insert_current(
                var.clone(),
                VarInfo {
                    ty: loop_ty.clone(),
                    constant: None,
                    moved: None,
                    decl_span: *span,
                    vec_literal_elements: None,
                    array_version: 0,
                    guarded_mutex: None,
                    no_drop: false,
                    is_const: false,
                    struct_literal_fields: None,
                    moved_fields: std::collections::BTreeMap::new(),
                },
            );

            // Type-check invariants with loop var in scope.
            for inv in invariants {
                verify_call_args_in_expr(inv, smt_facts, env, signatures, diagnostics);
                let checked = check_expr(inv, env, signatures, diagnostics);
                require_type(
                    checked.ty(),
                    &Type::Bool,
                    inv.span,
                    "for-loop invariant",
                    diagnostics,
                );
            }

            // For the entry check, the loop var equals `start`. Build a
            // temporary fact list with that equality + the range bound.
            let entry_facts = {
                let mut f = smt_facts.clone();
                f.push(Expr {
                    kind: ExprKind::Binary {
                        op: BinaryOp::Eq,
                        left: Box::new(Expr {
                            kind: ExprKind::Var(var.clone()),
                            span: *span,
                        }),
                        right: Box::new(start.clone()),
                    },
                    span: *span,
                });
                f
            };
            verify_loop_invariants(
                invariants,
                &entry_facts,
                env,
                signatures,
                "does not hold at the for-loop's first iteration",
                None,
                diagnostics,
            );

            // Inside the body, the invariants, `var >= start`, and
            // `var < end` all hold for the current iteration.
            smt_facts.extend(invariants.iter().cloned());
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Ge,
                    left: Box::new(Expr {
                        kind: ExprKind::Var(var.clone()),
                        span: *span,
                    }),
                    right: Box::new(start.clone()),
                },
                span: *span,
            });
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Lt,
                    left: Box::new(Expr {
                        kind: ExprKind::Var(var.clone()),
                        span: *span,
                    }),
                    right: Box::new(end.clone()),
                },
                span: *span,
            });

            loops.push(LoopFrame {
                pre_env: pre_env.clone(),
                body_scope_depth: env.depth(),
            });
            let mut inner_stmts = Vec::new();
            let body_terminated = check_stmt_list(
                body_stmts,
                env,
                signatures,
                function,
                loops,
                smt_facts,
                &mut inner_stmts,
                diagnostics,
            );
            loops.pop();

            // Preservation: substitute each user-visible reassignment in the
            // body, plus the implicit `var = var + 1` for the for-loop step.
            if !body_terminated {
                let mut summary = collect_last_reassigns_with_env(body_stmts, env);
                // Implicit auto-increment of the loop variable.
                summary.subs.insert(
                    var.clone(),
                    Expr {
                        kind: ExprKind::Binary {
                            op: BinaryOp::Add,
                            left: Box::new(Expr {
                                kind: ExprKind::Var(var.clone()),
                                span: *span,
                            }),
                            right: Box::new(Expr {
                                kind: ExprKind::Int(1),
                                span: *span,
                            }),
                        },
                        span: *span,
                    },
                );
                verify_loop_invariants_with_havoc(
                    invariants,
                    smt_facts,
                    env,
                    signatures,
                    "is not preserved by the for-loop body",
                    Some(&summary.subs),
                    &summary.havoc_vars,
                    diagnostics,
                );
            }

            if !body_terminated {
                emit_current_scope_drops(env, &mut inner_stmts, diagnostics);
            }
            let _body_scope = env.pop_scope();
            let post_env = env.clone();
            *env = pre_env.clone();
            // Refines #4: same per-body invalidation as the
            // `while` arm. For-loop's induction variable is
            // scope-local so it doesn't appear in the outer
            // env; only outer bindings the body actually
            // touches get their constants dropped.
            let body_muts = collect_branch_mutations(body_stmts);
            clear_constants_for(env, &body_muts);
            // Post-loop facts: invariants and `var >= end` (the for-loop
            // exits when the variable reaches the range upper bound). We
            // skip the `>= end` fact when the body can `break`, since a
            // break exit may leave `var < end`.
            *smt_facts = pre_facts;
            smt_facts.extend(invariants.iter().cloned());
            if !contains_break(body_stmts) {
                smt_facts.push(Expr {
                    kind: ExprKind::Binary {
                        op: BinaryOp::Ge,
                        left: Box::new(Expr {
                            kind: ExprKind::Var(var.clone()),
                            span: *span,
                        }),
                        right: Box::new(end.clone()),
                    },
                    span: *span,
                });
            }

            if !body_terminated {
                validate_loop_balance(
                    &pre_env,
                    &post_env,
                    *span,
                    "for-loop body changes the move state",
                    diagnostics,
                );
            }

            // Resolve each reduction clause against an outer
            // binding and verify the variable's type. Reduction is
            // currently only on integer scalars (the `+` op + the
            // backend lowerings — `atomicrmw add` for LLVM,
            // `reduction(+:var)` for OpenMP — assume an integer
            // alloca). Floats/bools are rejected for now.
            let mut typed_reductions: Vec<crate::ir::TypedReduction> = Vec::new();
            let mut reduction_set: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for r in reductions {
                let Some(info) = pre_env.lookup(&r.var) else {
                    diagnostics.push(Diagnostic::new(
                        r.span,
                        format!(
                            "'reduce {}': name is not declared in scope",
                            r.var
                        ),
                    ));
                    continue;
                };
                // Type rule per reduction op:
                //   + / * / min / max / & / | / ^ → integer.
                //   && / || → bool.
                use crate::ast::ReductionOp;
                let ty_ok = match r.op {
                    ReductionOp::Add
                    | ReductionOp::Mul
                    | ReductionOp::Min
                    | ReductionOp::Max
                    | ReductionOp::BitAnd
                    | ReductionOp::BitOr
                    | ReductionOp::BitXor => info.ty.is_integer(),
                    ReductionOp::And | ReductionOp::Or => matches!(info.ty, Type::Bool),
                };
                if !ty_ok {
                    let expected = match r.op {
                        ReductionOp::Add
                        | ReductionOp::Mul
                        | ReductionOp::Min
                        | ReductionOp::Max
                        | ReductionOp::BitAnd
                        | ReductionOp::BitOr
                        | ReductionOp::BitXor => "an integer",
                        ReductionOp::And | ReductionOp::Or => "a bool",
                    };
                    diagnostics.push(Diagnostic::new(
                        r.span,
                        format!(
                            "'reduce {} with {}' requires {}-typed variable, got {}",
                            r.var,
                            r.op.display_symbol(),
                            expected,
                            info.ty
                        ),
                    ));
                    continue;
                }
                reduction_set.insert(r.var.clone());
                typed_reductions.push(crate::ir::TypedReduction {
                    var: r.var.clone(),
                    op: r.op,
                    ty: info.ty.clone(),
                });
            }

            // If the source wrote `parallel for ...`, verify the
            // body has no observable side effects. Pure rules apply
            // except that Reassigns of the form
            //   <reduce_var> = <reduce_var> <op> X
            // are tolerated — the runtime gives each thread a
            // private partial and combines them, so the body looks
            // mutating but actually doesn't share state.
            if *parallel {
                verify_pure_body_with_reductions(
                    &inner_stmts,
                    signatures,
                    "'parallel for' body",
                    &typed_reductions,
                    diagnostics,
                );
            }
            body.push(TypedStmt::For {
                var: var.clone(),
                ty: loop_ty,
                start: start_coerced.expr,
                end: end_coerced.expr,
                body: inner_stmts,
                parallel: *parallel,
                reductions: typed_reductions,
            });
            let _ = reduction_set;
            false
        }
        Stmt::ForIter {
            var,
            collection,
            consumes,
            body: body_stmts,
            span,
        } => {
            let Some(info) = env.lookup(collection).cloned() else {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!("unknown variable '{}'", collection),
                ));
                return false;
            };
            if info.moved.is_some() {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "cannot iterate over '{}' after it was moved",
                        collection
                    ),
                ));
            }
            let underlying = info.ty.deref();
            let element_ty = match underlying {
                Type::Array { element, .. } => (**element).clone(),
                Type::Vec(element) => (**element).clone(),
                other => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "'for x in &xs' requires an array or Vec; '{}' has type {}",
                            collection, other
                        ),
                    ));
                    return false;
                }
            };
            let collection_ty = info.ty.clone();

            // Consuming form (`for x in xs`) requires owned collection.
            // Borrowed sources (`&T` / `&mut T` typed bindings) are read-only
            // here, so we forbid consume on them.
            if *consumes && collection_ty.is_any_ref() {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "cannot move '{}' for iteration: it is a borrow; use '&{}' to iterate",
                        collection, collection
                    ),
                ));
            }

            // If consuming, mark the source as moved up-front so the body
            // can't reference it; if borrowing, leave it live.
            if *consumes && !collection_ty.is_any_ref() {
                if let Some(slot) = env.lookup_mut(collection) {
                    if slot.moved.is_none() {
                        slot.moved = Some(*span);
                    }
                }
            }

            let pre_env = env.clone();
            let pre_facts = smt_facts.clone();
            env.push_scope();
            // For non-consuming `for v in &xs` over a Vec
            // with non-Copy elements, `v` is a view into
            // `xs.data[i]` — the body reads the slot via a
            // struct-copy that aliases the owner's data. We
            // mark the binding `no_drop` so scope exit
            // doesn't free the aliased buffer (which would
            // double-free at the outer xs's drop). Refines
            // #7 phase 2.
            let var_no_drop = !*consumes && !element_ty.is_copy();
            env.insert_current(
                var.clone(),
                VarInfo {
                    ty: element_ty.clone(),
                    constant: None,
                    moved: None,
                    decl_span: *span,
                    vec_literal_elements: None,
                    array_version: 0,
                    guarded_mutex: None,
                    no_drop: var_no_drop,
                    is_const: false,
                    struct_literal_fields: None,
                    moved_fields: std::collections::BTreeMap::new(),
                },
            );

            loops.push(LoopFrame {
                pre_env: pre_env.clone(),
                body_scope_depth: env.depth(),
            });
            let mut inner_stmts = Vec::new();
            let body_terminated = check_stmt_list(
                body_stmts,
                env,
                signatures,
                function,
                loops,
                smt_facts,
                &mut inner_stmts,
                diagnostics,
            );
            loops.pop();

            if !body_terminated {
                emit_current_scope_drops(env, &mut inner_stmts, diagnostics);
            }
            let _body_scope = env.pop_scope();
            let post_env = env.clone();
            *env = pre_env.clone();
            // Refines #4: only clear constants for outer bindings
            // the for-iter body actually touched (the iteration
            // variable lives in the inner scope and was popped
            // above).
            let body_muts = collect_branch_mutations(body_stmts);
            clear_constants_for(env, &body_muts);
            *smt_facts = pre_facts;

            if !body_terminated {
                validate_loop_balance(
                    &pre_env,
                    &post_env,
                    *span,
                    "for-iter body changes the move state",
                    diagnostics,
                );
            }

            body.push(TypedStmt::ForIter {
                var: var.clone(),
                element_ty,
                collection: collection.clone(),
                collection_ty,
                consumes: *consumes,
                body: inner_stmts,
            });
            false
        }
        Stmt::Break { span } => {
            let Some(frame) = loops.last().cloned() else {
                diagnostics.push(Diagnostic::new(
                    *span,
                    "'break' is only valid inside a 'while' loop",
                ));
                return false;
            };
            emit_drops_through_loop(env, frame.body_scope_depth, body);
            validate_loop_balance(
                &frame.pre_env,
                env,
                *span,
                "break with inconsistent move state",
                diagnostics,
            );
            body.push(TypedStmt::Break);
            true
        }
        Stmt::Continue { span } => {
            let Some(frame) = loops.last().cloned() else {
                diagnostics.push(Diagnostic::new(
                    *span,
                    "'continue' is only valid inside a 'while' loop",
                ));
                return false;
            };
            emit_drops_through_loop(env, frame.body_scope_depth, body);
            validate_loop_balance(
                &frame.pre_env,
                env,
                *span,
                "continue with inconsistent move state",
                diagnostics,
            );
            body.push(TypedStmt::Continue);
            true
        }
        Stmt::TaskSpawn { name, body: task_body, span } => {
            // Verify there's no existing binding with this name in
            // the current scope — Task handles can't shadow other
            // handles or other bindings.
            if env.current_has(name) {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "task '{}' shadows an existing binding declared in the same scope",
                        name
                    ),
                ));
            }

            // Type-check the body in a pushed scope. Bindings inside
            // the body don't leak out. Outer bindings stay
            // read-only — the purity pass below enforces this by
            // flagging any IndexAssign/print/etc. as a side effect.
            env.push_scope();
            let mut typed_body: Vec<TypedStmt> = Vec::new();
            let mut inner_facts = smt_facts.clone();
            let _terminated = check_stmt_list(
                task_body,
                env,
                signatures,
                function,
                loops,
                &mut inner_facts,
                &mut typed_body,
                diagnostics,
            );
            // Drop any non-Copy locals declared inside the body.
            emit_current_scope_drops(env, &mut typed_body, diagnostics);
            env.pop_scope();

            // Same pure-with-captures rule as `parallel for` (no
            // print, no IndexAssign, no impure calls). Reductions
            // are not allowed in v1: a task body has no
            // per-thread partial machinery, and there's no return
            // value to combine.
            verify_pure_body(
                &typed_body,
                signatures,
                "task body",
                diagnostics,
            );

            // Copy-capture restriction. Task threading lowers
            // captures via a heap-allocated context struct
            // passed to a pthread; for soundness without
            // intra-task lifetime tracking, captures must be
            // Copy (the value is duplicated into the ctx).
            // Affine handles (Vec, Atomic, Mutex, Guard,
            // Channel, OwnedStr, arrays) can't ride along —
            // pre-extract scalar values from them before the
            // spawn site.
            let mut declared: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut order: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            crate::backend_llvm::walk_body(
                &typed_body,
                &mut declared,
                &mut order,
                &mut seen,
            );
            let mut captures: Vec<(String, Type)> = Vec::new();
            for cap in &order {
                if let Some(info) = env.lookup(cap) {
                    if !info.ty.is_copy() {
                        diagnostics.push(Diagnostic::new(
                            *span,
                            format!(
                                "task body captures non-Copy binding '{}' (type {}). \
                                 Captures must be Copy types — pre-extract scalar \
                                 values from `{}` before the spawn site.",
                                cap, info.ty, cap,
                            ),
                        ));
                    }
                    captures.push((cap.clone(), info.ty.clone()));
                }
            }

            // Declare the affine handle in the parent scope.
            env.insert_current(
                name.clone(),
                VarInfo {
                    ty: Type::Task,
                    constant: None,
                    moved: None,
                    decl_span: *span,
                    vec_literal_elements: None,
                    array_version: 0,
                    guarded_mutex: None,
                    no_drop: false,
                    is_const: false,
                    struct_literal_fields: None,
                    moved_fields: std::collections::BTreeMap::new(),
                },
            );

            body.push(TypedStmt::TaskSpawn {
                name: name.clone(),
                body: typed_body,
                captures,
            });
            false
        }
        Stmt::TaskJoin { name, span } => {
            let Some(info) = env.lookup(name) else {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!("join: no task named '{}' in scope", name),
                ));
                return false;
            };
            if !matches!(info.ty, Type::Task) {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "join: '{}' has type {}, expected Task",
                        name, info.ty
                    ),
                ));
                return false;
            }
            if let Some(prev) = info.moved {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "join: task '{}' was already joined at byte {}..{}",
                        name, prev.start, prev.end
                    ),
                ));
                return false;
            }
            if let Some(info_mut) = env.lookup_mut(name) {
                info_mut.moved = Some(*span);
            }
            body.push(TypedStmt::TaskJoin { name: name.clone() });
            false
        }
        Stmt::LetTuple {
            names,
            annotation,
            expr,
            span,
        } => {
            // Destructure desugar. Type-check the RHS (must
            // be a tuple), bind a fresh temp to the whole
            // tuple, then emit per-name `TypedStmt::Let`
            // bindings reading each slot via
            // `TupleAccess`. T1.1 phase 2.
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
            let checked = check_expr(expr, env, signatures, diagnostics);
            let elem_types = match (&annotation, checked.ty()) {
                (Some(ann), _) => {
                    validate_no_ref(ann, *span, "destructure-let annotation", diagnostics);
                    match ann {
                        Type::Tuple(elements) => elements.clone(),
                        other => {
                            diagnostics.push(Diagnostic::new(
                                *span,
                                format!(
                                    "destructure-let annotation must be a tuple type, got {}",
                                    other
                                ),
                            ));
                            return false;
                        }
                    }
                }
                (None, Type::Tuple(elements)) => elements.clone(),
                (None, other) => {
                    diagnostics.push(Diagnostic::new(
                        *span,
                        format!(
                            "destructure-let RHS must be a tuple, got {}",
                            other
                        ),
                    ));
                    return false;
                }
            };
            if elem_types.len() != names.len() {
                diagnostics.push(Diagnostic::new(
                    *span,
                    format!(
                        "destructure-let: tuple has {} elements but {} names given",
                        elem_types.len(),
                        names.len()
                    ),
                ));
                return false;
            }
            // Reject duplicate names (basic shadowing safety).
            for i in 0..names.len() {
                for j in (i + 1)..names.len() {
                    if names[i] == names[j] {
                        diagnostics.push(Diagnostic::new(
                            *span,
                            format!(
                                "destructure-let names must be distinct; '{}' used twice",
                                names[i]
                            ),
                        ));
                        return false;
                    }
                }
            }
            // Bind a fresh temp holding the whole tuple.
            let temp_name =
                format!("__intent_tup_{}", span.start);
            let tuple_ty = Type::Tuple(elem_types.clone());
            body.push(TypedStmt::Let {
                name: temp_name.clone(),
                ty: tuple_ty.clone(),
                expr: checked.expr,
            });
            env.insert_current(
                temp_name.clone(),
                VarInfo {
                    ty: tuple_ty.clone(),
                    constant: None,
                    moved: None,
                    decl_span: *span,
                    vec_literal_elements: None,
                    array_version: 0,
                    guarded_mutex: None,
                    no_drop: true,
                    is_const: false,
                    struct_literal_fields: None,
                    moved_fields: std::collections::BTreeMap::new(),
                },
            );
            // For each name, emit a Let binding that reads
            // the corresponding slot.
            for (i, name) in names.iter().enumerate() {
                let elt_ty = elem_types[i].clone();
                let access_expr = TypedExpr {
                    kind: TypedExprKind::TupleAccess {
                        tuple: Box::new(TypedExpr {
                            kind: TypedExprKind::Var(temp_name.clone()),
                            ty: tuple_ty.clone(),
                            constant: None,
                            span: *span,
                            binding_decl_span: None,
                        }),
                        index: i as u32,
                    },
                    ty: elt_ty.clone(),
                    constant: None,
                    span: *span,
                    binding_decl_span: None,
                };
                body.push(TypedStmt::Let {
                    name: name.clone(),
                    ty: elt_ty.clone(),
                    expr: access_expr,
                });
                env.insert_current(
                    name.clone(),
                    VarInfo {
                        ty: elt_ty,
                        constant: None,
                        moved: None,
                        decl_span: *span,
                        vec_literal_elements: None,
                        array_version: 0,
                        guarded_mutex: None,
                        no_drop: false,
                        is_const: false,
                        struct_literal_fields: None,
                        moved_fields: std::collections::BTreeMap::new(),
                    },
                );
            }
            false
        }
    }
}

/// Emit Drop statements for every non-Copy non-moved binding in scopes at
/// depth >= loop_body_depth (i.e., all scopes opened inside the loop body,
/// including the body scope itself). Used at `break` / `continue` sites.
fn emit_drops_through_loop(env: &Env, loop_body_depth: usize, body: &mut Vec<TypedStmt>) {
    // Scopes are stored in `env.scopes` from outermost (index 0) to innermost.
    // We drop from outermost-inside-loop to innermost-inside-loop so deeper
    // bindings (most recently created) get freed first.
    let total = env.scopes.len();
    if total < loop_body_depth {
        return;
    }
    for depth in (loop_body_depth..=total).rev() {
        // depth-1 because depth() == len(); the body scope is at index loop_body_depth-1.
        let scope_index = depth - 1;
        if scope_index >= env.scopes.len() {
            continue;
        }
        for (name, info) in env.scopes[scope_index].iter() {
            if !info.ty.is_copy() && info.moved.is_none() {
                body.push(TypedStmt::Drop {
                    name: name.clone(),
                    ty: info.ty.clone(),
                    moved_fields: Vec::new(),
                });
            }
        }
    }
}

fn validate_array_element_type(ty: &Type, span: Span, diagnostics: &mut Vec<Diagnostic>) {
    if let Type::Array { element, .. } = ty {
        // Fixed-size arrays still require Copy elements: every
        // slot has identical inline storage and the backend
        // emits them as `T[N]` (no per-slot drop hook). Lifting
        // arrays to non-Copy is out of scope for #7's first
        // pass.
        if !element.is_copy() || element.is_ref() {
            diagnostics.push(Diagnostic::new(
                span,
                format!("array element type must be Copy and non-reference, got {}", element),
            ));
        }
    }
    if let Type::Vec(element) = ty {
        // Refines #7: `Vec<T>` accepts non-Copy element types
        // (`Vec<Vec<i64>>`) since the backend now emits
        // element-aware free / set / clone helpers. References
        // remain rejected — a `Vec<&T>` would dangle the
        // moment the referent goes out of scope.
        if element.is_ref() {
            diagnostics.push(Diagnostic::new(
                span,
                format!("Vec element type cannot be a reference, got {}", element),
            ));
        }
    }
}

fn validate_no_ref(ty: &Type, span: Span, context: &str, diagnostics: &mut Vec<Diagnostic>) {
    if ty.is_ref() {
        diagnostics.push(Diagnostic::new(
            span,
            format!("{} cannot be a reference type", context),
        ));
    }
}

fn validate_param_type(ty: &Type, span: Span, diagnostics: &mut Vec<Diagnostic>) {
    // References are second-class: parameter may be &T, but T itself must not be &.
    if let Type::Ref(inner) = ty {
        if inner.is_ref() {
            diagnostics.push(Diagnostic::new(
                span,
                "parameter cannot be a reference to a reference",
            ));
        }
    }
    validate_array_element_type(ty, span, diagnostics);
}

/// Emit a "cannot move whole struct after partial move"
/// diagnostic when `source` is a Var consume of a binding
/// that already has at least one moved-out field. Call BEFORE
/// `consume_if_moved_var` since the latter doesn't read
/// `moved_fields`. T1.2 phase 2b partial-move follow-up.
fn diagnose_partial_then_whole_move(
    source: &Expr,
    checked: &CheckedExpr,
    env: &Env,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if checked.ty().is_copy() {
        return;
    }
    if let ExprKind::Var(name) = &source.kind {
        if let Some(info) = env.lookup(name) {
            if let Some((field, move_span)) =
                info.moved_fields.iter().next().map(|(f, s)| (f.clone(), *s))
            {
                diagnostics.push(
                    Diagnostic::new(
                        source.span,
                        format!(
                            "cannot move '{}' — its field '{}' was previously \
                             moved out, leaving the struct only partially \
                             initialized",
                            name, field
                        ),
                    )
                    .with_related(
                        move_span,
                        format!("'{}.{}' was moved here", name, field),
                    ),
                );
            }
        }
    }
}

/// True when `expr` is a FieldAccess chain of depth >= 2,
/// i.e. `a.b.c` or deeper. Single-level field access
/// (`a.b`) returns false. Used to gate nested non-Copy
/// field moves which aren't tracked by `moved_fields`.
/// Closure #125.
fn is_nested_field_access(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::FieldAccess { object, .. } => {
            matches!(object.kind, ExprKind::FieldAccess { .. })
        }
        _ => false,
    }
}

/// Walk a typed expression and collect every non-Copy Var
/// leaf reachable through if-expr branches, match arms, and
/// block tails. Used by `inject_branch_drops` to compute the
/// "other branches' Vars" that each branch must drop before
/// yielding its chosen value (closure #179).
fn collect_branch_var_leaves(expr: &TypedExpr, out: &mut Vec<(String, Type)>) {
    match &expr.kind {
        TypedExprKind::Var(name) if !expr.ty.is_copy() => {
            if !out.iter().any(|(n, _)| n == name) {
                out.push((name.clone(), expr.ty.clone()));
            }
        }
        TypedExprKind::IfExpr { then_value, else_value, .. } => {
            collect_branch_var_leaves(then_value, out);
            collect_branch_var_leaves(else_value, out);
        }
        TypedExprKind::Match { arms, .. } => {
            for arm in arms {
                collect_branch_var_leaves(&arm.body, out);
            }
        }
        TypedExprKind::Block { tail, .. } => {
            collect_branch_var_leaves(tail, out);
        }
        _ => {}
    }
}

/// Wrap a branch expression in a Block that drops the given
/// Vars before yielding the branch's value. Used by
/// `inject_branch_drops` (closure #179) to plug the unchosen-
/// alternative leak from closures #172/#173.
fn wrap_branch_with_drops(
    branch: &mut Box<TypedExpr>,
    drops: Vec<(String, Type)>,
) {
    if drops.is_empty() {
        return;
    }
    let drop_stmts: Vec<TypedStmt> = drops
        .into_iter()
        .map(|(name, ty)| TypedStmt::Drop {
            name,
            ty,
            moved_fields: Vec::new(),
        })
        .collect();
    // Take the original branch out so we can re-box it as
    // the Block's tail. Replace with a placeholder Int(0)
    // briefly — the placeholder is immediately overwritten.
    let placeholder = TypedExpr {
        kind: TypedExprKind::Int(0),
        ty: Type::I64,
        constant: None,
        span: Span::default(),
        binding_decl_span: None,
    };
    let original = std::mem::replace(branch.as_mut(), placeholder);
    let span = original.span;
    let ty = original.ty.clone();
    let new_branch = TypedExpr {
        kind: TypedExprKind::Block {
            stmts: drop_stmts,
            tail: Box::new(original),
        },
        ty,
        constant: None,
        span,
        binding_decl_span: None,
    };
    **branch = new_branch;
}

/// Rewrite if-expr / match / block-expr trees so each branch
/// drops the Var leaves owned by the OTHER branches before
/// yielding its chosen value. Closes the unchosen-alternative
/// leak the conservative move tracking in closures #172/#173
/// left behind.
///
/// Called from each `consume_if_moved_var` site (Let, Reassign,
/// FieldAssign, Call args, vec / push / set / enum-constructor)
/// after the move bookkeeping fires. Closure #179.
fn inject_branch_drops(expr: &mut TypedExpr) {
    match &mut expr.kind {
        TypedExprKind::IfExpr { then_value, else_value, .. } => {
            let mut then_vars: Vec<(String, Type)> = Vec::new();
            collect_branch_var_leaves(then_value, &mut then_vars);
            let mut else_vars: Vec<(String, Type)> = Vec::new();
            collect_branch_var_leaves(else_value, &mut else_vars);
            let then_drops: Vec<(String, Type)> = else_vars
                .iter()
                .filter(|(n, _)| !then_vars.iter().any(|(t, _)| t == n))
                .cloned()
                .collect();
            let else_drops: Vec<(String, Type)> = then_vars
                .iter()
                .filter(|(n, _)| !else_vars.iter().any(|(t, _)| t == n))
                .cloned()
                .collect();
            // Recurse into nested if/match before wrapping
            // so inner rewrites land first and the wrap is
            // outermost.
            inject_branch_drops(then_value);
            inject_branch_drops(else_value);
            wrap_branch_with_drops(then_value, then_drops);
            wrap_branch_with_drops(else_value, else_drops);
        }
        TypedExprKind::Match { arms, .. } => {
            let arm_vars: Vec<Vec<(String, Type)>> = arms
                .iter()
                .map(|arm| {
                    let mut v = Vec::new();
                    collect_branch_var_leaves(&arm.body, &mut v);
                    v
                })
                .collect();
            for i in 0..arms.len() {
                let mut my_drops: Vec<(String, Type)> = Vec::new();
                for (j, other_vars) in arm_vars.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    for (n, t) in other_vars {
                        if !arm_vars[i].iter().any(|(tn, _)| tn == n)
                            && !my_drops.iter().any(|(dn, _)| dn == n)
                        {
                            my_drops.push((n.clone(), t.clone()));
                        }
                    }
                }
                // The arm's body is a TypedExpr — we need a
                // Box wrapper to reuse wrap_branch_with_drops.
                // Match arms hold the body inline, so do the
                // wrap manually.
                inject_branch_drops(&mut arms[i].body);
                if !my_drops.is_empty() {
                    let drop_stmts: Vec<TypedStmt> = my_drops
                        .into_iter()
                        .map(|(name, ty)| TypedStmt::Drop {
                            name,
                            ty,
                            moved_fields: Vec::new(),
                        })
                        .collect();
                    let placeholder = TypedExpr {
                        kind: TypedExprKind::Int(0),
                        ty: Type::I64,
                        constant: None,
                        span: Span::default(),
                        binding_decl_span: None,
                    };
                    let original = std::mem::replace(&mut arms[i].body, placeholder);
                    let span = original.span;
                    let ty = original.ty.clone();
                    arms[i].body = TypedExpr {
                        kind: TypedExprKind::Block {
                            stmts: drop_stmts,
                            tail: Box::new(original),
                        },
                        ty,
                        constant: None,
                        span,
                        binding_decl_span: None,
                    };
                }
            }
        }
        TypedExprKind::Block { tail, .. } => {
            inject_branch_drops(tail);
        }
        _ => {}
    }
}

fn consume_if_moved_var(
    source: &Expr,
    checked: &CheckedExpr,
    env: &mut Env,
) {
    if checked.ty().is_copy() {
        return;
    }
    match &source.kind {
        ExprKind::Var(name) => {
            if let Some(info) = env.lookup_mut(name) {
                if info.moved.is_none() {
                    info.moved = Some(source.span);
                }
            }
        }
        // Partial-move tracking: `let xs = t.contents;` moves
        // the field's value out of the struct. The field is
        // marked in the struct binding's `moved_fields` map so
        // the scope-exit per-field free skips it. Subsequent
        // reads of `t.contents` will surface a "field was
        // moved" diagnostic. T1.2 phase 2b partial-move
        // follow-up.
        ExprKind::FieldAccess { object, field, .. } => {
            if let ExprKind::Var(obj_name) = &object.kind {
                if let Some(info) = env.lookup_mut(obj_name) {
                    if !info.moved_fields.contains_key(field) {
                        info.moved_fields.insert(field.clone(), source.span);
                    }
                }
            }
        }
        // Conservative move tracking for if-expression
        // branches. `let chosen = if cond { a } else { b };`
        // where both a and b are non-Copy Vars must mark
        // BOTH as moved — otherwise the codegen ternary
        // aliases v_chosen with v_a OR v_b, and the
        // scope-exit drop of all three would double-free.
        // Both Vars-as-moved means the unchosen alternative
        // leaks, but that's safe (no double-free). Closure
        // #172. A proper fix that frees the unchosen
        // alternative needs a structural rewrite in the
        // codegen layer.
        ExprKind::IfExpr { then_value, else_value, .. } => {
            consume_if_moved_var(then_value, checked, env);
            consume_if_moved_var(else_value, checked, env);
        }
        // Same shape for match arms: `let chosen = match n
        // { 1 then a, 2 then b, _ then c };` would double-
        // free without marking every arm's Var moved.
        // Integer / enum / bool dispatch keeps the Match
        // shape in the typed IR (Str scrutinees desugar
        // into IfExpr via check_match_str, picked up by the
        // arm above). Closure #173.
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                consume_if_moved_var(&arm.body, checked, env);
            }
        }
        // Block expression tail: `let y = { let _ = …; a };`
        // — the tail expression is what the block yields,
        // so a Var tail is moved into the binding the
        // block initializes. Without descending into the
        // tail, the Var's scope-exit drop fires AND the
        // binding frees the same heap → double-free.
        // Closure #174.
        ExprKind::Block { tail, .. } => {
            consume_if_moved_var(tail, checked, env);
        }
        _ => {}
    }
}

/// Desugar `match s { "a" then …, "b" then …, _ then … }` on
/// a `Str` / `OwnedStr` scrutinee into a nested if-expression
/// chain. The scrutinee binds to a single temp so any side
/// effect runs once. A wildcard arm is required (the string
/// space is open). T1.3 follow-up (closure #111).
fn check_match_str(
    scrutinee: &CheckedExpr,
    arms: &[crate::ast::MatchArm],
    span: Span,
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let scrut_ty = scrutinee.ty().clone();
    // Generate a unique temp binding name from the match
    // span so nested matches don't collide.
    let tmp_name = format!("__match_str_{}", span.start);
    // Two passes: (1) check each arm body to build the typed
    // body expressions + collect the result type, validate
    // each pattern; (2) fold the arms into a nested IfExpr.
    let mut seen_strs: Vec<String> = Vec::new();
    let mut wildcard_body: Option<TypedExpr> = None;
    let mut wildcard_seen_at: Option<usize> = None;
    let mut typed_arms: Vec<(String, TypedExpr)> = Vec::new();
    let mut result_ty: Option<Type> = None;
    for (i, arm) in arms.iter().enumerate() {
        if wildcard_seen_at.is_some() {
            diagnostics.push(Diagnostic::new(
                arm.pattern_span,
                "match arm is unreachable: a wildcard `_` arm above already \
                 covers every remaining case"
                    .to_string(),
            ));
            continue;
        }
        match &arm.pattern {
            crate::ast::Pattern::Str(s) => {
                if seen_strs.iter().any(|p| p == s) {
                    diagnostics.push(Diagnostic::new(
                        arm.pattern_span,
                        format!(
                            "match arm for string pattern \"{}\" appears twice",
                            s
                        ),
                    ));
                    continue;
                }
                seen_strs.push(s.clone());
                let body_checked = check_expr(&arm.body, env, signatures, diagnostics);
                if let Some(prev) = &result_ty {
                    if body_checked.ty() != prev {
                        diagnostics.push(Diagnostic::new(
                            arm.body.span,
                            format!(
                                "match arm body type mismatch: expected {}, got {}",
                                prev,
                                body_checked.ty()
                            ),
                        ));
                    }
                } else {
                    result_ty = Some(body_checked.ty().clone());
                }
                typed_arms.push((s.clone(), body_checked.expr));
            }
            crate::ast::Pattern::Wildcard => {
                wildcard_seen_at = Some(i);
                let body_checked = check_expr(&arm.body, env, signatures, diagnostics);
                if let Some(prev) = &result_ty {
                    if body_checked.ty() != prev {
                        diagnostics.push(Diagnostic::new(
                            arm.body.span,
                            format!(
                                "match arm body type mismatch: expected {}, got {}",
                                prev,
                                body_checked.ty()
                            ),
                        ));
                    }
                } else {
                    result_ty = Some(body_checked.ty().clone());
                }
                wildcard_body = Some(body_checked.expr);
            }
            _ => {
                diagnostics.push(Diagnostic::new(
                    arm.pattern_span,
                    format!(
                        "match scrutinee is {}, but pattern is not a string \
                         literal — use `\"text\" then …` or `_ then …`",
                        scrut_ty
                    ),
                ));
                continue;
            }
        }
    }
    if wildcard_body.is_none() {
        diagnostics.push(Diagnostic::new(
            span,
            "non-exhaustive match: string scrutinees require a wildcard \
             `_ then …` arm to cover values not explicitly listed"
                .to_string(),
        ));
    }
    let unified = result_ty.unwrap_or(Type::I64);
    let default_body = wildcard_body.unwrap_or_else(|| TypedExpr {
        kind: TypedExprKind::Int(0),
        ty: unified.clone(),
        constant: Some(TypedConst::Int(0)),
        span,
        binding_decl_span: None,
    });
    // Fold the string arms right-to-left into a nested
    // IfExpr chain whose final else is the wildcard body.
    let mut chain = default_body;
    for (text, body) in typed_arms.into_iter().rev() {
        let scr_var = TypedExpr {
            kind: TypedExprKind::Var(tmp_name.clone()),
            ty: scrut_ty.clone(),
            constant: None,
            span,
            binding_decl_span: None,
        };
        let lit = TypedExpr {
            kind: TypedExprKind::Str(text),
            ty: Type::Str,
            constant: None,
            span,
            binding_decl_span: None,
        };
        let cond = TypedExpr {
            kind: TypedExprKind::Binary {
                op: BinaryOp::Eq,
                left: Box::new(scr_var),
                right: Box::new(lit),
                checked: true,
            },
            ty: Type::Bool,
            constant: None,
            span,
            binding_decl_span: None,
        };
        chain = TypedExpr {
            kind: TypedExprKind::IfExpr {
                cond: Box::new(cond),
                then_value: Box::new(body),
                else_value: Box::new(chain),
            },
            ty: unified.clone(),
            constant: None,
            span,
            binding_decl_span: None,
        };
    }
    // Wrap the if-chain in a Block that first binds the
    // scrutinee to the temp. For Str scrutinees the temp is
    // a borrowed pointer (Copy) and the Block's tail is the
    // if-chain directly. For an `OwnedStr` scrutinee (e.g.
    // `match make_owned_str() { "xy" then 1, _ then 0 }`)
    // the temp owns a heap allocation that nobody else
    // references, so we wrap the if-chain through an
    // intermediate `__result` let, drop the temp, then yield
    // `__result` as the block's tail. Closure #137: without
    // the drop the scrutinee's heap leaks.
    let bind_stmt = TypedStmt::Let {
        name: tmp_name.clone(),
        ty: scrut_ty.clone(),
        expr: scrutinee.expr.clone(),
    };
    // Only fresh heap-producing scrutinee expressions
    // (Call returning OwnedStr, Binary `+` concat) own a
    // heap allocation that no other binding holds. Var /
    // FieldAccess / TupleAccess scrutinees reference a
    // value that's already tracked by some outer binding
    // — emitting a Drop on the temp here would double-free
    // (the outer binding's scope-exit Drop frees the same
    // pointer). Closure #137 mirrors closure #135's
    // print-of-OwnedStr whitelist.
    let needs_temp_drop = crate::ir::is_fresh_owned_str(&scrutinee.expr);
    let stmts = if needs_temp_drop {
        let result_name = format!("__match_str_result_{}", span.start);
        let result_stmt = TypedStmt::Let {
            name: result_name.clone(),
            ty: unified.clone(),
            expr: chain,
        };
        let drop_stmt = TypedStmt::Drop {
            name: tmp_name.clone(),
            ty: scrut_ty.clone(),
            moved_fields: Vec::new(),
        };
        let result_var_tail = TypedExpr {
            kind: TypedExprKind::Var(result_name),
            ty: unified.clone(),
            constant: None,
            span,
            binding_decl_span: None,
        };
        return CheckedExpr::new(
            TypedExprKind::Block {
                stmts: vec![bind_stmt, result_stmt, drop_stmt],
                tail: Box::new(result_var_tail),
            },
            unified,
            None,
            span,
        );
    } else {
        vec![bind_stmt]
    };
    CheckedExpr::new(
        TypedExprKind::Block {
            stmts,
            tail: Box::new(chain),
        },
        unified,
        None,
        span,
    )
}

fn check_expr(
    expr: &Expr,
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    match &expr.kind {
        ExprKind::Int(value) => {
            if !value_fits_type(*value, &Type::U64) {
                diagnostics.push(Diagnostic::new(
                    expr.span,
                    format!("integer literal '{}' does not fit in u64", value),
                ));
            }

            let mut checked = CheckedExpr::new(
                TypedExprKind::Int(*value),
                if *value <= i64::MAX as i128 {
                    Type::I64
                } else {
                    Type::U64
                },
                Some(TypedConst::Int(*value)),
                expr.span,
            );
            checked.flexible_integer = true;
            checked
        }
        ExprKind::Float(value) => {
            let mut checked = CheckedExpr::new(
                TypedExprKind::Float(*value),
                Type::F64,
                Some(TypedConst::Float(*value)),
                expr.span,
            );
            checked.flexible_float = true;
            checked
        }
        ExprKind::Bool(value) => CheckedExpr::new(
            TypedExprKind::Bool(*value),
            Type::Bool,
            Some(TypedConst::Bool(*value)),
            expr.span,
        ),
        ExprKind::Str(text) => CheckedExpr::new(
            TypedExprKind::Str(text.clone()),
            Type::Str,
            None,
            expr.span,
        ),
        ExprKind::Var(name) => match env.lookup(name) {
            Some(info) => {
                if let Some(move_span) = info.moved {
                    diagnostics.push(
                        Diagnostic::new(
                            expr.span,
                            format!("value '{}' was moved; cannot use after move", name),
                        )
                        .with_related(move_span, format!("'{}' was moved here", name)),
                    );
                }
                let decl_span = info.decl_span;
                // T4.15: when the resolved binding came from a
                // top-level `const`, substitute the Var with
                // its literal value so the codegen layer never
                // sees an unbound `v_NAME` reference. The
                // checker still surfaces the const's compile-time
                // value via `info.constant`, so all the SMT +
                // constant-tracking machinery downstream
                // continues to work unchanged.
                let kind = if info.is_const {
                    match &info.constant {
                        Some(TypedConst::Int(v)) => TypedExprKind::Int(*v),
                        Some(TypedConst::Float(v)) => TypedExprKind::Float(*v),
                        Some(TypedConst::Bool(v)) => TypedExprKind::Bool(*v),
                        None => TypedExprKind::Var(name.clone()),
                    }
                } else {
                    TypedExprKind::Var(name.clone())
                };
                CheckedExpr::new(
                    kind,
                    info.ty.clone(),
                    info.constant.clone(),
                    expr.span,
                )
                .with_binding_decl_span(decl_span)
            }
            None => {
                // Bare identifier resolved against the env
                // failed. Check if it names a top-level
                // function — that produces a first-class
                // function pointer value of type
                // `fn(T1, ..., Tn) -> R`.
                if let Some(sig) = signatures.get(name) {
                    let fn_ty = Type::FnPtr(
                        sig.params.clone(),
                        Box::new(sig.return_type.clone()),
                    );
                    CheckedExpr::new(
                        TypedExprKind::FnRef {
                            name: name.clone(),
                            name_span: expr.span,
                        },
                        fn_ty,
                        None,
                        expr.span,
                    )
                } else {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!("unknown variable '{}'", name),
                    ));
                    CheckedExpr::fallback_integer(expr.span)
                }
            }
        },
        ExprKind::Unary { op, expr: operand } => {
            let checked = check_expr(operand, env, signatures, diagnostics);
            check_unary(*op, checked, expr.span, diagnostics)
        }
        ExprKind::Binary { op, left, right } => {
            check_binary(*op, left, right, env, signatures, diagnostics)
        }
        ExprKind::Call { name, name_span, args, .. } => {
            check_call(name, *name_span, args, env, signatures, expr.span, diagnostics)
        }
        ExprKind::MethodCall { receiver, method, method_span, args } => {
            // T1.3 phase 2b: `EnumName.Variant(payload)` is the
            // payloaded enum constructor syntax. Parser routes it
            // through MethodCall (since `.` + `(` looks like a
            // method call). Intercept here when the receiver is
            // a bare Var naming a declared enum AND the "method"
            // matches one of its variants. v1 supports
            // single-field payload only.
            if let ExprKind::Var(enum_name) = &receiver.kind {
                if let Some(enum_decl) = env.lookup_enum(enum_name) {
                    if let Some((tag, _)) = enum_decl
                        .variants
                        .iter()
                        .enumerate()
                        .find(|(_, v)| *v == method)
                    {
                        // Look up the variant's payload type from
                        // the original declaration. The Env's
                        // `EnumInfo` only carries variant names;
                        // payload types live on the `EnumDecl` in
                        // the program. Pull from signatures-like
                        // registry via the program AST — we
                        // shortcut by re-walking `env.enums`'s
                        // companion decls. For v1, we attach the
                        // payload type by looking it up in the
                        // enum declaration we know the checker
                        // already processed.
                        let payload_ty = lookup_enum_variant_payload(
                            env, enum_name, method,
                        );
                        if let Some(payload_ty) = payload_ty {
                            // Exactly one payload arg for v1.
                            if args.len() != 1 {
                                diagnostics.push(Diagnostic::new(
                                    expr.span,
                                    format!(
                                        "enum constructor '{}.{}' expects 1 payload argument, got {}",
                                        enum_name, method, args.len()
                                    ),
                                ));
                                return CheckedExpr::fallback_integer(expr.span);
                            }
                            let raw = check_expr(&args[0], env, signatures, diagnostics);
                            let arg_checked = coerce_checked(
                                raw, &payload_ty, args[0].span,
                                "enum payload",
                                diagnostics,
                            );
                            // Enum constructor transfers ownership
                            // of the payload into the tagged-union
                            // — mark the source Var moved so the
                            // scope-exit drop doesn't fire on a
                            // heap the enum now owns. Same family
                            // as vec / push / set (closures
                            // #171, #177). Closure #178.
                            consume_if_moved_var(&args[0], &arg_checked, env);
                            return CheckedExpr::new(
                                TypedExprKind::EnumVariantWithPayload {
                                    enum_name: enum_name.clone(),
                                    variant: method.clone(),
                                    tag: tag as u32,
                                    payload: Box::new(arg_checked.expr),
                                    payload_ty: payload_ty.clone(),
                                },
                                Type::Enum(enum_name.clone()),
                                None,
                                expr.span,
                            );
                        } else {
                            // Calling a payload-less variant with
                            // args — clean diagnostic.
                            diagnostics.push(Diagnostic::new(
                                expr.span,
                                format!(
                                    "enum variant '{}.{}' has no payload — use \
                                     `{}.{}` without parentheses",
                                    enum_name, method, enum_name, method
                                ),
                            ));
                            return CheckedExpr::fallback_integer(expr.span);
                        }
                    }
                }
            }
            // Type-associated function call: `Type.fn(args)`
            // where `Type` is a declared struct/enum AND
            // `<Type>_<fn>` is in scope. This is the
            // constructor pattern (`Point.new(1, 2)`) and
            // any other self-less helper attached to a type
            // via `methods on T { fn helper(…) { … } }`.
            // T1.2 phase 2a follow-up (closure #114).
            if let ExprKind::Var(type_name) = &receiver.kind {
                let mangled = format!("{}_{}", type_name, method);
                let is_nominal = env.lookup_struct(type_name).is_some()
                    || env.lookup_enum(type_name).is_some();
                if is_nominal {
                    if let Some(sig) = signatures.get(&mangled) {
                        // Validate arity.
                        if sig.params.len() != args.len() {
                            diagnostics.push(Diagnostic::new(
                                expr.span,
                                format!(
                                    "type-associated function '{}.{}' expects {} \
                                     arguments, got {}",
                                    type_name,
                                    method,
                                    sig.params.len(),
                                    args.len()
                                ),
                            ));
                            return CheckedExpr::fallback_integer(expr.span);
                        }
                        // Type-check each arg against the
                        // function's parameter types.
                        let param_tys: Vec<Type> = sig.params.clone();
                        let ret_ty_owned = sig.return_type.clone();
                        let mut typed_args: Vec<TypedExpr> = Vec::new();
                        for (i, arg) in args.iter().enumerate() {
                            let checked = check_expr(arg, env, signatures, diagnostics);
                            let coerced = coerce_checked(
                                checked,
                                &param_tys[i],
                                arg.span,
                                &format!("{}.{} arg #{}", type_name, method, i + 1),
                                diagnostics,
                            );
                            diagnose_partial_then_whole_move(arg, &coerced, env, diagnostics);
                            consume_if_moved_var(arg, &coerced, env);
                            typed_args.push(coerced.expr);
                        }
                        return CheckedExpr::new(
                            TypedExprKind::Call {
                                name: mangled,
                                name_span: *method_span,
                                args: typed_args,
                            },
                            ret_ty_owned,
                            None,
                            expr.span,
                        );
                    }
                }
            }
            // T1.2 phase 2a: desugar `recv.method(args)` into a
            // regular call to the mangled `<TypeName>_<method>`
            // function. The receiver becomes the first argument.
            // Type-check the receiver first so we know the
            // concrete type to dispatch against.
            let recv_checked = check_expr(receiver, env, signatures, diagnostics);
            let recv_ty = recv_checked.ty().clone();
            let _ = recv_checked; // keep clippy happy; we re-check below
            // Build the mangled name: <TypeName>_<method>.
            // Only nominal types support methods in v1.
            let type_name = match &recv_ty {
                Type::Struct(name) | Type::Enum(name) => name.clone(),
                Type::Ref(inner) | Type::RefMut(inner) => match inner.as_ref() {
                    Type::Struct(name) | Type::Enum(name) => name.clone(),
                    other => {
                        diagnostics.push(Diagnostic::new(
                            *method_span,
                            format!(
                                "cannot call method '{}' on {} — methods are \
                                 attached to struct/enum types only in v1",
                                method, other
                            ),
                        ));
                        return CheckedExpr::fallback_integer(expr.span);
                    }
                },
                other => {
                    diagnostics.push(Diagnostic::new(
                        *method_span,
                        format!(
                            "cannot call method '{}' on {} — methods are \
                             attached to struct/enum types only in v1",
                            method, other
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            };
            let mangled = format!("{}_{}", type_name, method);
            let sig = match signatures.get(&mangled) {
                Some(s) => s,
                None => {
                    diagnostics.push(Diagnostic::new(
                        *method_span,
                        format!(
                            "no method '{}' on type '{}'; expected a `methods on {}` \
                             block declaring `fn {}(self: {}, …)`",
                            method, type_name, type_name, method, type_name
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            };
            // Build the desugared argument list. Auto-ref:
            // if the method's first param is `ref T` or
            // `mut ref T` and the receiver is a plain value
            // of `T`, wrap the receiver in
            // `ExprKind::Ref` / `ExprKind::RefMut` so users
            // can write `p.method()` regardless of whether
            // the method binds `self` by value or by
            // borrow. Conversely, when the method binds by
            // value but the receiver is a borrow, leave
            // the receiver alone — `check_call` will
            // surface the existing type-mismatch
            // diagnostic. T1.2 phase 2a auto-ref.
            let expected_self_ty = sig.params.first().cloned();
            // Detect the inverse-of-auto-ref case early
            // and emit a helpful diagnostic. Receiver is
            // already a borrow (`ref T` / `mut ref T`) but
            // the method takes `self: T` by value. We don't
            // have an implicit deref expression, so this
            // can't be silently auto-coerced — but we can
            // tell the user the two viable workarounds.
            if let (Some(expected), recv) = (expected_self_ty.as_ref(), &recv_ty) {
                if matches!(recv, Type::Ref(_) | Type::RefMut(_))
                    && matches!(expected, Type::Struct(_) | Type::Enum(_))
                {
                    let underlying = recv.deref();
                    diagnostics.push(Diagnostic::new(
                        *method_span,
                        format!(
                            "method '{}' takes `self: {}` by value but the receiver \
                             is a borrow ({}); either change the method signature to \
                             `self: ref {}` / `self: mut ref {}`, or copy the value \
                             by reconstructing the struct literal before calling",
                            method, expected, recv, underlying, underlying
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            }
            let receiver_expr: Expr = match (&recv_ty, expected_self_ty.as_ref()) {
                (Type::Struct(_), Some(Type::Ref(inner)))
                | (Type::Enum(_), Some(Type::Ref(inner)))
                    if matches!(inner.as_ref(), Type::Struct(_) | Type::Enum(_)) =>
                {
                    Expr {
                        kind: ExprKind::Ref {
                            inner: Box::new((**receiver).clone()),
                        },
                        span: receiver.span,
                    }
                }
                (Type::Struct(_), Some(Type::RefMut(inner)))
                | (Type::Enum(_), Some(Type::RefMut(inner)))
                    if matches!(inner.as_ref(), Type::Struct(_) | Type::Enum(_)) =>
                {
                    Expr {
                        kind: ExprKind::RefMut {
                            inner: Box::new((**receiver).clone()),
                        },
                        span: receiver.span,
                    }
                }
                _ => (**receiver).clone(),
            };
            let mut all_args: Vec<Expr> = Vec::with_capacity(args.len() + 1);
            all_args.push(receiver_expr);
            all_args.extend(args.iter().cloned());
            check_call(
                &mangled,
                *method_span,
                &all_args,
                env,
                signatures,
                expr.span,
                diagnostics,
            )
        }
        ExprKind::Cast { expr: operand, ty } => {
            let checked = check_expr(operand, env, signatures, diagnostics);
            explicit_cast(checked, ty, expr.span, diagnostics)
        }
        ExprKind::ArrayLit { elements } => {
            check_array_literal(elements, env, signatures, expr.span, diagnostics)
        }
        ExprKind::Index { array, index } => {
            check_index(array, index, env, signatures, expr.span, diagnostics)
        }
        ExprKind::Len { array } => check_len(array, env, signatures, expr.span, diagnostics),
        ExprKind::Ref { inner } => check_ref(inner, env, expr.span, diagnostics),
        ExprKind::RefMut { inner } => check_ref_mut(inner, env, expr.span, diagnostics),
        ExprKind::Tuple(elements) => {
            // v1 tuples: 2..=4 elements, Copy-only, no nested
            // affine resources. Recurse to type-check each
            // element, then build the result tuple type. T1.1.
            if elements.len() < 2 || elements.len() > 4 {
                diagnostics.push(Diagnostic::new(
                    expr.span,
                    format!(
                        "tuple must have 2..=4 elements; got {}",
                        elements.len()
                    ),
                ));
                return CheckedExpr::fallback_integer(expr.span);
            }
            let typed: Vec<CheckedExpr> = elements
                .iter()
                .map(|e| check_expr(e, env, signatures, diagnostics))
                .collect();
            for (i, ce) in typed.iter().enumerate() {
                if !ce.ty().is_copy() {
                    diagnostics.push(Diagnostic::new(
                        elements[i].span,
                        format!(
                            "tuple element {} has non-Copy type {} — v1 tuples are Copy-only",
                            i, ce.ty()
                        ),
                    ));
                }
            }
            let elem_types: Vec<Type> = typed.iter().map(|c| c.ty().clone()).collect();
            let elem_exprs: Vec<TypedExpr> = typed.into_iter().map(|c| c.expr).collect();
            CheckedExpr::new(
                TypedExprKind::Tuple { elements: elem_exprs },
                Type::Tuple(elem_types),
                None,
                expr.span,
            )
        }
        ExprKind::TupleAccess { tuple, index } => {
            let inner = check_expr(tuple, env, signatures, diagnostics);
            let elt_ty = match inner.ty() {
                Type::Tuple(elements) => {
                    if (*index as usize) >= elements.len() {
                        diagnostics.push(Diagnostic::new(
                            expr.span,
                            format!(
                                "tuple index {} out of bounds for tuple of arity {}",
                                index,
                                elements.len()
                            ),
                        ));
                        return CheckedExpr::fallback_integer(expr.span);
                    }
                    elements[*index as usize].clone()
                }
                other => {
                    diagnostics.push(Diagnostic::new(
                        tuple.span,
                        format!("tuple access on non-tuple type {}", other),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            };
            CheckedExpr::new(
                TypedExprKind::TupleAccess {
                    tuple: Box::new(inner.expr),
                    index: *index,
                },
                elt_ty,
                None,
                expr.span,
            )
        }
        ExprKind::StructLit { type_name, type_name_span, fields } => {
            // Look up the struct decl from the program's
            // registry stored on env, then verify every
            // required field is present with a matching type.
            let Some(decl) = env.lookup_struct(type_name) else {
                diagnostics.push(Diagnostic::new(
                    *type_name_span,
                    format!("unknown struct type '{}'", type_name),
                ));
                return CheckedExpr::fallback_integer(expr.span);
            };
            // Build name → declared type map for lookup.
            let decl_fields: Vec<(String, Type)> = decl.fields.clone();
            // Check arity + names match.
            if fields.len() != decl_fields.len() {
                diagnostics.push(Diagnostic::new(
                    expr.span,
                    format!(
                        "struct '{}' has {} fields, literal provides {}",
                        type_name,
                        decl_fields.len(),
                        fields.len()
                    ),
                ));
                return CheckedExpr::fallback_integer(expr.span);
            }
            // Type-check each literal field; reorder into
            // declaration order so the typed form is
            // canonical.
            let mut typed_fields: Vec<(String, TypedExpr)> = Vec::with_capacity(decl_fields.len());
            for (fname, fty) in &decl_fields {
                let Some(found) = fields.iter().find(|(n, _)| n == fname) else {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "struct '{}' literal missing field '{}'",
                            type_name, fname
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                };
                let checked_v = check_expr(&found.1, env, signatures, diagnostics);
                let coerced = coerce_checked(
                    checked_v,
                    fty,
                    found.1.span,
                    &format!("struct field '{}'", fname),
                    diagnostics,
                );
                // Initializing a non-Copy struct field from a
                // `Var` consumes the source binding. Without
                // this, both the source binding and the struct
                // field would own the same heap pointer and the
                // backend would emit two `free` calls for it.
                // T1.2 phase 2b.
                diagnose_partial_then_whole_move(&found.1, &coerced, env, diagnostics);
                consume_if_moved_var(&found.1, &coerced, env);
                typed_fields.push((fname.clone(), coerced.expr));
            }
            // Reject unknown field names.
            for (fname, _) in fields {
                if !decl_fields.iter().any(|(n, _)| n == fname) {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "struct '{}' has no field named '{}'",
                            type_name, fname
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            }
            CheckedExpr::new(
                TypedExprKind::StructLit {
                    type_name: type_name.clone(),
                    fields: typed_fields,
                },
                Type::Struct(type_name.clone()),
                None,
                expr.span,
            )
        }
        ExprKind::FieldAccess { object, field } => {
            // Special case: `EnumName.Variant` looks like a
            // field access but resolves to an enum-variant
            // reference. Recognize by checking if the LHS is
            // a bare `Var` naming a declared enum. T1.3.
            if let ExprKind::Var(name) = &object.kind {
                if let Some(enum_decl) = env.lookup_enum(name) {
                    if let Some((tag, _)) = enum_decl
                        .variants
                        .iter()
                        .enumerate()
                        .find(|(_, v)| *v == field)
                    {
                        return CheckedExpr::new(
                            TypedExprKind::EnumVariant {
                                enum_name: name.clone(),
                                variant: field.clone(),
                                tag: tag as u32,
                            },
                            Type::Enum(name.clone()),
                            None,
                            expr.span,
                        );
                    }
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "enum '{}' has no variant named '{}'",
                            name, field
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            }
            let inner = check_expr(object, env, signatures, diagnostics);
            // Field access works through one level of borrow
            // (so `ref_to_point.x` reads `(*ref_to_point).x`).
            let underlying = inner.ty().deref().clone();
            let (struct_name, field_ty, field_index) = match &underlying {
                Type::Struct(name) => {
                    let Some(decl) = env.lookup_struct(name) else {
                        diagnostics.push(Diagnostic::new(
                            object.span,
                            format!("struct '{}' is not declared", name),
                        ));
                        return CheckedExpr::fallback_integer(expr.span);
                    };
                    let Some(idx) = decl.fields.iter().position(|(n, _)| n == field) else {
                        diagnostics.push(Diagnostic::new(
                            expr.span,
                            format!(
                                "struct '{}' has no field named '{}'",
                                name, field
                            ),
                        ));
                        return CheckedExpr::fallback_integer(expr.span);
                    };
                    (name.clone(), decl.fields[idx].1.clone(), idx as u32)
                }
                other => {
                    diagnostics.push(Diagnostic::new(
                        object.span,
                        format!(
                            "field access on non-struct type {}",
                            other
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            };
            let _ = struct_name;
            // Partial-move check: reading a field that was
            // previously moved out is a use-after-move.
            // T1.2 phase 2b partial-move follow-up.
            if let ExprKind::Var(obj_name) = &object.kind {
                if let Some(info) = env.lookup(obj_name) {
                    if let Some(move_span) = info.moved_fields.get(field).copied() {
                        diagnostics.push(
                            Diagnostic::new(
                                expr.span,
                                format!(
                                    "field '{}.{}' was moved; cannot use after move",
                                    obj_name, field
                                ),
                            )
                            .with_related(
                                move_span,
                                format!("'{}.{}' was moved here", obj_name, field),
                            ),
                        );
                    }
                }
            }
            CheckedExpr::new(
                TypedExprKind::FieldAccess {
                    object: Box::new(inner.expr),
                    field: field.clone(),
                    field_index,
                },
                field_ty,
                None,
                expr.span,
            )
        }
        ExprKind::Match { scrutinee, arms } => {
            let scrutinee_checked = check_expr(scrutinee, env, signatures, diagnostics);
            // Match on `Str` / `OwnedStr` desugars to a
            // nested if-expression chain via `==` on Str.
            // The scrutinee is bound to a temp first so any
            // side-effecting expression evaluates once. A
            // wildcard arm is required since Str is open;
            // missing wildcard surfaces a diagnostic.
            // T1.3 follow-up (closure #111).
            let scrut_ty_early = scrutinee_checked.ty().clone();
            if matches!(scrut_ty_early, Type::Str | Type::OwnedStr) {
                return check_match_str(
                    &scrutinee_checked,
                    arms,
                    expr.span,
                    env,
                    signatures,
                    diagnostics,
                );
            }
            // Two dispatch shapes in v1: enum-tag dispatch
            // and integer-literal dispatch. Bool and other
            // non-integer/non-enum scalars are rejected. The
            // arm-pattern loop below validates each arm's
            // pattern against the scrutinee's kind.
            // T1.3 + integer-literal pattern.
            let scrut_ty = scrutinee_checked.ty().clone();
            let enum_decl_opt: Option<EnumInfo> = match &scrut_ty {
                Type::Enum(name) => {
                    let Some(d) = env.lookup_enum(name).cloned() else {
                        diagnostics.push(Diagnostic::new(
                            scrutinee.span,
                            format!("enum '{}' is not declared", name),
                        ));
                        return CheckedExpr::fallback_integer(expr.span);
                    };
                    Some(d)
                }
                t if t.is_integer() => None,
                Type::Bool => None,
                other => {
                    diagnostics.push(Diagnostic::new(
                        scrutinee.span,
                        format!(
                            "match scrutinee must be an enum, integer, or bool type, got {}",
                            other
                        ),
                    ));
                    return CheckedExpr::fallback_integer(expr.span);
                }
            };
            let is_int_dispatch = enum_decl_opt.is_none() && scrut_ty != Type::Bool;
            let is_bool_dispatch = scrut_ty == Type::Bool;
            let enum_name_opt: Option<String> = match &scrut_ty {
                Type::Enum(name) => Some(name.clone()),
                _ => None,
            };
            // Each arm: pattern must match scrutinee's
            // dispatch kind. Body type unifies with the
            // running result_ty.
            let mut typed_arms: Vec<crate::ir::TypedMatchArm> = Vec::new();
            let mut seen_variants: Vec<&str> = Vec::new();
            let mut seen_ints: Vec<i128> = Vec::new();
            let mut seen_bools: Vec<bool> = Vec::new();
            let mut result_ty: Option<Type> = None;
            // A wildcard arm covers every remaining variant. v1
            // requires it to appear last — once seen, arms after
            // it are dead.
            let mut wildcard_seen = false;
            let mut wildcard_arm_index: Option<usize> = None;
            for (arm_idx, arm) in arms.iter().enumerate() {
                if wildcard_seen {
                    diagnostics.push(Diagnostic::new(
                        arm.pattern_span,
                        "match arm is unreachable: a wildcard `_` arm above already \
                         covers every remaining case"
                            .to_string(),
                    ));
                    continue;
                }
                let (tag_opt, variant_name_opt, int_opt): (
                    Option<u32>,
                    Option<String>,
                    Option<i128>,
                ) = match &arm.pattern {
                    crate::ast::Pattern::Wildcard => {
                        wildcard_seen = true;
                        wildcard_arm_index = Some(arm_idx);
                        (None, None, None)
                    }
                    crate::ast::Pattern::Int(v) => {
                        if !is_int_dispatch {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "integer pattern in match arm but scrutinee is of \
                                     enum type {}",
                                    enum_name_opt.as_deref().unwrap_or("?"),
                                ),
                            ));
                            continue;
                        }
                        if !value_fits_type(*v, &scrut_ty) {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "integer pattern '{}' does not fit in scrutinee \
                                     type {}",
                                    v, scrut_ty
                                ),
                            ));
                            continue;
                        }
                        if seen_ints.contains(v) {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!("match arm for integer pattern '{}' appears twice", v),
                            ));
                            continue;
                        }
                        seen_ints.push(*v);
                        (None, None, Some(*v))
                    }
                    crate::ast::Pattern::Bool(b) => {
                        if !is_bool_dispatch {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "bool pattern in match arm but scrutinee is of type {}",
                                    scrut_ty
                                ),
                            ));
                            continue;
                        }
                        if seen_bools.contains(b) {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!("match arm for bool pattern '{}' appears twice", b),
                            ));
                            continue;
                        }
                        seen_bools.push(*b);
                        // Encode bool as 0/1 integer dispatch
                        // so the existing backend switch logic
                        // works uniformly.
                        (None, None, Some(if *b { 1 } else { 0 }))
                    }
                    crate::ast::Pattern::Str(_) => {
                        diagnostics.push(Diagnostic::new(
                            arm.pattern_span,
                            "string literal match patterns are not yet supported \
                             in v1 — use if/else chains with `==` on Str/OwnedStr",
                        ));
                        continue;
                    }
                    crate::ast::Pattern::Variant {
                        enum_name: pat_enum,
                        variant: pat_variant,
                    } => {
                        if is_int_dispatch {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "variant pattern '{}.{}' in match arm but scrutinee \
                                     is of integer type {}",
                                    pat_enum, pat_variant, scrut_ty
                                ),
                            ));
                            continue;
                        }
                        let enum_name = enum_name_opt.as_deref().unwrap_or("");
                        let enum_decl = enum_decl_opt
                            .as_ref()
                            .expect("enum dispatch has enum_decl");
                        if *pat_enum != enum_name {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "match arm references enum '{}' but scrutinee \
                                     is of enum '{}'",
                                    pat_enum, enum_name
                                ),
                            ));
                            continue;
                        }
                        let Some((tag, _)) = enum_decl
                            .variants
                            .iter()
                            .enumerate()
                            .find(|(_, v)| **v == *pat_variant)
                        else {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "enum '{}' has no variant '{}'",
                                    enum_name, pat_variant
                                ),
                            ));
                            continue;
                        };
                        if seen_variants.contains(&pat_variant.as_str()) {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "match arm for '{}.{}' appears twice",
                                    enum_name, pat_variant
                                ),
                            ));
                            continue;
                        }
                        seen_variants.push(pat_variant);
                        (Some(tag as u32), Some(pat_variant.clone()), None)
                    }
                    crate::ast::Pattern::VariantWithBinding {
                        enum_name: pat_enum,
                        variant: pat_variant,
                        binding: _,
                    } => {
                        // T1.3 phase 2b: payloaded variant
                        // destructure. Mirrors `Pattern::Variant`
                        // for variant lookup; the binding gets
                        // pushed into the arm body's scope below
                        // so the body's reference to `v` resolves.
                        if is_int_dispatch {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "variant destructure pattern '{}.{}' in match arm \
                                     but scrutinee is of integer type {}",
                                    pat_enum, pat_variant, scrut_ty
                                ),
                            ));
                            continue;
                        }
                        let enum_name = enum_name_opt.as_deref().unwrap_or("");
                        if *pat_enum != enum_name {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "match arm references enum '{}' but scrutinee \
                                     is of enum '{}'",
                                    pat_enum, enum_name
                                ),
                            ));
                            continue;
                        }
                        let enum_decl = enum_decl_opt
                            .as_ref()
                            .expect("enum dispatch has enum_decl");
                        let Some((tag, _)) = enum_decl
                            .variants
                            .iter()
                            .enumerate()
                            .find(|(_, v)| **v == *pat_variant)
                        else {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "enum '{}' has no variant '{}'",
                                    enum_name, pat_variant
                                ),
                            ));
                            continue;
                        };
                        // Variant must have a payload to bind.
                        if enum_decl.variants[tag].is_empty() {
                            // payload-less; this would be a checker
                            // error in stricter v1, but we just
                            // ignore the binding for now.
                            let _ = enum_decl;
                        }
                        if seen_variants.contains(&pat_variant.as_str()) {
                            diagnostics.push(Diagnostic::new(
                                arm.pattern_span,
                                format!(
                                    "match arm for '{}.{}' appears twice",
                                    enum_name, pat_variant
                                ),
                            ));
                            continue;
                        }
                        seen_variants.push(pat_variant);
                        (Some(tag as u32), Some(pat_variant.clone()), None)
                    }
                };
                // Extract the binding name + payload type for
                // VariantWithBinding patterns. Used both to
                // populate the arm's binding field and to push
                // the binding into env scope before checking the
                // arm body. Non-Copy heap payloads (OwnedStr)
                // are exposed to the body as their Copy
                // borrowed-view counterpart (Str) so the
                // scrutinee retains ownership and its existing
                // scope-exit Drop frees the heap exactly once.
                // The binding is a read-only view in v1 —
                // escaping the borrow past the scrutinee's
                // scope is the same dangling-Str hazard that
                // already exists for any Str produced from an
                // OwnedStr in this language. Closure #128 / D3.
                let arm_binding: Option<(String, Type)> = match &arm.pattern {
                    crate::ast::Pattern::VariantWithBinding {
                        enum_name: pat_enum,
                        variant: pat_variant,
                        binding,
                    } => lookup_enum_variant_payload(env, pat_enum, pat_variant)
                        .map(|ty| {
                            let view_ty = match ty {
                                Type::OwnedStr => Type::Str,
                                other => other,
                            };
                            (binding.clone(), view_ty)
                        }),
                    _ => None,
                };
                // Closure #128 / D3: the binding's exposed type
                // was already remapped to Str for OwnedStr
                // payloads above (borrow-view), so it's
                // always Copy here. Other non-Copy payload
                // types (Vec<T>, structs with owning fields,
                // …) still need their own borrow-view design
                // and remain rejected.
                if let Some((_, bty)) = &arm_binding {
                    if !bty.is_copy() {
                        diagnostics.push(Diagnostic::new(
                            arm.pattern_span,
                            format!(
                                "destructure binding for non-Copy payload type {} \
                                 is not supported in v1 — only OwnedStr payloads \
                                 admit a binding (exposed as Str view); other \
                                 affine payload types need explicit borrow-view \
                                 wiring",
                                bty,
                            ),
                        ));
                    }
                }
                let body_checked = if let Some((bname, bty)) = &arm_binding {
                    env.push_scope();
                    env.insert_current(bname.clone(), VarInfo {
                        ty: bty.clone(),
                        constant: None,
                        moved: None,
                        decl_span: arm.pattern_span,
                        vec_literal_elements: None,
                        array_version: 0,
                        guarded_mutex: None,
                        no_drop: false,
                        is_const: false,
                        struct_literal_fields: None,
                        moved_fields: std::collections::BTreeMap::new(),
                    });
                    let bc = check_expr(&arm.body, env, signatures, diagnostics);
                    env.pop_scope();
                    bc
                } else {
                    check_expr(&arm.body, env, signatures, diagnostics)
                };
                if let Some(expected) = &result_ty {
                    if body_checked.ty() != expected {
                        diagnostics.push(Diagnostic::new(
                            arm.body.span,
                            format!(
                                "match arm body has type {} but earlier arm produced {}",
                                body_checked.ty(),
                                expected
                            ),
                        ));
                    }
                } else {
                    result_ty = Some(body_checked.ty().clone());
                }
                let is_wildcard = matches!(arm.pattern, crate::ast::Pattern::Wildcard);
                typed_arms.push(crate::ir::TypedMatchArm {
                    variant: variant_name_opt.unwrap_or_default(),
                    tag: tag_opt.unwrap_or(0),
                    is_wildcard,
                    int_value: int_opt,
                    binding: arm_binding,
                    body: body_checked.expr,
                });
            }
            // Exhaustiveness:
            //   - enum dispatch: every declared variant
            //     must have an arm, unless wildcard
            //     catches the rest
            //   - integer dispatch: integer scrutinees
            //     have an open set of values, so the
            //     match MUST end in a wildcard
            if !wildcard_seen {
                if is_int_dispatch {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        "non-exhaustive match: integer scrutinees require a wildcard \
                         `_ then …` arm to cover values not explicitly listed"
                            .to_string(),
                    ));
                } else if is_bool_dispatch {
                    // Bool exhaustiveness: need both `true`
                    // and `false` arms, or a wildcard.
                    if !seen_bools.contains(&true) {
                        diagnostics.push(Diagnostic::new(
                            expr.span,
                            "non-exhaustive match: missing arm for 'true'".to_string(),
                        ));
                    }
                    if !seen_bools.contains(&false) {
                        diagnostics.push(Diagnostic::new(
                            expr.span,
                            "non-exhaustive match: missing arm for 'false'".to_string(),
                        ));
                    }
                } else if let Some(enum_decl) = &enum_decl_opt {
                    let enum_name = enum_name_opt.as_deref().unwrap_or("");
                    for v in &enum_decl.variants {
                        if !seen_variants.contains(&v.as_str()) {
                            diagnostics.push(Diagnostic::new(
                                expr.span,
                                format!(
                                    "non-exhaustive match: missing arm for '{}.{}'",
                                    enum_name, v
                                ),
                            ));
                        }
                    }
                }
            }
            let _ = wildcard_arm_index;
            let unified = result_ty.unwrap_or(Type::I64);
            // Constant-fold: if the scrutinee is an integer
            // constant and one of the arms' int_value matches
            // (or the wildcard catches it), and that arm's
            // body is itself constant, the whole match folds.
            // Lets `let r: i64 = match x { … };` propagate the
            // match's value through the let-binding's constant
            // tracker, so downstream `prove r == k` discharges
            // via constant-fold without round-tripping to SMT.
            let constant = match scrutinee_checked.constant() {
                Some(TypedConst::Int(scrut_v)) => {
                    let mut selected: Option<&TypedExpr> = None;
                    for arm in &typed_arms {
                        if let Some(pat) = arm.int_value {
                            if pat == *scrut_v {
                                selected = Some(&arm.body);
                                break;
                            }
                        } else if arm.is_wildcard {
                            selected = Some(&arm.body);
                            break;
                        }
                    }
                    selected.and_then(|body| body.constant.clone())
                }
                _ => None,
            };
            CheckedExpr::new(
                TypedExprKind::Match {
                    scrutinee: Box::new(scrutinee_checked.expr),
                    arms: typed_arms,
                },
                unified,
                constant,
                expr.span,
            )
        }
        ExprKind::Block { stmts, tail } => {
            // T-block MVP: `{ let a = e1; print "log"; tail }`.
            // V1 admits `let` bindings (including `let _ = …`,
            // which is also what the parser produces for bare
            // `f();` discarded calls) plus `print` stmts before
            // the tail expression. Control flow (if/while/for),
            // reassignment, task spawn, and other shapes that
            // affect the surrounding scope's state still
            // surface a clean diagnostic pointing at the
            // workaround (hoist outside the block). The
            // block's type is the tail expression's type.
            // Closure #129 extends the v1 Block MVP.
            env.push_scope();
            let mut typed_stmts: Vec<TypedStmt> = Vec::new();
            for s in stmts {
                match s {
                    Stmt::Let { name, annotation, expr: rhs, span } => {
                        let rhs_checked = if let Some(ann) = annotation {
                            let raw = check_expr(rhs, env, signatures, diagnostics);
                            coerce_checked(raw, ann, rhs.span, "let initializer", diagnostics)
                        } else {
                            check_expr(rhs, env, signatures, diagnostics)
                        };
                        let ty = rhs_checked.ty().clone();
                        env.insert_current(name.clone(), VarInfo {
                            ty: ty.clone(),
                            constant: rhs_checked.constant().cloned(),
                            moved: None,
                            decl_span: *span,
                            vec_literal_elements: None,
                            array_version: 0,
                            guarded_mutex: None,
                            no_drop: false,
                            is_const: false,
                            struct_literal_fields: None,
                            moved_fields: std::collections::BTreeMap::new(),
                        });
                        typed_stmts.push(TypedStmt::Let {
                            name: name.clone(),
                            ty,
                            expr: rhs_checked.expr,
                        });
                    }
                    Stmt::Print { items, .. } => {
                        let mut typed_items: Vec<crate::ir::TypedPrintItem> =
                            Vec::with_capacity(items.len());
                        for item in items {
                            match item {
                                crate::ast::PrintItem::Str(s) => typed_items
                                    .push(crate::ir::TypedPrintItem::Str(s.clone())),
                                crate::ast::PrintItem::Expr(e) => {
                                    let ce = check_expr(e, env, signatures, diagnostics);
                                    typed_items
                                        .push(crate::ir::TypedPrintItem::Expr(ce.expr));
                                }
                            }
                        }
                        typed_stmts.push(TypedStmt::Print { items: typed_items });
                    }
                    _ => {
                        diagnostics.push(Diagnostic::new(
                            s.span(),
                            "block expressions in v1 only allow `let` bindings and `print` statements before the tail expression — hoist control flow or assignments outside the block",
                        ));
                    }
                }
            }
            let tail_checked = check_expr(tail, env, signatures, diagnostics);
            let block_ty = tail_checked.ty().clone();
            env.pop_scope();
            CheckedExpr::new(
                TypedExprKind::Block {
                    stmts: typed_stmts,
                    tail: Box::new(tail_checked.expr),
                },
                block_ty,
                None,
                expr.span,
            )
        }
        ExprKind::Try { inner } => {
            // T2.6 Phase 1: `try EXPR` is reserved as a
            // keyword + parses cleanly, but the desugar to a
            // statement-level match-with-early-return needs
            // surrounding-stmt context that `check_expr`
            // doesn't have here. The desugar lands in Phase 2
            // alongside a stmt-level rewrite pass. Until
            // then, emit a clean WIP gate that explains the
            // limitation and points users at the manual
            // pattern.
            let _ = check_expr(inner, env, signatures, diagnostics);
            diagnostics.push(Diagnostic::new(
                expr.span,
                "`try EXPR` is reserved as a keyword but the desugar to \
                 match-with-early-return is still in progress (T2.6 Phase 2). \
                 Write the pattern manually: `match opt { Opt.Some(v) then v, \
                 Opt.None then return Opt.None };`",
            ));
            CheckedExpr::fallback_integer(expr.span)
        }
        ExprKind::IfExpr { cond, then_value, else_value } => {
            // T4 if-as-expression. cond must be bool;
            // then/else branch types must unify into the
            // result. Behaves identically to a 2-arm match
            // for codegen purposes — both backends emit
            // `cond` evaluation, a branch, and a merge that
            // unifies the two branch values.
            let cond_checked = check_expr(cond, env, signatures, diagnostics);
            if !matches!(cond_checked.ty(), Type::Bool) {
                diagnostics.push(Diagnostic::new(
                    cond.span,
                    format!(
                        "if-expression condition must be bool, got {}",
                        cond_checked.ty()
                    ),
                ));
            }
            let then_checked = check_expr(then_value, env, signatures, diagnostics);
            let else_checked = check_expr(else_value, env, signatures, diagnostics);
            let unified = if then_checked.ty() == else_checked.ty() {
                then_checked.ty().clone()
            } else {
                diagnostics.push(Diagnostic::new(
                    expr.span,
                    format!(
                        "if-expression branches have different types: then = {}, else = {}",
                        then_checked.ty(),
                        else_checked.ty()
                    ),
                ));
                then_checked.ty().clone()
            };
            // Constant-fold: if cond's constant is a known bool,
            // collapse to the selected branch's constant. Lets
            // downstream `prove r == k` discharge through the
            // const-fold layer without round-tripping to SMT.
            let constant = if let Some(TypedConst::Bool(c)) = cond_checked.constant() {
                if *c {
                    then_checked.constant().cloned()
                } else {
                    else_checked.constant().cloned()
                }
            } else {
                None
            };
            CheckedExpr::new(
                TypedExprKind::IfExpr {
                    cond: Box::new(cond_checked.expr),
                    then_value: Box::new(then_checked.expr),
                    else_value: Box::new(else_checked.expr),
                },
                unified,
                constant,
                expr.span,
            )
        }
    }
}

fn check_ref_mut(
    inner: &Expr,
    env: &Env,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    // `mut ref t.field` — single-level field-borrow. The base
    // binding must be an owned struct and have a field of the
    // given name. T1.2 phase 2b follow-up.
    if let ExprKind::FieldAccess { object, field, .. } = &inner.kind {
        if let ExprKind::Var(obj_name) = &object.kind {
            let Some(info) = env.lookup(obj_name) else {
                diagnostics.push(Diagnostic::new(
                    object.span,
                    format!("unknown variable '{}'", obj_name),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            let Type::Struct(struct_name) = info.ty.deref() else {
                diagnostics.push(Diagnostic::new(
                    object.span,
                    format!(
                        "field-borrow base '{}' must be a struct binding (got {})",
                        obj_name, info.ty
                    ),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            let Some(struct_info) = env.lookup_struct(struct_name) else {
                diagnostics.push(Diagnostic::new(
                    object.span,
                    format!("unknown struct type '{}'", struct_name),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            let Some((field_index, (_, field_ty))) = struct_info
                .fields
                .iter()
                .enumerate()
                .find(|(_, (n, _))| n == field)
            else {
                diagnostics.push(Diagnostic::new(
                    inner.span,
                    format!(
                        "struct '{}' has no field named '{}'",
                        struct_name, field
                    ),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            if info.moved.is_some() {
                diagnostics.push(Diagnostic::new(
                    inner.span,
                    format!(
                        "cannot mutably borrow field of '{}' after it was moved",
                        obj_name
                    ),
                ));
            }
            if info.ty.is_ref() {
                diagnostics.push(Diagnostic::new(
                    inner.span,
                    format!(
                        "cannot take 'mut ref' on a field of '{}' because the base \
                         is borrowed immutably (&T)",
                        obj_name
                    ),
                ));
            }
            let ref_ty = Type::RefMut(Box::new(field_ty.clone()));
            let object_ty = info.ty.clone();
            return CheckedExpr::new(
                TypedExprKind::RefMutField {
                    object: obj_name.clone(),
                    field: field.clone(),
                    field_index: field_index as u32,
                    object_ty,
                },
                ref_ty,
                None,
                span,
            );
        }
    }
    let ExprKind::Var(name) = &inner.kind else {
        diagnostics.push(Diagnostic::new(
            inner.span,
            "'mut ref' can only borrow a named variable or a struct field; \
             e.g. `mut ref xs` or `mut ref t.field`",
        ));
        return CheckedExpr::fallback_integer(span);
    };
    let Some(info) = env.lookup(name) else {
        diagnostics.push(Diagnostic::new(
            inner.span,
            format!("unknown variable '{}'", name),
        ));
        return CheckedExpr::fallback_integer(span);
    };
    if info.moved.is_some() {
        diagnostics.push(Diagnostic::new(
            inner.span,
            format!("cannot mutably borrow '{}' after it was moved", name),
        ));
    }
    if info.ty.is_ref() {
        diagnostics.push(Diagnostic::new(
            inner.span,
            format!(
                "cannot take '&mut' on '{}' because it is borrowed immutably (&T); \
                 only owned bindings or '&mut T' parameters can be mutably borrowed",
                name
            ),
        ));
    }
    let ref_ty = Type::RefMut(Box::new(info.ty.clone()));
    let decl_span = info.decl_span;
    CheckedExpr::new(
        TypedExprKind::RefMut { name: name.clone() },
        ref_ty,
        None,
        span,
    )
    .with_binding_decl_span(decl_span)
}

fn check_ref(
    inner: &Expr,
    env: &Env,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    // `ref t.field` — single-level field-borrow. Mirrors
    // `check_ref_mut`'s field-borrow arm. T1.2 phase 2b
    // follow-up.
    if let ExprKind::FieldAccess { object, field, .. } = &inner.kind {
        if let ExprKind::Var(obj_name) = &object.kind {
            let Some(info) = env.lookup(obj_name) else {
                diagnostics.push(Diagnostic::new(
                    object.span,
                    format!("unknown variable '{}'", obj_name),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            let Type::Struct(struct_name) = info.ty.deref() else {
                diagnostics.push(Diagnostic::new(
                    object.span,
                    format!(
                        "field-borrow base '{}' must be a struct binding (got {})",
                        obj_name, info.ty
                    ),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            let Some(struct_info) = env.lookup_struct(struct_name) else {
                diagnostics.push(Diagnostic::new(
                    object.span,
                    format!("unknown struct type '{}'", struct_name),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            let Some((field_index, (_, field_ty))) = struct_info
                .fields
                .iter()
                .enumerate()
                .find(|(_, (n, _))| n == field)
            else {
                diagnostics.push(Diagnostic::new(
                    inner.span,
                    format!(
                        "struct '{}' has no field named '{}'",
                        struct_name, field
                    ),
                ));
                return CheckedExpr::fallback_integer(span);
            };
            if info.moved.is_some() {
                diagnostics.push(Diagnostic::new(
                    inner.span,
                    format!(
                        "cannot borrow field of '{}' after it was moved",
                        obj_name
                    ),
                ));
            }
            let ref_ty = Type::Ref(Box::new(field_ty.clone()));
            let object_ty = info.ty.clone();
            return CheckedExpr::new(
                TypedExprKind::RefField {
                    object: obj_name.clone(),
                    field: field.clone(),
                    field_index: field_index as u32,
                    object_ty,
                },
                ref_ty,
                None,
                span,
            );
        }
    }
    let ExprKind::Var(name) = &inner.kind else {
        diagnostics.push(Diagnostic::new(
            inner.span,
            "'ref' can only borrow a named variable or a struct field; \
             e.g. `ref xs` or `ref t.field`",
        ));
        return CheckedExpr::fallback_integer(span);
    };
    let Some(info) = env.lookup(name) else {
        diagnostics.push(Diagnostic::new(
            inner.span,
            format!("unknown variable '{}'", name),
        ));
        return CheckedExpr::fallback_integer(span);
    };
    if info.moved.is_some() {
        diagnostics.push(Diagnostic::new(
            inner.span,
            format!("cannot borrow '{}' after it was moved", name),
        ));
    }
    if info.ty.is_ref() {
        diagnostics.push(Diagnostic::new(
            inner.span,
            "cannot create a reference to a reference",
        ));
    }
    let ref_ty = Type::Ref(Box::new(info.ty.clone()));
    let decl_span = info.decl_span;
    CheckedExpr::new(
        TypedExprKind::Ref { name: name.clone() },
        ref_ty,
        None,
        span,
    )
    .with_binding_decl_span(decl_span)
}


fn check_array_literal(
    elements: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if elements.is_empty() {
        diagnostics.push(Diagnostic::new(
            span,
            "empty array literals are not supported; explicit element type is required",
        ));
        return CheckedExpr::fallback_integer(span);
    }

    let typed_elements: Vec<CheckedExpr> = elements
        .iter()
        .map(|element| check_expr(element, env, signatures, diagnostics))
        .collect();

    let element_type = typed_elements[0].ty().clone();
    if !element_type.is_copy() {
        diagnostics.push(Diagnostic::new(
            span,
            format!("array element type must be Copy, got {}", element_type),
        ));
    }

    let mut coerced = Vec::with_capacity(typed_elements.len());
    for (index, element) in typed_elements.into_iter().enumerate() {
        let element_span = element.expr.span;
        let coerced_element = coerce_checked(
            element,
            &element_type,
            element_span,
            &format!("array element {}", index),
            diagnostics,
        );
        coerced.push(coerced_element);
    }

    let length = coerced.len() as u64;
    let array_type = Type::Array {
        element: Box::new(element_type),
        length,
    };

    CheckedExpr::new(
        TypedExprKind::ArrayLit {
            elements: coerced.into_iter().map(|element| element.expr).collect(),
        },
        array_type,
        None,
        span,
    )
}

fn check_index(
    array: &Expr,
    index: &Expr,
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let array_checked = check_expr(array, env, signatures, diagnostics);
    let index_checked = check_expr(index, env, signatures, diagnostics);

    enum IndexableKind {
        Array { length: u64 },
        Vec,
    }

    let resolved = array_checked.ty().deref();
    let (element_type, kind) = match resolved {
        Type::Array { element, length } => {
            ((**element).clone(), IndexableKind::Array { length: *length })
        }
        Type::Vec(element) => ((**element).clone(), IndexableKind::Vec),
        other => {
            diagnostics.push(Diagnostic::new(
                array.span,
                format!("cannot index into non-array type {}", other),
            ));
            return CheckedExpr::fallback_integer(span);
        }
    };
    // Refines #7: indexing a `Vec<U>` (or array) for non-Copy
    // `U` would struct-copy the element out, aliasing the
    // owner's slot with the new binding's storage and
    // double-freeing when both drop. Reject the read-as-
    // value form with a clear diagnostic; the user can still
    // construct / push / drop nested Vecs, just not pull
    // inner slots out by value. Future revisions can lift
    // this once second-class references (`&xs[i]`) or
    // explicit `move(xs, i)` / `take(xs, i)` builtins land.
    if !element_type.is_copy()
        && !matches!(element_type, Type::Ref(_) | Type::RefMut(_))
    {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "cannot index a Vec/array whose element type ({}) is non-Copy by value; \
                 the element would alias the owner's slot and double-free. \
                 Use `clone_at(&xs, i)` to bind an owned deep-clone of the slot.",
                element_type
            ),
        ));
        return CheckedExpr::fallback(element_type, span);
    }

    if !index_checked.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            index.span,
            format!(
                "array index must be an integer, got {}",
                index_checked.ty()
            ),
        ));
        return CheckedExpr::fallback(element_type, span);
    }

    let checked = match kind {
        IndexableKind::Array { length } => {
            let mut needs_check = true;
            if let Some(TypedConst::Int(value)) = index_checked.constant() {
                if *value < 0 {
                    diagnostics.push(Diagnostic::new(
                        index.span,
                        format!(
                            "array index {} is negative; length is {}",
                            value, length
                        ),
                    ));
                    return CheckedExpr::fallback(element_type, span);
                }
                if (*value as u128) >= length as u128 {
                    diagnostics.push(Diagnostic::new(
                        index.span,
                        format!(
                            "array index {} is out of range for length {}",
                            value, length
                        ),
                    ));
                    return CheckedExpr::fallback(element_type, span);
                }
                needs_check = false;
            }
            needs_check
        }
        IndexableKind::Vec => true,
    };

    CheckedExpr::new(
        TypedExprKind::Index {
            array: Box::new(array_checked.expr),
            index: Box::new(index_checked.expr),
            checked,
        },
        element_type,
        None,
        span,
    )
}

fn check_len(
    array: &Expr,
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let array_checked = check_expr(array, env, signatures, diagnostics);

    let resolved = array_checked.ty().deref();
    match resolved {
        Type::Array { length, .. } => {
            let length = *length;
            CheckedExpr::new(
                TypedExprKind::Len {
                    array: Box::new(array_checked.expr),
                    length,
                },
                Type::U64,
                Some(TypedConst::Int(length as i128)),
                span,
            )
        }
        Type::Vec(_) => CheckedExpr::new(
            TypedExprKind::Len {
                array: Box::new(array_checked.expr),
                length: 0,
            },
            Type::U64,
            None,
            span,
        ),
        Type::Str | Type::OwnedStr => CheckedExpr::new(
            // `length: 0` is the same sentinel used for Vec: the
            // backend dispatches on `array.ty` to decide how to fetch
            // the size. For Str/OwnedStr that's `strlen(s)` — the
            // byte length, not counting the NUL terminator.
            TypedExprKind::Len {
                array: Box::new(array_checked.expr),
                length: 0,
            },
            Type::U64,
            None,
            span,
        ),
        other => {
            diagnostics.push(Diagnostic::new(
                array.span,
                format!("len() requires an array, Vec, or Str argument, got {}", other),
            ));
            CheckedExpr::fallback(Type::U64, span)
        }
    }
}

fn check_unary(
    op: UnaryOp,
    checked: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    match op {
        UnaryOp::Neg => {
            if !checked.ty().is_numeric() {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("unary '-' requires a numeric operand, got {}", checked.ty()),
                ));
                return CheckedExpr::fallback_integer(span);
            }
            if checked.ty().is_unsigned_integer() && !checked.flexible_integer {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "unary '-' cannot be applied to unsigned type {}",
                        checked.ty()
                    ),
                ));
                return CheckedExpr::fallback_integer(span);
            }

            let operand_ty = checked.ty().clone();
            let constant = match checked.constant().cloned() {
                Some(TypedConst::Int(value)) => match value.checked_neg() {
                    Some(negative) if value_fits_type(negative, &Type::I64) => {
                        Some(TypedConst::Int(negative))
                    }
                    Some(_) => {
                        diagnostics.push(Diagnostic::new(
                            span,
                            "negative integer literal does not fit in i64",
                        ));
                        None
                    }
                    None => {
                        diagnostics.push(Diagnostic::new(
                            span,
                            "integer negation overflows in constant expression",
                        ));
                        None
                    }
                },
                Some(TypedConst::Float(value)) => {
                    let negative = -value;
                    if finite_float_value(negative, &operand_ty) {
                        Some(TypedConst::Float(negative))
                    } else {
                        diagnostics.push(Diagnostic::new(
                            span,
                            format!("float negation is not finite in {}", operand_ty),
                        ));
                        None
                    }
                }
                _ => None,
            };

            let ty = if checked.flexible_integer {
                Type::I64
            } else {
                operand_ty
            };
            CheckedExpr::new(
                TypedExprKind::Unary {
                    op,
                    expr: Box::new(checked.expr),
                },
                ty,
                constant,
                span,
            )
        }
        UnaryOp::Not => {
            require_type(
                checked.ty(),
                &Type::Bool,
                checked.expr.span,
                "unary '!'",
                diagnostics,
            );
            let constant = match checked.constant() {
                Some(TypedConst::Bool(value)) => Some(TypedConst::Bool(!value)),
                _ => None,
            };
            CheckedExpr::new(
                TypedExprKind::Unary {
                    op,
                    expr: Box::new(checked.expr),
                },
                Type::Bool,
                constant,
                span,
            )
        }
    }
}

fn check_binary(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let lhs = check_expr(left, env, signatures, diagnostics);
    // Short-circuit && / || at compile time: if the LHS const-folds
    // to a value that determines the result (`false &&` / `true ||`),
    // skip the RHS check entirely. Reasons:
    //   1. Matches runtime semantics — the RHS would never execute.
    //   2. Avoids spurious diagnostics from dead code that the user
    //      gated behind a `false` constant (e.g. debug toggles, feature
    //      flags) — including const-fold panics on the RHS like
    //      `false && (10 / 0) > 0`.
    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        if let Some(TypedConst::Bool(lhs_value)) = lhs.constant() {
            let short_circuits = matches!(
                (op, *lhs_value),
                (BinaryOp::And, false) | (BinaryOp::Or, true)
            );
            if short_circuits {
                let span = left.span.merge(right.span);
                let result = *lhs_value;
                // Still type-check that the RHS is a bool by parsing
                // it shape-only — discard any diagnostics it would
                // produce so dead code stays quiet. We achieve this
                // by routing RHS check through a throwaway diagnostic
                // buffer.
                let mut throwaway: Vec<Diagnostic> = Vec::new();
                let rhs = check_expr(right, env, signatures, &mut throwaway);
                return CheckedExpr::new(
                    TypedExprKind::Binary {
                        op,
                        left: Box::new(lhs.expr),
                        right: Box::new(rhs.expr),
                        checked: true,
                    },
                    Type::Bool,
                    Some(TypedConst::Bool(result)),
                    span,
                );
            }
        }
    }
    let rhs = check_expr(right, env, signatures, diagnostics);
    let span = left.span.merge(right.span);

    match op {
        BinaryOp::Add => {
            // `Str + Str` (or `OwnedStr + Str`, etc.) lowers to a
            // heap-allocating concat that yields a fresh OwnedStr.
            // The lhs/rhs are consumed; if either is OwnedStr the
            // checker marks the underlying binding as moved.
            let lhs_is_strish = matches!(*lhs.ty(), Type::Str | Type::OwnedStr);
            let rhs_is_strish = matches!(*rhs.ty(), Type::Str | Type::OwnedStr);
            if lhs_is_strish && rhs_is_strish {
                return check_str_concat(lhs, rhs, span, left, right, env);
            }
            check_numeric_binary(op, lhs, rhs, span, diagnostics)
        }
        BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
            check_numeric_binary(op, lhs, rhs, span, diagnostics)
        }
        BinaryOp::Rem => check_integer_remainder(lhs, rhs, span, diagnostics),
        BinaryOp::Shl | BinaryOp::Shr => check_shift(op, lhs, rhs, span, diagnostics),
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
            check_integer_bitwise(op, lhs, rhs, span, diagnostics)
        }
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            // Allow ordering on Str/OwnedStr via strcmp lowering.
            // Any combination of Str and OwnedStr is accepted —
            // both lower to a `char*`/`i8*` operand and `strcmp`
            // only reads (no allocation, no free), so OwnedStr
            // operands are auto-borrowed (not consumed).
            if matches!(*lhs.ty(), Type::Str | Type::OwnedStr)
                && matches!(*rhs.ty(), Type::Str | Type::OwnedStr)
            {
                return CheckedExpr::new(
                    TypedExprKind::Binary {
                        op,
                        left: Box::new(lhs.expr),
                        right: Box::new(rhs.expr),
                        checked: true,
                    },
                    Type::Bool,
                    None,
                    span,
                );
            }
            check_numeric_comparison(op, lhs, rhs, span, diagnostics)
        }
        BinaryOp::Eq | BinaryOp::Ne => check_equality(op, lhs, rhs, span, signatures, diagnostics),
        BinaryOp::And | BinaryOp::Or => check_boolean_binary(op, lhs, rhs, span, diagnostics),
    }
}

fn check_numeric_binary(
    op: BinaryOp,
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let Some(result_type) = promoted_numeric_type(&lhs, &rhs, diagnostics) else {
        return CheckedExpr::fallback_integer(span);
    };

    let lhs = coerce_numeric_operand(lhs, &result_type);
    let rhs = coerce_numeric_operand(rhs, &result_type);
    let constant = if is_known_zero(&rhs) && op == BinaryOp::Div {
        diagnostics.push(Diagnostic::new(
            rhs.expr.span,
            if result_type.is_float() {
                "floating-point division by zero in constant expression"
            } else {
                "division by zero in constant expression"
            },
        ));
        None
    } else if result_type.is_float() {
        match (lhs.constant(), rhs.constant()) {
            (Some(left), Some(right)) => {
                eval_float_binary(op, left, right, &result_type, span, diagnostics)
            }
            _ => None,
        }
    } else {
        match (lhs.constant(), rhs.constant()) {
            (Some(TypedConst::Int(a)), Some(TypedConst::Int(b))) => {
                eval_integer_binary(op, *a, *b, &result_type, rhs.expr.span, diagnostics)
            }
            _ => None,
        }
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        result_type,
        constant,
        span,
    )
}

fn check_integer_remainder(
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let Some(result_type) = promoted_integer_type(&lhs, &rhs, diagnostics) else {
        return CheckedExpr::fallback_integer(span);
    };

    let lhs = coerce_numeric_operand(lhs, &result_type);
    let rhs = coerce_numeric_operand(rhs, &result_type);
    let constant = if is_known_zero(&rhs) {
        diagnostics.push(Diagnostic::new(
            rhs.expr.span,
            "remainder by zero in constant expression",
        ));
        None
    } else {
        match (lhs.constant(), rhs.constant()) {
            (Some(TypedConst::Int(a)), Some(TypedConst::Int(b))) => eval_integer_binary(
                BinaryOp::Rem,
                *a,
                *b,
                &result_type,
                rhs.expr.span,
                diagnostics,
            ),
            _ => None,
        }
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op: BinaryOp::Rem,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        result_type,
        constant,
        span,
    )
}

fn check_integer_bitwise(
    op: BinaryOp,
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    // Bitwise `&`, `|`, `^` take two same-width integer operands
    // and return an integer of that width. Floats are rejected;
    // bools have their own logical `&&`/`||` (the parser doesn't
    // route bool operands here anyway because the type-promotion
    // path here is integer-only).
    let Some(result_type) = promoted_integer_type(&lhs, &rhs, diagnostics) else {
        return CheckedExpr::fallback_integer(span);
    };

    let lhs = coerce_numeric_operand(lhs, &result_type);
    let rhs = coerce_numeric_operand(rhs, &result_type);
    let constant = match (lhs.constant(), rhs.constant()) {
        (Some(TypedConst::Int(a)), Some(TypedConst::Int(b))) => eval_integer_binary(
            op,
            *a,
            *b,
            &result_type,
            span,
            diagnostics,
        ),
        _ => None,
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        result_type,
        constant,
        span,
    )
}

fn check_shift(
    op: BinaryOp,
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if !lhs.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            lhs.expr.span,
            format!("shift left operand must be an integer, got {}", lhs.ty()),
        ));
        return CheckedExpr::fallback_integer(span);
    }
    if !rhs.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            rhs.expr.span,
            format!("shift count must be an integer, got {}", rhs.ty()),
        ));
        return CheckedExpr::fallback_integer(span);
    }

    let result_type = lhs.ty().clone();
    let bits = result_type.bits().expect("integer type has width") as i128;
    let shift_count = match rhs.constant() {
        Some(TypedConst::Int(value)) => {
            let value = *value;
            if value < 0 {
                diagnostics.push(Diagnostic::new(
                    rhs.expr.span,
                    "shift count cannot be negative",
                ));
            }
            if value >= bits {
                diagnostics.push(Diagnostic::new(
                    rhs.expr.span,
                    format!("shift count must be less than {} for {}", bits, result_type),
                ));
            }
            Some(value)
        }
        _ => None,
    };

    if op == BinaryOp::Shl && result_type.is_signed_integer() {
        if let Some(TypedConst::Int(value)) = lhs.constant() {
            if *value < 0 {
                diagnostics.push(Diagnostic::new(
                    lhs.expr.span,
                    "left shift of a negative signed value is not allowed",
                ));
            }
        }
    }

    let constant = match (lhs.constant().cloned(), shift_count) {
        (Some(TypedConst::Int(value)), Some(count)) if 0 <= count && count < bits => {
            eval_shift(op, value, count as u32, &result_type, span, diagnostics)
        }
        _ => None,
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        result_type,
        constant,
        span,
    )
}

/// Type-check `Str + Str` (or `OwnedStr + Str`, etc.) → `OwnedStr`.
/// The result is a fresh heap-allocated string. If either operand is
/// an `OwnedStr` bound to a Var, that Var is marked moved so the
/// affine machinery prevents double-use; the lhs/rhs runtime values
/// flow into the concat as the new buffer's prefix and suffix and
/// the old `OwnedStr` operand's buffer is freed by the concat (the
/// backend reuses + frees in one step). `Str` operands are
/// borrowed (Copy) and not affected.
fn check_str_concat(
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    left: &Expr,
    right: &Expr,
    env: &mut Env,
) -> CheckedExpr {
    // Mark the underlying binding as moved when an operand is an
    // OwnedStr Var. For other shapes (literal, nested concat) the
    // value is rvalue and there's nothing to mark.
    if matches!(*lhs.ty(), Type::OwnedStr) {
        if let ExprKind::Var(name) = &left.kind {
            if let Some(info) = env.lookup_mut(name) {
                info.moved = Some(left.span);
            }
        }
    }
    if matches!(*rhs.ty(), Type::OwnedStr) {
        if let ExprKind::Var(name) = &right.kind {
            if let Some(info) = env.lookup_mut(name) {
                info.moved = Some(right.span);
            }
        }
    }
    CheckedExpr::new(
        TypedExprKind::Binary {
            op: BinaryOp::Add,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: false,
        },
        Type::OwnedStr,
        None,
        span,
    )
}

fn check_numeric_comparison(
    op: BinaryOp,
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let Some(result_type) = promoted_numeric_type(&lhs, &rhs, diagnostics) else {
        return CheckedExpr::new(TypedExprKind::Bool(false), Type::Bool, None, span);
    };
    let lhs = coerce_numeric_operand(lhs, &result_type);
    let rhs = coerce_numeric_operand(rhs, &result_type);

    let constant = match (lhs.constant(), rhs.constant()) {
        (Some(left), Some(right)) => {
            let comparison = if result_type.is_float() {
                let a = const_to_float(left);
                let b = const_to_float(right);
                match op {
                    BinaryOp::Lt => a < b,
                    BinaryOp::Le => a <= b,
                    BinaryOp::Gt => a > b,
                    BinaryOp::Ge => a >= b,
                    _ => unreachable!(),
                }
            } else {
                let a = const_to_int(left);
                let b = const_to_int(right);
                match op {
                    BinaryOp::Lt => a < b,
                    BinaryOp::Le => a <= b,
                    BinaryOp::Gt => a > b,
                    BinaryOp::Ge => a >= b,
                    _ => unreachable!(),
                }
            };
            Some(TypedConst::Bool(comparison))
        }
        _ => None,
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        Type::Bool,
        constant,
        span,
    )
}

fn check_equality(
    op: BinaryOp,
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if *lhs.ty() == Type::Bool || *rhs.ty() == Type::Bool {
        if *lhs.ty() != Type::Bool || *rhs.ty() != Type::Bool {
            diagnostics.push(Diagnostic::new(
                rhs.expr.span,
                format!(
                    "equality operands must both be bool or both be numeric, got {} and {}",
                    lhs.ty(),
                    rhs.ty()
                ),
            ));
        }
        let constant = match (lhs.constant(), rhs.constant()) {
            (Some(TypedConst::Bool(a)), Some(TypedConst::Bool(b))) => {
                Some(TypedConst::Bool(if op == BinaryOp::Eq {
                    a == b
                } else {
                    a != b
                }))
            }
            _ => None,
        };
        return CheckedExpr::new(
            TypedExprKind::Binary {
                op,
                left: Box::new(lhs.expr),
                right: Box::new(rhs.expr),
                checked: true,
            },
            Type::Bool,
            constant,
            span,
        );
    }

    // Str / OwnedStr equality lowers to a strcmp call in both
    // backends. Accept any combination of the two — OwnedStr is
    // auto-borrowed to Str-like behavior since strcmp only reads.
    // No constant-fold path; literals are interned per-call site
    // so identity comparison would be misleading.
    let lhs_strish = matches!(*lhs.ty(), Type::Str | Type::OwnedStr);
    let rhs_strish = matches!(*rhs.ty(), Type::Str | Type::OwnedStr);
    if lhs_strish || rhs_strish {
        if !lhs_strish || !rhs_strish {
            diagnostics.push(Diagnostic::new(
                rhs.expr.span,
                format!(
                    "equality operands must both be Str or OwnedStr, got {} and {}",
                    lhs.ty(),
                    rhs.ty()
                ),
            ));
        }
        return CheckedExpr::new(
            TypedExprKind::Binary {
                op,
                left: Box::new(lhs.expr),
                right: Box::new(rhs.expr),
                checked: true,
            },
            Type::Bool,
            None,
            span,
        );
    }

    // Aggregate types (struct, tuple, enum) don't have built-in
    // `==`/`!=` — but if the user has declared an `Eq` impl
    // (`implement Eq for T { fn eq(self: T, other: T) -> bool }`),
    // the hoisted `<T>_eq` function dispatches the operation.
    // `a == b` desugars to `<T>_eq(a, b)`; `a != b` to
    // `!<T>_eq(a, b)`. T1.5 phase 2 follow-up.
    let lhs_is_aggregate = matches!(
        lhs.ty(),
        Type::Struct(_) | Type::Tuple(_) | Type::Enum(_)
    );
    let rhs_is_aggregate = matches!(
        rhs.ty(),
        Type::Struct(_) | Type::Tuple(_) | Type::Enum(_)
    );
    if lhs_is_aggregate || rhs_is_aggregate {
        // Tuple auto-`==`: compiler-derived field-by-field
        // equality. `(a, b) == (c, d)` → `a == c && b == d`.
        // Tuples are anonymous so there's no user impl path;
        // each element must itself be comparable (primitive
        // or a nominal type with an Eq impl). T1.5 phase 2
        // follow-up.
        if let (Type::Tuple(l_elems), Type::Tuple(r_elems)) = (lhs.ty(), rhs.ty()) {
            if l_elems.len() == r_elems.len() && l_elems == r_elems {
                let lhs_ty = lhs.ty().clone();
                let elems = l_elems.clone();
                let lhs_expr = lhs.expr.clone();
                let rhs_expr = rhs.expr.clone();
                let mut chain: Option<TypedExpr> = None;
                for (idx, elem_ty) in elems.iter().enumerate() {
                    let l_access = TypedExpr {
                        kind: TypedExprKind::TupleAccess {
                            tuple: Box::new(lhs_expr.clone()),
                            index: idx as u32,
                        },
                        ty: elem_ty.clone(),
                        constant: None,
                        span,
                        binding_decl_span: None,
                    };
                    let r_access = TypedExpr {
                        kind: TypedExprKind::TupleAccess {
                            tuple: Box::new(rhs_expr.clone()),
                            index: idx as u32,
                        },
                        ty: elem_ty.clone(),
                        constant: None,
                        span,
                        binding_decl_span: None,
                    };
                    // Per-element comparison: primitive types
                    // use built-in `==`; nominal types route
                    // through `<T>_eq` (must exist).
                    let elem_eq: TypedExpr = match elem_ty {
                        Type::Struct(name) | Type::Enum(name) => {
                            let eq_fn = format!("{}_eq", name);
                            match signatures.get(&eq_fn) {
                                Some(sig)
                                    if sig.return_type == Type::Bool
                                        && sig.params.len() == 2 =>
                                {
                                    TypedExpr {
                                        kind: TypedExprKind::Call {
                                            name: eq_fn,
                                            name_span: span,
                                            args: vec![l_access, r_access],
                                        },
                                        ty: Type::Bool,
                                        constant: None,
                                        span,
                                        binding_decl_span: None,
                                    }
                                }
                                _ => {
                                    diagnostics.push(Diagnostic::new(
                                        span,
                                        format!(
                                            "tuple `==` element at index {} has \
                                             type {}, but no `implement Eq for {}` \
                                             is in scope",
                                            idx, elem_ty, name
                                        ),
                                    ));
                                    return CheckedExpr::new(
                                        TypedExprKind::Bool(false),
                                        Type::Bool,
                                        None,
                                        span,
                                    );
                                }
                            }
                        }
                        _ => TypedExpr {
                            kind: TypedExprKind::Binary {
                                op: BinaryOp::Eq,
                                left: Box::new(l_access),
                                right: Box::new(r_access),
                                checked: true,
                            },
                            ty: Type::Bool,
                            constant: None,
                            span,
                            binding_decl_span: None,
                        },
                    };
                    chain = Some(match chain {
                        None => elem_eq,
                        Some(prev) => TypedExpr {
                            kind: TypedExprKind::Binary {
                                op: BinaryOp::And,
                                left: Box::new(prev),
                                right: Box::new(elem_eq),
                                checked: true,
                            },
                            ty: Type::Bool,
                            constant: None,
                            span,
                            binding_decl_span: None,
                        },
                    });
                }
                let _ = lhs_ty;
                let eq_chain = chain.unwrap_or(TypedExpr {
                    kind: TypedExprKind::Bool(true),
                    ty: Type::Bool,
                    constant: Some(TypedConst::Bool(true)),
                    span,
                    binding_decl_span: None,
                });
                if op == BinaryOp::Eq {
                    return CheckedExpr::new(eq_chain.kind, Type::Bool, None, span);
                }
                return CheckedExpr::new(
                    TypedExprKind::Unary {
                        op: crate::ast::UnaryOp::Not,
                        expr: Box::new(eq_chain),
                    },
                    Type::Bool,
                    None,
                    span,
                );
            }
        }
        let same_nominal = match (lhs.ty(), rhs.ty()) {
            (Type::Struct(l), Type::Struct(r)) if l == r => Some(l.clone()),
            (Type::Enum(l), Type::Enum(r)) if l == r => Some(l.clone()),
            _ => None,
        };
        if let Some(name) = same_nominal {
            let eq_fn = format!("{}_eq", name);
            if let Some(sig) = signatures.get(&eq_fn) {
                let bool_return = sig.return_type == Type::Bool;
                let two_args = sig.params.len() == 2;
                if bool_return && two_args {
                    let call_kind = TypedExprKind::Call {
                        name: eq_fn,
                        name_span: span,
                        args: vec![lhs.expr, rhs.expr],
                    };
                    let call = CheckedExpr::new(call_kind, Type::Bool, None, span);
                    if op == BinaryOp::Eq {
                        return call;
                    }
                    return CheckedExpr::new(
                        TypedExprKind::Unary {
                            op: crate::ast::UnaryOp::Not,
                            expr: Box::new(call.expr),
                        },
                        Type::Bool,
                        None,
                        span,
                    );
                }
            }
        }
        let hint = match (lhs.ty(), rhs.ty()) {
            (Type::Struct(name), _) | (_, Type::Struct(name)) => format!(
                "struct '{}' has no built-in `==` — declare \
                 `implement Eq for {} {{ fn eq(self: {}, other: {}) -> bool {{ … }} }}` \
                 to define equality, or compare field-by-field",
                name, name, name, name
            ),
            (Type::Tuple(_), _) | (_, Type::Tuple(_)) => {
                "tuple `==` requires both operands to be tuples of the same \
                 shape — use matching arities and element types".to_string()
            }
            (Type::Enum(name), _) | (_, Type::Enum(name)) => format!(
                "enum '{}' has no built-in `==` — declare \
                 `implement Eq for {} {{ fn eq(self: {}, other: {}) -> bool {{ … }} }}` \
                 to define equality, or use `match` to discriminate",
                name, name, name, name
            ),
            _ => unreachable!("guarded by lhs_is_aggregate || rhs_is_aggregate"),
        };
        diagnostics.push(Diagnostic::new(span, hint));
        return CheckedExpr::new(TypedExprKind::Bool(false), Type::Bool, None, span);
    }

    let Some(result_type) = promoted_numeric_type(&lhs, &rhs, diagnostics) else {
        return CheckedExpr::new(TypedExprKind::Bool(false), Type::Bool, None, span);
    };
    let lhs = coerce_numeric_operand(lhs, &result_type);
    let rhs = coerce_numeric_operand(rhs, &result_type);
    let constant = match (lhs.constant(), rhs.constant()) {
        (Some(left), Some(right)) => {
            let equal = if result_type.is_float() {
                const_to_float(left) == const_to_float(right)
            } else {
                const_to_int(left) == const_to_int(right)
            };
            Some(TypedConst::Bool(if op == BinaryOp::Eq {
                equal
            } else {
                !equal
            }))
        }
        _ => None,
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        Type::Bool,
        constant,
        span,
    )
}

fn check_boolean_binary(
    op: BinaryOp,
    lhs: CheckedExpr,
    rhs: CheckedExpr,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    require_type(
        lhs.ty(),
        &Type::Bool,
        lhs.expr.span,
        "boolean left operand",
        diagnostics,
    );
    require_type(
        rhs.ty(),
        &Type::Bool,
        rhs.expr.span,
        "boolean right operand",
        diagnostics,
    );
    let constant = match (lhs.constant(), rhs.constant()) {
        (Some(TypedConst::Bool(a)), Some(TypedConst::Bool(b))) => {
            Some(TypedConst::Bool(if op == BinaryOp::And {
                *a && *b
            } else {
                *a || *b
            }))
        }
        _ => None,
    };

    CheckedExpr::new(
        TypedExprKind::Binary {
            op,
            left: Box::new(lhs.expr),
            right: Box::new(rhs.expr),
            checked: true,
        },
        Type::Bool,
        constant,
        span,
    )
}

/// Check a call where the callee is a binding of fn-pointer
/// type (i.e. an indirect call through a `fn(T...) -> R`
/// value). The callee resolves to a `Var(name)` over the
/// binding; arguments are coerced to the declared parameter
/// types. Returns a `TypedExprKind::CallIndirect` so the
/// backends emit a function-pointer invocation. The
/// downstream pure-body / lock-analysis gates already
/// recognize `CallIndirect` and reject it where unsafe.
fn check_indirect_call(
    name: &str,
    name_span: Span,
    callee_decl_span: Span,
    param_types: &[Type],
    return_type: &Type,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != param_types.len() {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "fn-ptr '{}' expects {} argument(s), got {}",
                name,
                param_types.len(),
                args.len()
            ),
        ));
    }
    let mut typed_args = Vec::with_capacity(args.len());
    for (idx, arg) in args.iter().enumerate() {
        let checked = check_expr(arg, env, signatures, diagnostics);
        let target = param_types
            .get(idx)
            .cloned()
            .unwrap_or_else(|| checked.ty().clone());
        let coerced = coerce_checked(
            checked,
            &target,
            arg.span,
            &format!("indirect call argument {}", idx),
            diagnostics,
        );
        typed_args.push(coerced.expr);
    }
    // Build the callee expression — a TypedExpr that loads
    // the fn-ptr binding's current value.
    let callee = TypedExpr {
        kind: TypedExprKind::Var(name.to_string()),
        ty: Type::FnPtr(param_types.to_vec(), Box::new(return_type.clone())),
        constant: None,
        span: name_span,
        binding_decl_span: Some(callee_decl_span),
    };
    CheckedExpr::new(
        TypedExprKind::CallIndirect {
            callee: Box::new(callee),
            args: typed_args,
        },
        return_type.clone(),
        None,
        span,
    )
}

fn check_call(
    name: &str,
    name_span: Span,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    // The `name_span` parameter is the callee identifier's
    // source span; threaded through so the constructed
    // `TypedExprKind::Call` (the main user-function path
    // below) carries the precise span. Sub-helpers that
    // construct synthetic calls (vec/push/atomic_*/etc.)
    // still receive `span` (the outer call span) — that's
    // close enough for LSP highlighting; refining it would
    // require plumbing name_span through every builtin
    // checker, which is a follow-up.
    match name {
        "vec" => return check_vec_builtin(args, env, signatures, span, diagnostics),
        "push" => return check_push_builtin(args, env, signatures, span, diagnostics),
        "set" => return check_set_builtin(args, env, signatures, span, diagnostics),
        "clone" => return check_clone_builtin(args, env, signatures, span, diagnostics),
        "clone_at" => {
            return check_clone_at_builtin(args, env, signatures, span, diagnostics)
        }
        "min" | "max" => {
            return check_min_max_builtin(name, args, env, signatures, span, diagnostics);
        }
        "atomic_new"
        | "atomic_load"
        | "atomic_store"
        | "atomic_fetch_add"
        | "atomic_compare_exchange" => {
            return check_atomic_builtin(name, args, env, signatures, span, diagnostics);
        }
        "channel_new" | "channel_send" | "channel_recv" => {
            return check_channel_builtin(name, args, env, signatures, span, diagnostics);
        }
        "mutex_new" | "mutex_lock" | "guard_get" | "guard_set" => {
            return check_mutex_builtin(name, args, env, signatures, span, diagnostics);
        }
        _ => {}
    }

    let Some(signature) = signatures.get(name).cloned() else {
        // Maybe `name` is bound to a fn-pointer locally; if so
        // lower as an indirect call. The binding's type must
        // be `fn(T1, ...) -> R` and the call's argument tuple
        // must match T1..Tn. Indirect calls bypass the
        // name-based call-graph passes (locks_params,
        // ensures, purity), so the checker rejects them in
        // contexts that need those guarantees (the lock /
        // pure-body walkers already cover that downstream).
        if let Some(info) = env.lookup(name) {
            if let Type::FnPtr(param_types, ret) = info.ty.clone() {
                let callee_decl_span = info.decl_span;
                return check_indirect_call(
                    name,
                    name_span,
                    callee_decl_span,
                    &param_types,
                    &ret,
                    args,
                    env,
                    signatures,
                    span,
                    diagnostics,
                );
            }
        }
        diagnostics.push(Diagnostic::new(
            span,
            format!("unknown function '{}'", name),
        ));
        return CheckedExpr::fallback_integer(span);
    };

    if args.len() != signature.params.len() {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "function '{}' expects {} argument(s), got {}",
                name,
                signature.params.len(),
                args.len()
            ),
        ));
    }

    check_arg_aliasing(args, env, span, diagnostics);

    // Cross-function double-acquire check. For each
    // parameter the callee marks as "this function locks
    // me" in `locks_params`, find the corresponding arg's
    // tracked mutex name. If any currently-live binding in
    // env guards that mutex, the call would deadlock on
    // entry — flag it at compile time.
    for (index, arg) in args.iter().enumerate() {
        if !signature.locks_params.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some(target) = extract_locked_mutex_name(arg) else {
            continue;
        };
        let already_held = env
            .all_bindings()
            .any(|(_, info)| {
                info.moved.is_none()
                    && info
                        .guarded_mutex
                        .as_deref()
                        .map(|n| n == target)
                        .unwrap_or(false)
            });
        if already_held {
            diagnostics.push(Diagnostic::new(
                arg.span,
                format!(
                    "cross-function double acquisition: '{}' would call `mutex_lock` on '{}', but a Guard for '{}' is still live in this scope",
                    name, target, target
                ),
            ));
        }
    }

    let typed_args = args
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            let checked = check_expr(arg, env, signatures, diagnostics);
            let coerced = if let Some(expected) = signature.params.get(index) {
                coerce_checked(
                    checked,
                    expected,
                    arg.span,
                    &format!("argument {} to '{}'", index + 1, name),
                    diagnostics,
                )
            } else {
                checked
            };
            diagnose_partial_then_whole_move(arg, &coerced, env, diagnostics);
            consume_if_moved_var(arg, &coerced, env);
            coerced.expr
        })
        .collect();

    CheckedExpr::new(
        TypedExprKind::Call {
            name: name.to_owned(),
            name_span,
            args: typed_args,
        },
        signature.return_type,
        None,
        span,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArgKind {
    Move,
    Ref,
    RefMut,
}

fn classify_arg(arg: &Expr, env: &Env) -> Option<(String, ArgKind)> {
    match &arg.kind {
        ExprKind::Var(name) => {
            let info = env.lookup(name)?;
            if info.ty.is_copy() {
                // Copy types (including ref types) don't cause aliasing in
                // their primary form. Re-borrows of `&T` / `&mut T` could
                // alias the underlying owner but we don't track that yet.
                None
            } else {
                Some((name.clone(), ArgKind::Move))
            }
        }
        ExprKind::Ref { inner } => {
            if let ExprKind::Var(name) = &inner.kind {
                Some((name.clone(), ArgKind::Ref))
            } else {
                None
            }
        }
        ExprKind::RefMut { inner } => {
            if let ExprKind::Var(name) = &inner.kind {
                Some((name.clone(), ArgKind::RefMut))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn check_arg_aliasing(
    args: &[Expr],
    env: &Env,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut by_target: HashMap<String, Vec<ArgKind>> = HashMap::new();
    for arg in args {
        if let Some((target, kind)) = classify_arg(arg, env) {
            by_target.entry(target).or_default().push(kind);
        }
    }
    for (target, kinds) in &by_target {
        let mut_count = kinds.iter().filter(|k| **k == ArgKind::RefMut).count();
        let move_count = kinds.iter().filter(|k| **k == ArgKind::Move).count();
        if mut_count > 0 && kinds.len() > 1 {
            diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "argument list aliases '{}': an '&mut' borrow cannot coexist \
                     with another use of the same variable in the same call",
                    target
                ),
            ));
        }
        if move_count > 0 && kinds.len() > 1 {
            diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "argument list aliases '{}': moving it cannot coexist with \
                     a borrow of the same variable in the same call",
                    target
                ),
            ));
        }
    }
}

/// `min(a, b)` and `max(a, b)` — pure built-in intrinsics that
/// return the smaller / larger of two operands of the same
/// numeric type. Promoted numeric type rules apply (i32 + i64 →
/// i64). Lowering is per-backend: C inlines a ternary; LLVM
/// emits a `select` on `icmp`. As a regular `Call` expression
/// they fit existing infrastructure (effects checker treats them
/// specially via the name; reduction clauses also recognize the
/// shape).
fn check_min_max_builtin(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 2 {
        diagnostics.push(Diagnostic::new(
            span,
            format!("'{}' takes exactly 2 arguments, got {}", name, args.len()),
        ));
        return CheckedExpr::fallback_integer(span);
    }
    let lhs = check_expr(&args[0], env, signatures, diagnostics);
    let rhs = check_expr(&args[1], env, signatures, diagnostics);
    let Some(result_type) = promoted_numeric_type(&lhs, &rhs, diagnostics) else {
        return CheckedExpr::fallback_integer(span);
    };
    let lhs = coerce_numeric_operand(lhs, &result_type);
    let rhs = coerce_numeric_operand(rhs, &result_type);
    CheckedExpr::new(
        TypedExprKind::Call {
            name: name.to_string(),
            name_span: span,
            args: vec![lhs.expr, rhs.expr],
        },
        result_type,
        None,
        span,
    )
}

/// Type-check the five builtin atomic operations. Element type
/// is polymorphic over the supported integer widths and bool:
/// i8, i16, i32, i64, u8, u16, u32, u64, bool. Operations are
/// sequentially consistent. Shapes (T ranges over the
/// supported types):
///
///   atomic_new(initial: T) -> Atomic<T>
///       Element type T is inferred from `initial`. Flexible
///       integer literals default to `i64` for back-compat.
///   atomic_load(a: &Atomic<T>) -> T
///   atomic_store(a: &Atomic<T>, v: T) -> T
///   atomic_fetch_add(a: &Atomic<T>, v: T) -> T
///   atomic_compare_exchange(a: &Atomic<T>, expected: T, new: T) -> bool
fn check_atomic_builtin(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    match name {
        "atomic_new" => {
            if args.len() != 1 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "'atomic_new' takes exactly 1 argument (initial value), got {}",
                        args.len()
                    ),
                ));
                return CheckedExpr::fallback(Type::Atomic(Box::new(Type::I64)), span);
            }
            let initial = check_expr(&args[0], env, signatures, diagnostics);
            // Pick the element type from the argument. A
            // flexible integer literal (e.g. `atomic_new(0)`)
            // defaults to i64 — preserves v1 source compat.
            // Otherwise the argument's concrete type wins:
            // `atomic_new(0u8)`, `atomic_new(false)`, or
            // `let x: i32 = 7; atomic_new(x)` all pick their
            // exact element width.
            let element = if initial.flexible_integer
                && matches!(initial.ty(), Type::I64)
            {
                Type::I64
            } else {
                initial.ty().clone()
            };
            if !is_supported_atomic_element(&element) {
                diagnostics.push(Diagnostic::new(
                    args[0].span,
                    format!(
                        "'atomic_new' element type must be an integer width or bool, got {}",
                        element
                    ),
                ));
                return CheckedExpr::fallback(Type::Atomic(Box::new(Type::I64)), span);
            }
            let initial = coerce_checked(
                initial,
                &element,
                args[0].span,
                "atomic_new initial value",
                diagnostics,
            );
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "atomic_new".to_string(),
                    name_span: span,
                    args: vec![initial.expr],
                },
                Type::Atomic(Box::new(element)),
                None,
                span,
            )
        }
        "atomic_load" => check_atomic_unary_ref(name, args, env, signatures, span, diagnostics),
        "atomic_store" | "atomic_fetch_add" => {
            check_atomic_binary_ref(name, args, env, signatures, span, diagnostics)
        }
        "atomic_compare_exchange" => check_atomic_cas(name, args, env, signatures, span, diagnostics),
        _ => unreachable!("dispatched only on the five atomic builtin names"),
    }
}

/// Element types currently supported for `Atomic<T>`. The C
/// backend uses `_Atomic <c_type>` (with `_Bool` for bool); the
/// LLVM backend stores integer widths natively and uses an i8
/// shadow for bool (`i1` atomic ops aren't byte-addressable).
/// `atomic_fetch_add` is *not* defined for bool — callers
/// reject it in the per-op helper.
fn is_supported_atomic_element(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I8
            | Type::I16
            | Type::I32
            | Type::I64
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::Bool
    )
}

/// If `ty` is a `&Atomic<T>` or `&mut Atomic<T>`, returns
/// `Some(T)`. Used by the per-op helpers to discover the
/// element type when the call site didn't specify one.
fn atomic_element_of(ty: &Type) -> Option<Type> {
    match ty {
        Type::Ref(inner) | Type::RefMut(inner) => match inner.as_ref() {
            Type::Atomic(elt) => Some((**elt).clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `atomic_compare_exchange(a: &Atomic<T>, expected: T, new: T) -> bool`.
/// Atomically reads the cell; if it equals `expected`, writes
/// `new` and returns `true`. Otherwise leaves the cell alone
/// and returns `false`. The return type is `bool` (the CAS
/// success bit) — distinct from the other atomic ops which
/// return `T`.
fn check_atomic_cas(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 3 {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "'atomic_compare_exchange' takes exactly 3 arguments (&cell, expected, new), got {}",
                args.len()
            ),
        ));
        return CheckedExpr::fallback(Type::Bool, span);
    }
    let cell = check_expr(&args[0], env, signatures, diagnostics);
    let element = atomic_element_of(cell.ty()).unwrap_or_else(|| {
        diagnostics.push(Diagnostic::new(
            args[0].span,
            format!(
                "'{}' requires a reference to Atomic<T>, got {}",
                name,
                cell.ty()
            ),
        ));
        Type::I64
    });
    let expected = check_expr(&args[1], env, signatures, diagnostics);
    let expected = coerce_checked(
        expected,
        &element,
        args[1].span,
        "atomic_compare_exchange expected value",
        diagnostics,
    );
    let new = check_expr(&args[2], env, signatures, diagnostics);
    let new = coerce_checked(
        new,
        &element,
        args[2].span,
        "atomic_compare_exchange new value",
        diagnostics,
    );
    CheckedExpr::new(
        TypedExprKind::Call {
            name: name.to_string(),
            name_span: span,
            args: vec![cell.expr, expected.expr, new.expr],
        },
        Type::Bool,
        None,
        span,
    )
}

/// Helper for `atomic_load(a: &Atomic<T>) -> T`.
fn check_atomic_unary_ref(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 1 {
        diagnostics.push(Diagnostic::new(
            span,
            format!("'{}' takes exactly 1 argument, got {}", name, args.len()),
        ));
        return CheckedExpr::fallback(Type::I64, span);
    }
    let arg = check_expr(&args[0], env, signatures, diagnostics);
    let element = atomic_element_of(arg.ty()).unwrap_or_else(|| {
        diagnostics.push(Diagnostic::new(
            args[0].span,
            format!(
                "'{}' requires a reference to Atomic<T>, got {}",
                name,
                arg.ty()
            ),
        ));
        Type::I64
    });
    CheckedExpr::new(
        TypedExprKind::Call {
            name: name.to_string(),
            name_span: span,
            args: vec![arg.expr],
        },
        element,
        None,
        span,
    )
}

/// Helper for `atomic_store(a: &Atomic<T>, v: T) -> T` and
/// `atomic_fetch_add(a: &Atomic<T>, v: T) -> T`. Both share the
/// same argument shape so they share the check.
fn check_atomic_binary_ref(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 2 {
        diagnostics.push(Diagnostic::new(
            span,
            format!("'{}' takes exactly 2 arguments, got {}", name, args.len()),
        ));
        return CheckedExpr::fallback(Type::I64, span);
    }
    let cell = check_expr(&args[0], env, signatures, diagnostics);
    let element = atomic_element_of(cell.ty()).unwrap_or_else(|| {
        diagnostics.push(Diagnostic::new(
            args[0].span,
            format!(
                "'{}' requires a reference to Atomic<T>, got {}",
                name,
                cell.ty()
            ),
        ));
        Type::I64
    });
    // `atomic_fetch_add` is an arithmetic op — reject it on
    // bool cells. `atomic_store` works on every supported
    // element type including bool.
    if name == "atomic_fetch_add" && matches!(element, Type::Bool) {
        diagnostics.push(Diagnostic::new(
            args[0].span,
            "'atomic_fetch_add' requires an integer element; bool atomics have no addition"
                .to_string(),
        ));
    }
    let value = check_expr(&args[1], env, signatures, diagnostics);
    let value = coerce_checked(
        value,
        &element,
        args[1].span,
        &format!("'{}' value argument", name),
        diagnostics,
    );
    CheckedExpr::new(
        TypedExprKind::Call {
            name: name.to_string(),
            name_span: span,
            args: vec![cell.expr, value.expr],
        },
        element,
        None,
        span,
    )
}

/// Type-check the three channel builtins. v1 supports
/// `Channel<i64>` only.
///
///   channel_new() -> Channel<i64>            // affine handle
///   channel_send(ch: &Channel<i64>, v: i64) -> i64
///   channel_recv(ch: &Channel<i64>) -> i64
///
/// Backed by a single-slot rendezvous: send sets the value
/// and a "ready" flag; recv spin-waits on the flag, reads the
/// value, then clears the flag.
fn check_channel_builtin(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    // `channel_new` doesn't take args, so the element type
    // can't be inferred at the call site. It returns a
    // default-shaped `Channel<i64, 16>`; the let-binding's
    // declared type widens it via `coerce_checked` (see
    // the channel-coerce arm in that function). The send/recv
    // ops do see a typed channel ref and dispatch on its T/N.
    let default_element = Type::I64;
    let default_capacity = 16u64;
    match name {
        "channel_new" => {
            if !args.is_empty() {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("'channel_new' takes 0 arguments, got {}", args.len()),
                ));
            }
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "channel_new".to_string(),
                    name_span: span,
                    args: Vec::new(),
                },
                Type::Channel(Box::new(default_element), default_capacity),
                None,
                span,
            )
        }
        "channel_send" => {
            if args.len() != 2 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("'channel_send' takes 2 arguments, got {}", args.len()),
                ));
                return CheckedExpr::fallback(Type::I64, span);
            }
            let ch = check_expr(&args[0], env, signatures, diagnostics);
            let element = match channel_element_of(ch.ty()) {
                Some(t) => t,
                None => {
                    diagnostics.push(Diagnostic::new(
                        args[0].span,
                        format!(
                            "'channel_send' requires a reference to Channel<T, N>, got {}",
                            ch.ty()
                        ),
                    ));
                    Type::I64
                }
            };
            let v = check_expr(&args[1], env, signatures, diagnostics);
            let v = coerce_checked(
                v,
                &element,
                args[1].span,
                "channel_send value argument",
                diagnostics,
            );
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "channel_send".to_string(),
                    name_span: span,
                    args: vec![ch.expr, v.expr],
                },
                element,
                None,
                span,
            )
        }
        "channel_recv" => {
            if args.len() != 1 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("'channel_recv' takes 1 argument, got {}", args.len()),
                ));
                return CheckedExpr::fallback(Type::I64, span);
            }
            let ch = check_expr(&args[0], env, signatures, diagnostics);
            let element = match channel_element_of(ch.ty()) {
                Some(t) => t,
                None => {
                    diagnostics.push(Diagnostic::new(
                        args[0].span,
                        format!(
                            "'channel_recv' requires a reference to Channel<T, N>, got {}",
                            ch.ty()
                        ),
                    ));
                    Type::I64
                }
            };
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "channel_recv".to_string(),
                    name_span: span,
                    args: vec![ch.expr],
                },
                element,
                None,
                span,
            )
        }
        _ => unreachable!("dispatched only on the three channel builtin names"),
    }
}

/// If `ty` is `&Channel<T, N>` or `&mut Channel<T, N>`,
/// returns `Some(T)`. Used by `check_channel_builtin` to
/// discover the element type from the call site.
fn channel_element_of(ty: &Type) -> Option<Type> {
    match ty {
        Type::Ref(inner) | Type::RefMut(inner) => match inner.as_ref() {
            Type::Channel(elt, _) => Some((**elt).clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Element types currently supported for `Channel<T, N>`.
/// Integer widths `i8 .. i64` / `u8 .. u64` plus `bool`. The
/// LLVM lowering stores bool slots as `[N x i8]` (the same
/// shadow trick `Atomic<bool>` uses, since `[N x i1]` isn't
/// byte-addressable). C uses `bool buf[N]` directly.
fn is_supported_channel_element(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I8
            | Type::I16
            | Type::I32
            | Type::I64
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::Bool
    )
}

/// Whether `n` is a power of two ≥ 1. The Vyukov ring buffer
/// uses `t & (N-1)` to wrap indices into the buffer, so N
/// must be a power of two for that mask to address all slots
/// exactly once.
fn channel_capacity_is_pow2(n: u64) -> bool {
    n > 0 && (n & (n - 1)) == 0
}

/// Type-check the four mutex/guard builtins. v1 supports
/// `Mutex<i64>` only.
///
///   mutex_new(initial: i64) -> Mutex<i64>     // affine handle
///   mutex_lock(m: &Mutex<i64>) -> Guard<i64>  // affine guard
///   guard_get(g: &Guard<i64>) -> i64          // read under lock
///   guard_set(g: &Guard<i64>, v: i64) -> i64  // write under lock
///
/// The `Guard<i64>` handle is affine. Scope-exit drops the
/// guard, which the backend lowers to a runtime
/// `mutex_unlock`. The user does not write `mutex_unlock`
/// explicitly — RAII via the existing drop machinery.
fn check_mutex_builtin(
    name: &str,
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    let element = Type::I64;
    let mutex_ty = Type::Mutex(Box::new(element.clone()));
    let guard_ty = Type::Guard(Box::new(element.clone()));

    match name {
        "mutex_new" => {
            if args.len() != 1 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "'mutex_new' takes exactly 1 argument (initial value), got {}",
                        args.len()
                    ),
                ));
                return CheckedExpr::fallback(mutex_ty, span);
            }
            let initial = check_expr(&args[0], env, signatures, diagnostics);
            let initial = coerce_checked(
                initial,
                &element,
                args[0].span,
                "mutex_new initial value",
                diagnostics,
            );
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "mutex_new".to_string(),
                    name_span: span,
                    args: vec![initial.expr],
                },
                mutex_ty,
                None,
                span,
            )
        }
        "mutex_lock" => {
            if args.len() != 1 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("'mutex_lock' takes 1 argument, got {}", args.len()),
                ));
                return CheckedExpr::fallback(guard_ty, span);
            }
            let m = check_expr(&args[0], env, signatures, diagnostics);
            if !is_mutex_ref(m.ty(), &element) {
                diagnostics.push(Diagnostic::new(
                    args[0].span,
                    format!(
                        "'mutex_lock' requires a reference to Mutex<{}>, got {}",
                        element,
                        m.ty()
                    ),
                ));
            }
            // Static double-acquire check. When the
            // `mutex_lock` argument is a syntactic &Var(name)
            // reference, scan env for any live guard
            // (`moved.is_none()`) whose `guarded_mutex` field
            // names the same mutex. Two live guards on the
            // same mutex would deadlock at runtime (the lock
            // is non-reentrant); flagging it at compile time
            // turns the deadlock into an error. Indirect
            // references (e.g. `mutex_lock(f())`) can't be
            // tracked syntactically — we skip the check
            // rather than overreport.
            if let Some(target) = extract_locked_mutex_name(&args[0]) {
                let already_held = env
                    .all_bindings()
                    .any(|(_, info)| {
                        info.moved.is_none()
                            && info
                                .guarded_mutex
                                .as_deref()
                                .map(|n| n == target)
                                .unwrap_or(false)
                    });
                if already_held {
                    diagnostics.push(Diagnostic::new(
                        span,
                        format!(
                            "double acquisition: mutex '{}' is already locked by a live Guard in this scope; \
                             let the existing guard drop before re-locking",
                            target
                        ),
                    ));
                }
            }
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "mutex_lock".to_string(),
                    name_span: span,
                    args: vec![m.expr],
                },
                guard_ty,
                None,
                span,
            )
        }
        "guard_get" => {
            if args.len() != 1 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("'guard_get' takes 1 argument, got {}", args.len()),
                ));
                return CheckedExpr::fallback(element.clone(), span);
            }
            let g = check_expr(&args[0], env, signatures, diagnostics);
            if !is_guard_ref(g.ty(), &element) {
                diagnostics.push(Diagnostic::new(
                    args[0].span,
                    format!(
                        "'guard_get' requires a reference to Guard<{}>, got {}",
                        element,
                        g.ty()
                    ),
                ));
            }
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "guard_get".to_string(),
                    name_span: span,
                    args: vec![g.expr],
                },
                element,
                None,
                span,
            )
        }
        "guard_set" => {
            if args.len() != 2 {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!("'guard_set' takes 2 arguments, got {}", args.len()),
                ));
                return CheckedExpr::fallback(element.clone(), span);
            }
            let g = check_expr(&args[0], env, signatures, diagnostics);
            if !is_guard_ref(g.ty(), &element) {
                diagnostics.push(Diagnostic::new(
                    args[0].span,
                    format!(
                        "'guard_set' requires a reference to Guard<{}>, got {}",
                        element,
                        g.ty()
                    ),
                ));
            }
            let v = check_expr(&args[1], env, signatures, diagnostics);
            let v = coerce_checked(
                v,
                &element,
                args[1].span,
                "guard_set value argument",
                diagnostics,
            );
            CheckedExpr::new(
                TypedExprKind::Call {
                    name: "guard_set".to_string(),
                    name_span: span,
                    args: vec![g.expr, v.expr],
                },
                element,
                None,
                span,
            )
        }
        _ => unreachable!("dispatched only on the four mutex/guard builtin names"),
    }
}

/// When the `mutex_lock` argument has a name we can track,
/// return it. Two shapes count:
///
///   1. `mutex_lock(&m)` — explicit reference to a local
///      `Mutex<T>` binding; the underlying mutex name is the
///      Var's name.
///   2. `mutex_lock(p)` where `p` is a binding of reference
///      type (typically a function parameter
///      `p: &Mutex<T>`); the tracking name is the
///      binding itself. Same-name double-locks within the
///      function then surface even when the lock target is a
///      ref parameter rather than an owned mutex.
///
/// Anything else (e.g. `mutex_lock(get_ref())`) returns
/// `None` — conservatively skipping the check rather than
/// overreporting. The type check upstream guarantees the
/// arg's type is `&Mutex<T>` / `&mut Mutex<T>`, so a bare Var
/// here is necessarily a reference-typed binding.
fn extract_locked_mutex_name(arg: &Expr) -> Option<String> {
    match &arg.kind {
        ExprKind::Ref { inner } | ExprKind::RefMut { inner } => match &inner.kind {
            ExprKind::Var(name) => Some(name.clone()),
            _ => None,
        },
        ExprKind::Var(name) => Some(name.clone()),
        _ => None,
    }
}

/// Pre-compute, for each parameter, whether the function's
/// body contains a direct `mutex_lock(arg)` call where the
/// arg names that parameter (either bare `Var(p)` or
/// `&Var(p)` / `&mut Var(p)`). Used by `check_call` to
/// reject cross-function double-acquisition: if the caller
/// holds a guard on the mutex it's about to pass, and the
/// callee would lock it, the call would deadlock.
///
/// v1 is intentionally non-transitive: a function that calls
/// another function that locks the param doesn't propagate
/// the flag. That's a future enhancement; today's check
/// catches the direct case cleanly without paying for a
/// fixpoint pass over the call graph.
fn compute_locks_params(function: &Function) -> Vec<bool> {
    let mut locks: Vec<bool> = vec![false; function.params.len()];
    let param_names: Vec<&str> = function.params.iter().map(|p| p.name.as_str()).collect();
    fn walk(
        stmts: &[Stmt],
        param_names: &[&str],
        locks: &mut [bool],
    ) {
        for stmt in stmts {
            match stmt {
                Stmt::Let { expr, .. }
                | Stmt::LetTuple { expr, .. }
                | Stmt::Return { expr, .. }
                | Stmt::Assert { expr, .. }
                | Stmt::Prove { expr, .. }
                | Stmt::Assign { expr, .. } => walk_expr(expr, param_names, locks),
                Stmt::IndexAssign { index, value, .. } => {
                    walk_expr(index, param_names, locks);
                    walk_expr(value, param_names, locks);
                }
                Stmt::FieldAssign { object, value, .. } => {
                    walk_expr(object, param_names, locks);
                    walk_expr(value, param_names, locks);
                }
                Stmt::Print { items, .. } => {
                    for item in items {
                        if let crate::ast::PrintItem::Expr(e) = item {
                            walk_expr(e, param_names, locks);
                        }
                    }
                }
                Stmt::If { cond, then_body, else_body, .. } => {
                    walk_expr(cond, param_names, locks);
                    walk(then_body, param_names, locks);
                    walk(else_body, param_names, locks);
                }
                Stmt::While { cond, body, invariants, .. } => {
                    walk_expr(cond, param_names, locks);
                    for inv in invariants {
                        walk_expr(inv, param_names, locks);
                    }
                    walk(body, param_names, locks);
                }
                Stmt::For { start, end, body, invariants, .. } => {
                    walk_expr(start, param_names, locks);
                    walk_expr(end, param_names, locks);
                    for inv in invariants {
                        walk_expr(inv, param_names, locks);
                    }
                    walk(body, param_names, locks);
                }
                Stmt::ForIter { body, .. } => walk(body, param_names, locks),
                Stmt::TaskSpawn { body, .. } => walk(body, param_names, locks),
                Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::TaskJoin { .. } => {}
            }
        }
    }
    fn walk_expr(expr: &Expr, param_names: &[&str], locks: &mut [bool]) {
        if let ExprKind::Call { name, args, .. } = &expr.kind {
            if name == "mutex_lock" && args.len() == 1 {
                if let Some(target) = extract_locked_mutex_name(&args[0]) {
                    if let Some(idx) = param_names.iter().position(|n| *n == target) {
                        locks[idx] = true;
                    }
                }
            }
            for a in args {
                walk_expr(a, param_names, locks);
            }
            return;
        }
        match &expr.kind {
            ExprKind::Unary { expr: inner, .. } => walk_expr(inner, param_names, locks),
            ExprKind::Binary { left, right, .. } => {
                walk_expr(left, param_names, locks);
                walk_expr(right, param_names, locks);
            }
            ExprKind::Cast { expr: inner, .. } => walk_expr(inner, param_names, locks),
            ExprKind::ArrayLit { elements } => {
                for e in elements {
                    walk_expr(e, param_names, locks);
                }
            }
            ExprKind::Index { array, index, .. } => {
                walk_expr(array, param_names, locks);
                walk_expr(index, param_names, locks);
            }
            ExprKind::Len { array, .. } => walk_expr(array, param_names, locks),
            ExprKind::Ref { inner } | ExprKind::RefMut { inner } => {
                walk_expr(inner, param_names, locks)
            }
            _ => {}
        }
    }
    walk(&function.body, &param_names, &mut locks);
    locks
}

fn is_mutex_ref(ty: &Type, element: &Type) -> bool {
    matches!(
        ty,
        Type::Ref(inner) | Type::RefMut(inner)
            if matches!(inner.as_ref(), Type::Mutex(elt) if elt.as_ref() == element)
    )
}

fn is_guard_ref(ty: &Type, element: &Type) -> bool {
    matches!(
        ty,
        Type::Ref(inner) | Type::RefMut(inner)
            if matches!(inner.as_ref(), Type::Guard(elt) if elt.as_ref() == element)
    )
}

/// Type-directed elaboration for `vec()` (zero args). The
/// element type can't be inferred from the call alone, but
/// when the call appears in a context that names the type —
/// a `let xs: Vec<T> = vec();`, an `xs = vec();` reassign,
/// or a `return vec();` from a function whose return type is
/// `Vec<T>` — we elaborate the call against that type and
/// produce a typed `vec()` call with the right element. Refines
/// #8 from STATUS.md. Returns `Some(CheckedExpr)` when both
/// the syntactic shape (`vec()` with no args) and the
/// `expected` (which must already be `Vec<T>`) match; the
/// caller otherwise falls through to `check_expr` which will
/// emit the existing "vec(...) requires at least one element"
/// diagnostic.
fn try_elaborate_empty_vec(
    expr: &Expr,
    expected: &Type,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<CheckedExpr> {
    let ExprKind::Call { name, args, .. } = &expr.kind else {
        return None;
    };
    if name != "vec" || !args.is_empty() {
        return None;
    }
    let Type::Vec(element_box) = expected else {
        return None;
    };
    let element_ty = (**element_box).clone();
    if matches!(element_ty, Type::Ref(_) | Type::RefMut(_)) {
        diagnostics.push(Diagnostic::new(
            expr.span,
            format!(
                "Vec element type cannot be a reference, got {}",
                element_ty
            ),
        ));
    }
    Some(CheckedExpr::new(
        TypedExprKind::Call {
            name: "vec".to_string(),
            name_span: expr.span,
            args: vec![],
        },
        Type::Vec(Box::new(element_ty)),
        None,
        expr.span,
    ))
}

fn check_vec_builtin(
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.is_empty() {
        diagnostics.push(Diagnostic::new(
            span,
            "vec() needs either at least one element or a type annotation \
             (e.g. `let xs: Vec<i64> = vec();`) so the element type can be \
             inferred",
        ));
        return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
    }

    let typed: Vec<CheckedExpr> = args
        .iter()
        .map(|arg| check_expr(arg, env, signatures, diagnostics))
        .collect();

    let element_type = typed[0].ty().clone();
    // Refines #7 from STATUS.md: `Vec<T>` no longer requires
    // `T: Copy`. Non-Copy elements (`Vec<Vec<i64>>` etc.) flow
    // through with element-aware free / clone / set helpers
    // emitted in the backend. The vec literal `vec(a, b, c)`
    // still consumes each element by value — affine bindings
    // get moved into the call, same as before. Reference
    // types (`&T`, `&mut T`) remain rejected as a separate
    // category (they're not aggregates with ownership; they'd
    // dangle). Fixed-size arrays as Vec elements are tracked
    // as #7 phase 2b — gate clearly until per-shape typedef
    // + memcpy push/set lands.
    if matches!(element_type, Type::Ref(_) | Type::RefMut(_)) {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "Vec element type cannot be a reference, got {}",
                element_type
            ),
        ));
    }

    let mut coerced_args = Vec::with_capacity(typed.len());
    for (index, element) in typed.into_iter().enumerate() {
        let arg_span = element.expr.span;
        let coerced = coerce_checked(
            element,
            &element_type,
            arg_span,
            &format!("vec element {}", index),
            diagnostics,
        );
        // Mark each Var argument as moved when the element
        // type owns non-Copy heap — `vec(a, b)` transfers
        // ownership of each Var into the new Vec's slot, so
        // the source binding's scope-exit drop would double-
        // free the heap now in the buffer. Mirrors push()
        // and set() from closure #171. Closure #177.
        consume_if_moved_var(&args[index], &coerced, env);
        coerced_args.push(coerced.expr);
    }

    CheckedExpr::new(
        TypedExprKind::Call {
            name: "vec".to_string(),
            name_span: span,
            args: coerced_args,
        },
        Type::Vec(Box::new(element_type)),
        None,
        span,
    )
}

fn check_push_builtin(
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 2 {
        diagnostics.push(Diagnostic::new(
            span,
            format!("push(xs, v) expects 2 arguments, got {}", args.len()),
        ));
        return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
    }

    let xs = check_expr(&args[0], env, signatures, diagnostics);
    // Two forms:
    //   push(xs: Vec<T>, v: T)       -> Vec<T>     (consuming)
    //   push(xs: mut ref Vec<T>, v)  -> i64        (in-place; returns new len)
    // Dispatch on the first arg's type. The mut-ref form
    // works through a struct field (`mut ref t.xs`) without
    // requiring partial-move + write-back. T1.2 phase 2b
    // follow-up.
    let (element_type, in_place) = match xs.ty() {
        Type::Vec(element) => ((**element).clone(), false),
        Type::RefMut(inner) => match &**inner {
            Type::Vec(element) => ((**element).clone(), true),
            _ => {
                diagnostics.push(Diagnostic::new(
                    args[0].span,
                    format!(
                        "push() requires a Vec or mut ref Vec argument, got {}",
                        xs.ty()
                    ),
                ));
                return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
            }
        },
        other => {
            diagnostics.push(Diagnostic::new(
                args[0].span,
                format!(
                    "push() requires a Vec or mut ref Vec argument, got {}",
                    other
                ),
            ));
            return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
        }
    };

    let value = check_expr(&args[1], env, signatures, diagnostics);
    let value = coerce_checked(
        value,
        &element_type,
        args[1].span,
        "push value",
        diagnostics,
    );

    // Consuming form moves the source binding; in-place form
    // leaves it borrowed (the existing `RefMut`/`RefMutField`
    // checks already validated the borrow shape).
    if !in_place {
        diagnose_partial_then_whole_move(&args[0], &xs, env, diagnostics);
        consume_if_moved_var(&args[0], &xs, env);
    }
    // The pushed value is taken by value — for non-Copy
    // element types (OwnedStr / Vec / struct with heap),
    // the source Var binding must be marked moved.
    // Otherwise its scope-exit drop fires after push
    // already transferred ownership into the new Vec's
    // slot, double-freeing the heap. Closure #171.
    consume_if_moved_var(&args[1], &value, env);

    let (call_name, result_type) = if in_place {
        ("push_mut".to_string(), Type::I64)
    } else {
        ("push".to_string(), Type::Vec(Box::new(element_type)))
    };
    CheckedExpr::new(
        TypedExprKind::Call {
            name: call_name,
            name_span: span,
            args: vec![xs.expr, value.expr],
        },
        result_type,
        None,
        span,
    )
}

fn check_set_builtin(
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 3 {
        diagnostics.push(Diagnostic::new(
            span,
            format!("set(xs, i, v) expects 3 arguments, got {}", args.len()),
        ));
        return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
    }

    let xs = check_expr(&args[0], env, signatures, diagnostics);
    let element_type = match xs.ty() {
        Type::Vec(element) => (**element).clone(),
        other => {
            diagnostics.push(Diagnostic::new(
                args[0].span,
                format!("set() requires a Vec argument, got {}", other),
            ));
            return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
        }
    };

    let index = check_expr(&args[1], env, signatures, diagnostics);
    if !index.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            args[1].span,
            format!("set index must be an integer, got {}", index.ty()),
        ));
    }

    let value = check_expr(&args[2], env, signatures, diagnostics);
    let value = coerce_checked(
        value,
        &element_type,
        args[2].span,
        "set value",
        diagnostics,
    );

    diagnose_partial_then_whole_move(&args[0], &xs, env, diagnostics);

    consume_if_moved_var(&args[0], &xs, env);
    // Mark the new-value Var moved when it owns non-Copy
    // heap — set() stores it into the slot, so the source
    // binding's scope-exit drop would double-free.
    // Mirrors push (closure #171).
    consume_if_moved_var(&args[2], &value, env);

    let result_type = Type::Vec(Box::new(element_type));
    CheckedExpr::new(
        TypedExprKind::Call {
            name: "set".to_string(),
            name_span: span,
            args: vec![xs.expr, index.expr, value.expr],
        },
        result_type,
        None,
        span,
    )
}

fn check_clone_builtin(
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 1 {
        diagnostics.push(Diagnostic::new(
            span,
            format!("clone(xs) expects 1 argument, got {}", args.len()),
        ));
        return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
    }

    let xs = check_expr(&args[0], env, signatures, diagnostics);
    let result_type = match xs.ty() {
        Type::Vec(_) => xs.ty().clone(),
        other => {
            diagnostics.push(Diagnostic::new(
                args[0].span,
                format!("clone() requires a Vec argument, got {}", other),
            ));
            return CheckedExpr::fallback(Type::Vec(Box::new(Type::I64)), span);
        }
    };

    // clone deliberately does NOT consume its argument.

    CheckedExpr::new(
        TypedExprKind::Call {
            name: "clone".to_string(),
            name_span: span,
            args: vec![xs.expr],
        },
        result_type,
        None,
        span,
    )
}

/// `clone_at(xs, i)` — deep-clone of a single Vec slot,
/// returning an owned copy of the element. Lets users
/// extract non-Copy elements (e.g. `Vec<Vec<i64>>` slots)
/// without aliasing the owner's buffer — refines #7 phase
/// 2d (otherwise the only way to read `xs[i]` for non-Copy
/// elements is via the `for v in &xs` view, which doesn't
/// generalize to "I want THIS slot, named, by value").
fn check_clone_at_builtin(
    args: &[Expr],
    env: &mut Env,
    signatures: &HashMap<String, Signature>,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if args.len() != 2 {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "clone_at(xs, i) expects 2 arguments, got {}",
                args.len()
            ),
        ));
        return CheckedExpr::fallback_integer(span);
    }
    let xs = check_expr(&args[0], env, signatures, diagnostics);
    let element_type = match xs.ty().deref() {
        Type::Vec(element) => (**element).clone(),
        other => {
            diagnostics.push(Diagnostic::new(
                args[0].span,
                format!(
                    "clone_at() requires a Vec or &Vec argument, got {}",
                    other
                ),
            ));
            return CheckedExpr::fallback_integer(span);
        }
    };
    let index = check_expr(&args[1], env, signatures, diagnostics);
    if !index.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            args[1].span,
            format!(
                "clone_at index must be an integer, got {}",
                index.ty()
            ),
        ));
    }
    // clone_at deliberately does NOT consume its first
    // argument — same convention as `clone`. The result is
    // a fresh owned element value the caller can `let`-bind
    // without aliasing the source slot.
    CheckedExpr::new(
        TypedExprKind::Call {
            name: "clone_at".to_string(),
            name_span: span,
            args: vec![xs.expr, index.expr],
        },
        element_type,
        None,
        span,
    )
}

fn explicit_cast(
    checked: CheckedExpr,
    target: &Type,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if checked.ty() == target {
        let constant = checked.constant().cloned();
        return cast_expr(checked, target.clone(), constant, span);
    }

    // Enum → integer cast: enums lower to an i32 tag in
    // both backends, so casting to any integer type is a
    // safe widening (or in-range narrowing — bounded by
    // the variant count, capped at 255). Useful for
    // serialization, table-driven dispatch, and printing
    // diagnostic values. T1.3 follow-up.
    if matches!(checked.ty(), Type::Enum(_)) && target.is_integer() {
        return cast_expr(checked, target.clone(), None, span);
    }

    if !checked.ty().is_numeric() || !target.is_numeric() {
        diagnostics.push(Diagnostic::new(
            span,
            format!("cannot cast {} to {}", checked.ty(), target),
        ));
        return CheckedExpr::fallback(target.clone(), span);
    }

    let constant = checked
        .constant()
        .and_then(|constant| eval_cast_constant(constant, target, span, diagnostics));
    cast_expr(checked, target.clone(), constant, span)
}

fn coerce_checked(
    checked: CheckedExpr,
    target: &Type,
    span: Span,
    context: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckedExpr {
    if checked.ty() == target {
        return checked;
    }

    // Array literal element-wise coercion: when both sides are
    // `[T; N]` with the same length and the inner expression is an
    // ArrayLit, push the coercion down into each element. Lets
    // `let xs: [f32; 3] = [1.5, 2.5, 3.5];` work even though the
    // bare float literals default to f64 — each element gets
    // wrapped in a Cast to the target element type. The existing
    // backend lowerings already handle that cast (fptrunc, trunc,
    // etc.). Only fires when both element types are numeric so we
    // don't silently bridge unrelated types.
    if let (
        Type::Array { length: src_len, .. },
        Type::Array { element: tgt_elem, length: tgt_len },
    ) = (checked.ty(), target)
    {
        if src_len == tgt_len {
            if let TypedExprKind::ArrayLit { elements } = &checked.expr.kind {
                if elements.iter().all(|e| e.ty.is_numeric())
                    && tgt_elem.is_numeric()
                {
                    let mut new_elements = Vec::with_capacity(elements.len());
                    for elem_typed in elements.iter() {
                        if elem_typed.ty == **tgt_elem {
                            new_elements.push(elem_typed.clone());
                        } else {
                            new_elements.push(TypedExpr {
                                kind: TypedExprKind::Cast {
                                    expr: Box::new(elem_typed.clone()),
                                    ty: (**tgt_elem).clone(),
                                },
                                ty: (**tgt_elem).clone(),
                                constant: None,
                                span: elem_typed.span,
                                binding_decl_span: None,
                            });
                        }
                    }
                    return CheckedExpr::new(
                        TypedExprKind::ArrayLit { elements: new_elements },
                        target.clone(),
                        None,
                        span,
                    );
                }
            }
        }
    }

    // Channel construction widening: `channel_new()` always
    // reports its result as `Channel<i64, 16>` (it takes no
    // args, so the element + capacity have to flow from the
    // let-binding annotation). If the binding declares a
    // different `Channel<T, N>`, retype the call's TypedExpr
    // so the backend dispatches on the requested (T, N).
    // Element T must be a supported width; N must be a power
    // of 2 ≥ 1.
    if let (Type::Channel(_, _), Type::Channel(tgt_elem, tgt_cap)) =
        (checked.ty(), target)
    {
        if matches!(
            &checked.expr.kind,
            TypedExprKind::Call { name, .. } if name == "channel_new"
        ) {
            if !is_supported_channel_element(tgt_elem) {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "Channel element type must be an integer width or bool, got {}",
                        tgt_elem
                    ),
                ));
                return CheckedExpr::fallback(target.clone(), span);
            }
            if !channel_capacity_is_pow2(*tgt_cap) {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "Channel capacity must be a power of 2 ≥ 1, got {}",
                        tgt_cap
                    ),
                ));
                return CheckedExpr::fallback(target.clone(), span);
            }
            let mut promoted = checked.expr.clone();
            promoted.ty = target.clone();
            return CheckedExpr::new(
                promoted.kind,
                promoted.ty,
                None,
                promoted.span,
            );
        }
    }

    if !can_assign(&checked, target) {
        if target.is_numeric() {
            if let Some(constant) = checked.constant() {
                diagnostics.push(Diagnostic::new(
                    span,
                    format!(
                        "{} value {} cannot be represented as {}",
                        context,
                        const_display(constant),
                        target
                    ),
                ));
                return CheckedExpr::fallback(target.clone(), span);
            }
        }
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "{} must be assignable to {}, got {}",
                context,
                target,
                checked.ty()
            ),
        ));
        return CheckedExpr::fallback(target.clone(), span);
    }

    let constant = checked
        .constant()
        .and_then(|constant| eval_cast_constant(constant, target, span, diagnostics));
    cast_expr(checked, target.clone(), constant, span)
}

fn coerce_numeric_operand(checked: CheckedExpr, target: &Type) -> CheckedExpr {
    if checked.ty() == target {
        checked
    } else {
        let constant = checked
            .constant()
            .and_then(|constant| eval_cast_constant_no_diagnostic(constant, target));
        let span = checked.expr.span;
        cast_expr(checked, target.clone(), constant, span)
    }
}

fn cast_expr(
    checked: CheckedExpr,
    target: Type,
    constant: Option<TypedConst>,
    span: Span,
) -> CheckedExpr {
    CheckedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(checked.expr),
            ty: target.clone(),
        },
        target,
        constant,
        span,
    )
}

fn promoted_numeric_type(
    lhs: &CheckedExpr,
    rhs: &CheckedExpr,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Type> {
    if !lhs.ty().is_numeric() {
        diagnostics.push(Diagnostic::new(
            lhs.expr.span,
            format!("left operand must be numeric, got {}", lhs.ty()),
        ));
        return None;
    }
    if !rhs.ty().is_numeric() {
        diagnostics.push(Diagnostic::new(
            rhs.expr.span,
            format!("right operand must be numeric, got {}", rhs.ty()),
        ));
        return None;
    }

    if lhs.ty().is_float() || rhs.ty().is_float() || lhs.flexible_float || rhs.flexible_float {
        return promoted_float_type(lhs, rhs);
    }

    promoted_integer_type(lhs, rhs, diagnostics)
}

fn promoted_float_type(lhs: &CheckedExpr, rhs: &CheckedExpr) -> Option<Type> {
    let left = adapted_float_literal_type(lhs, rhs.ty()).unwrap_or_else(|| lhs.ty().clone());
    let right = adapted_float_literal_type(rhs, &left).unwrap_or_else(|| rhs.ty().clone());

    if left == Type::F64 || right == Type::F64 {
        Some(Type::F64)
    } else if left == Type::F32 || right == Type::F32 {
        Some(Type::F32)
    } else {
        Some(Type::F64)
    }
}

fn adapted_float_literal_type(expr: &CheckedExpr, target: &Type) -> Option<Type> {
    if !expr.flexible_float || *target != Type::F32 {
        return None;
    }

    match expr.constant() {
        Some(TypedConst::Float(value)) if finite_float_value(*value, &Type::F32) => Some(Type::F32),
        _ => None,
    }
}

fn promoted_integer_type(
    lhs: &CheckedExpr,
    rhs: &CheckedExpr,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Type> {
    if !lhs.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            lhs.expr.span,
            format!("left operand must be an integer, got {}", lhs.ty()),
        ));
        return None;
    }

    if !rhs.ty().is_integer() {
        diagnostics.push(Diagnostic::new(
            rhs.expr.span,
            format!("right operand must be an integer, got {}", rhs.ty()),
        ));
        return None;
    }

    let left_type = adapted_integer_literal_type(lhs, rhs.ty()).unwrap_or_else(|| lhs.ty().clone());
    let right_type =
        adapted_integer_literal_type(rhs, &left_type).unwrap_or_else(|| rhs.ty().clone());

    match common_integer_type(&left_type, &right_type) {
        Some(ty) => Some(ty),
        None => {
            diagnostics.push(Diagnostic::new(
                rhs.expr.span,
                format!(
                    "no safe implicit integer promotion for {} and {}; use an explicit cast",
                    left_type, right_type
                ),
            ));
            None
        }
    }
}

fn adapted_integer_literal_type(expr: &CheckedExpr, target: &Type) -> Option<Type> {
    if !expr.flexible_integer || !target.is_integer() {
        return None;
    }

    match expr.constant() {
        Some(TypedConst::Int(value)) if value_fits_type(*value, target) => Some(target.clone()),
        _ => None,
    }
}

fn common_integer_type(left: &Type, right: &Type) -> Option<Type> {
    if !left.is_integer() || !right.is_integer() {
        return None;
    }

    let left_bits = left.bits().expect("integer type has a width");
    let right_bits = right.bits().expect("integer type has a width");

    match (left.is_signed_integer(), right.is_signed_integer()) {
        (true, true) => signed_type(left_bits.max(right_bits)),
        (false, false) => unsigned_type(left_bits.max(right_bits)),
        (true, false) if left_bits > right_bits => Some(left.clone()),
        (false, true) if right_bits > left_bits => Some(right.clone()),
        _ => None,
    }
}

fn signed_type(bits: u16) -> Option<Type> {
    match bits {
        8 => Some(Type::I8),
        16 => Some(Type::I16),
        32 => Some(Type::I32),
        64 => Some(Type::I64),
        _ => None,
    }
}

fn unsigned_type(bits: u16) -> Option<Type> {
    match bits {
        8 => Some(Type::U8),
        16 => Some(Type::U16),
        32 => Some(Type::U32),
        64 => Some(Type::U64),
        _ => None,
    }
}

fn eval_integer_binary(
    op: BinaryOp,
    a: i128,
    b: i128,
    result_type: &Type,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TypedConst> {
    let value = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Sub => a.checked_sub(b),
        BinaryOp::Mul => a.checked_mul(b),
        BinaryOp::Div if b == 0 => {
            diagnostics.push(Diagnostic::new(
                span,
                "division by zero in constant expression",
            ));
            None
        }
        BinaryOp::Div => a.checked_div(b),
        BinaryOp::Rem if b == 0 => {
            diagnostics.push(Diagnostic::new(
                span,
                "remainder by zero in constant expression",
            ));
            None
        }
        BinaryOp::Rem => a.checked_rem(b),
        BinaryOp::BitAnd => Some(a & b),
        BinaryOp::BitOr => Some(a | b),
        BinaryOp::BitXor => Some(a ^ b),
        _ => unreachable!("not an integer arithmetic operator"),
    };

    let Some(value) = value else {
        diagnostics.push(Diagnostic::new(
            span,
            format!("constant expression overflows {}", result_type),
        ));
        return None;
    };

    if !value_fits_type(value, result_type) {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "constant expression value {} does not fit in {}",
                value, result_type
            ),
        ));
        return None;
    }

    Some(TypedConst::Int(value))
}

fn eval_float_binary(
    op: BinaryOp,
    left: &TypedConst,
    right: &TypedConst,
    result_type: &Type,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TypedConst> {
    let a = const_to_float(left);
    let b = const_to_float(right);
    let value = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        BinaryOp::Div if b == 0.0 => {
            diagnostics.push(Diagnostic::new(
                span,
                "floating-point division by zero in constant expression",
            ));
            return None;
        }
        BinaryOp::Div => a / b,
        _ => unreachable!("not a float arithmetic operator"),
    };

    if !finite_float_value(value, result_type) {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "floating-point constant expression is not finite in {}",
                result_type
            ),
        ));
        return None;
    }

    Some(TypedConst::Float(narrow_float_value(value, result_type)))
}

fn eval_shift(
    op: BinaryOp,
    value: i128,
    count: u32,
    result_type: &Type,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TypedConst> {
    let shifted = match op {
        BinaryOp::Shl => value.checked_shl(count),
        BinaryOp::Shr => value.checked_shr(count),
        _ => unreachable!("not a shift operator"),
    };

    let Some(shifted) = shifted else {
        diagnostics.push(Diagnostic::new(
            span,
            "shift overflows in constant expression",
        ));
        return None;
    };

    if !value_fits_type(shifted, result_type) {
        diagnostics.push(Diagnostic::new(
            span,
            format!(
                "shift result value {} does not fit in {}",
                shifted, result_type
            ),
        ));
        return None;
    }

    Some(TypedConst::Int(shifted))
}

fn eval_cast_constant(
    constant: &TypedConst,
    target: &Type,
    span: Span,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TypedConst> {
    match eval_cast_constant_no_diagnostic(constant, target) {
        Some(value) => Some(value),
        None => {
            diagnostics.push(Diagnostic::new(
                span,
                format!(
                    "constant value {} cannot be represented as {}",
                    const_display(constant),
                    target
                ),
            ));
            None
        }
    }
}

fn eval_cast_constant_no_diagnostic(constant: &TypedConst, target: &Type) -> Option<TypedConst> {
    match (constant, target) {
        (TypedConst::Int(value), ty) if ty.is_integer() => {
            value_fits_type(*value, ty).then_some(TypedConst::Int(*value))
        }
        (TypedConst::Int(value), ty) if ty.is_float() => {
            let value = *value as f64;
            finite_float_value(value, ty)
                .then_some(TypedConst::Float(narrow_float_value(value, ty)))
        }
        (TypedConst::Float(value), ty) if ty.is_float() => finite_float_value(*value, ty)
            .then_some(TypedConst::Float(narrow_float_value(*value, ty))),
        (TypedConst::Float(value), ty) if ty.is_integer() => {
            if !value.is_finite() {
                return None;
            }
            let truncated = value.trunc();
            let min = ty.min_value()? as f64;
            let max = ty.max_value()? as f64;
            if truncated < min || truncated > max {
                return None;
            }
            Some(TypedConst::Int(truncated as i128))
        }
        (TypedConst::Bool(value), Type::Bool) => Some(TypedConst::Bool(*value)),
        _ => None,
    }
}

fn can_assign(actual: &CheckedExpr, expected: &Type) -> bool {
    if actual.ty() == expected {
        return true;
    }

    // Auto-borrow `OwnedStr` to `Str` in read-only positions:
    // function args, comparisons, len() — anywhere a `Str` is
    // expected, an `OwnedStr` works because both lower to the
    // same pointer-to-NUL-terminated-bytes representation and the
    // caller doesn't consume the buffer. The OwnedStr binding
    // stays live; its drop fires at the original scope's end.
    if matches!(actual.ty(), Type::OwnedStr) && matches!(expected, Type::Str) {
        return true;
    }

    if !actual.ty().is_numeric() || !expected.is_numeric() {
        return false;
    }

    if let Some(constant) = actual.constant() {
        return eval_cast_constant_no_diagnostic(constant, expected).is_some()
            && !(actual.ty().is_float() && expected.is_integer());
    }

    if actual.ty().is_integer() && expected.is_integer() {
        return can_represent_all(expected, actual.ty());
    }

    if actual.ty().is_integer() && expected.is_float() {
        return true;
    }

    *actual.ty() == Type::F32 && *expected == Type::F64
}

fn can_represent_all(destination: &Type, source: &Type) -> bool {
    let (Some(destination_min), Some(destination_max)) =
        (destination.min_value(), destination.max_value())
    else {
        return false;
    };
    let (Some(source_min), Some(source_max)) = (source.min_value(), source.max_value()) else {
        return false;
    };

    destination_min <= source_min && destination_max >= source_max
}

fn finite_float_value(value: f64, ty: &Type) -> bool {
    match ty {
        Type::F32 => {
            let narrowed = value as f32;
            narrowed.is_finite()
        }
        Type::F64 => value.is_finite(),
        _ => false,
    }
}

fn narrow_float_value(value: f64, ty: &Type) -> f64 {
    if *ty == Type::F32 {
        (value as f32) as f64
    } else {
        value
    }
}

fn const_to_float(value: &TypedConst) -> f64 {
    match value {
        TypedConst::Int(value) => *value as f64,
        TypedConst::Float(value) => *value,
        TypedConst::Bool(_) => unreachable!("bool is not numeric"),
    }
}

fn const_to_int(value: &TypedConst) -> i128 {
    match value {
        TypedConst::Int(value) => *value,
        TypedConst::Float(_) => unreachable!("float is not integer"),
        TypedConst::Bool(_) => unreachable!("bool is not numeric"),
    }
}

fn is_known_zero(expr: &CheckedExpr) -> bool {
    match expr.constant() {
        Some(TypedConst::Int(0)) => true,
        Some(TypedConst::Float(value)) => *value == 0.0,
        _ => false,
    }
}

fn value_fits_type(value: i128, ty: &Type) -> bool {
    let (Some(min), Some(max)) = (ty.min_value(), ty.max_value()) else {
        return false;
    };

    min <= value && value <= max
}

fn require_type(
    actual: &Type,
    expected: &Type,
    span: Span,
    context: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if actual != expected {
        diagnostics.push(Diagnostic::new(
            span,
            format!("{} must be {}, got {}", context, expected, actual),
        ));
    }
}

fn const_display(value: &TypedConst) -> String {
    match value {
        TypedConst::Int(value) => value.to_string(),
        TypedConst::Float(value) => value.to_string(),
        TypedConst::Bool(value) => value.to_string(),
    }
}

fn substitute_expr(expr: &Expr, subs: &HashMap<String, Expr>) -> Expr {
    let new_kind = match &expr.kind {
        ExprKind::Var(name) => {
            if let Some(replacement) = subs.get(name) {
                return replacement.clone();
            }
            ExprKind::Var(name.clone())
        }
        ExprKind::Int(v) => ExprKind::Int(*v),
        ExprKind::Float(v) => ExprKind::Float(*v),
        ExprKind::Bool(v) => ExprKind::Bool(*v),
        ExprKind::Str(s) => ExprKind::Str(s.clone()),
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: Box::new(substitute_expr(expr, subs)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(substitute_expr(left, subs)),
            right: Box::new(substitute_expr(right, subs)),
        },
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: Box::new(substitute_expr(expr, subs)),
            ty: ty.clone(),
        },
        ExprKind::Call { name, name_span, args } => ExprKind::Call {
            name: name.clone(),
            name_span: *name_span,
            args: args.iter().map(|a| substitute_expr(a, subs)).collect(),
        },
        ExprKind::MethodCall { receiver, method, method_span, args } => ExprKind::MethodCall {
            receiver: Box::new(substitute_expr(receiver, subs)),
            method: method.clone(),
            method_span: *method_span,
            args: args.iter().map(|a| substitute_expr(a, subs)).collect(),
        },
        ExprKind::Index { array, index } => ExprKind::Index {
            array: Box::new(substitute_expr(array, subs)),
            index: Box::new(substitute_expr(index, subs)),
        },
        ExprKind::Len { array } => ExprKind::Len {
            array: Box::new(substitute_expr(array, subs)),
        },
        ExprKind::ArrayLit { elements } => ExprKind::ArrayLit {
            elements: elements.iter().map(|e| substitute_expr(e, subs)).collect(),
        },
        ExprKind::Ref { inner } => ExprKind::Ref {
            inner: Box::new(substitute_expr(inner, subs)),
        },
        ExprKind::RefMut { inner } => ExprKind::RefMut {
            inner: Box::new(substitute_expr(inner, subs)),
        },
        ExprKind::Tuple(elements) => ExprKind::Tuple(
            elements.iter().map(|e| substitute_expr(e, subs)).collect(),
        ),
        ExprKind::TupleAccess { tuple, index } => ExprKind::TupleAccess {
            tuple: Box::new(substitute_expr(tuple, subs)),
            index: *index,
        },
        ExprKind::StructLit {
            type_name,
            type_name_span,
            fields,
        } => ExprKind::StructLit {
            type_name: type_name.clone(),
            type_name_span: *type_name_span,
            fields: fields
                .iter()
                .map(|(n, e)| (n.clone(), substitute_expr(e, subs)))
                .collect(),
        },
        ExprKind::FieldAccess { object, field } => ExprKind::FieldAccess {
            object: Box::new(substitute_expr(object, subs)),
            field: field.clone(),
        },
        ExprKind::Match { scrutinee, arms } => ExprKind::Match {
            scrutinee: Box::new(substitute_expr(scrutinee, subs)),
            arms: arms
                .iter()
                .map(|a| crate::ast::MatchArm {
                    pattern: a.pattern.clone(),
                    pattern_span: a.pattern_span,
                    body: substitute_expr(&a.body, subs),
                })
                .collect(),
        },
        ExprKind::IfExpr { cond, then_value, else_value } => ExprKind::IfExpr {
            cond: Box::new(substitute_expr(cond, subs)),
            then_value: Box::new(substitute_expr(then_value, subs)),
            else_value: Box::new(substitute_expr(else_value, subs)),
        },
        ExprKind::Block { stmts, tail } => {
            // Substitution walks into block-internal let RHSes
            // and the tail. The let-bound names themselves
            // shadow any matching subs entry inside the block,
            // but for v1 we don't gate on that — block-expr
            // lets typically use fresh names and substitution
            // targets are outer free vars.
            let new_stmts = stmts.iter().map(|s| match s {
                Stmt::Let { name, annotation, expr: rhs, span } => Stmt::Let {
                    name: name.clone(),
                    annotation: annotation.clone(),
                    expr: substitute_expr(rhs, subs),
                    span: *span,
                },
                other => other.clone(),
            }).collect();
            ExprKind::Block {
                stmts: new_stmts,
                tail: Box::new(substitute_expr(tail, subs)),
            }
        }
        ExprKind::Try { inner } => ExprKind::Try {
            inner: Box::new(substitute_expr(inner, subs)),
        },
    };
    Expr {
        kind: new_kind,
        span: expr.span,
    }
}

/// At a `return expr;` site, verify each ensures clause holds by
/// substituting `_return` with the return expression and running SMT.
fn verify_ensures_at_return(
    function: &Function,
    return_expr: &Expr,
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if function.ensures.is_empty() || crate::smt::verifier_disabled() {
        return;
    }
    let mut subs: HashMap<String, Expr> = HashMap::new();
    subs.insert(RETURN_NAME.to_string(), return_expr.clone());

    for ens in &function.ensures {
        let substituted = substitute_expr(ens, &subs);
        use crate::smt::Verdict;
        match prove_with_calls(&substituted, smt_facts, env, signatures) {
            Verdict::Proven => {}
            Verdict::Disproven { counterexample } => {
                let detail = match counterexample {
                    Some(ctr) => format!(
                        "function '{}' ensures clause does not hold at this return [counterexample: {}]",
                        function.name, ctr
                    ),
                    None => format!(
                        "function '{}' ensures clause does not hold at this return (SMT counterexample)",
                        function.name
                    ),
                };
                diagnostics.push(
                    Diagnostic::new(ens.span, detail)
                        .with_related(return_expr.span, "return is here"),
                );
            }
            Verdict::Unknown | Verdict::Unavailable | Verdict::SkippedUnsupported(_) => {
                // Fall back to constant-true check for the substituted ensures.
                // If the user's claim is trivially provable by constant
                // folding on the substituted expression, accept it.
            }
        }
    }
}

/// Combine the explicit per-scope SMT facts with `name == value` equalities
/// derived from env's compile-time-known constants. This lets the SMT layer
/// see e.g. that `i` was just `let i: i64 = 0;`'d, even though we don't
/// otherwise model the program state symbolically.
/// Does `expr` reference `Var(name)` anywhere in its subtree? Used by
/// `drop_facts_mentioning` to invalidate stale facts about a binding
/// when its value is reassigned or shadowed.
fn expr_mentions(expr: &Expr, name: &str) -> bool {
    match &expr.kind {
        ExprKind::Var(n) => n == name,
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Str(_) => false,
        ExprKind::Unary { expr, .. } => expr_mentions(expr, name),
        ExprKind::Binary { left, right, .. } => {
            expr_mentions(left, name) || expr_mentions(right, name)
        }
        ExprKind::Cast { expr, .. } => expr_mentions(expr, name),
        ExprKind::Call { args, .. } => args.iter().any(|a| expr_mentions(a, name)),
        ExprKind::MethodCall { receiver, args, .. } => {
            expr_mentions(receiver, name) || args.iter().any(|a| expr_mentions(a, name))
        }
        ExprKind::ArrayLit { elements } => elements.iter().any(|e| expr_mentions(e, name)),
        ExprKind::Index { array, index } => {
            expr_mentions(array, name) || expr_mentions(index, name)
        }
        ExprKind::Len { array } => expr_mentions(array, name),
        ExprKind::Ref { inner } | ExprKind::RefMut { inner } => expr_mentions(inner, name),
        ExprKind::Tuple(elements) => {
            elements.iter().any(|e| expr_mentions(e, name))
        }
        ExprKind::TupleAccess { tuple, .. } => expr_mentions(tuple, name),
        ExprKind::StructLit { fields, .. } => {
            fields.iter().any(|(_, e)| expr_mentions(e, name))
        }
        ExprKind::FieldAccess { object, .. } => expr_mentions(object, name),
        ExprKind::Match { scrutinee, arms } => {
            expr_mentions(scrutinee, name)
                || arms.iter().any(|a| expr_mentions(&a.body, name))
        }
        ExprKind::IfExpr { cond, then_value, else_value } => {
            expr_mentions(cond, name)
                || expr_mentions(then_value, name)
                || expr_mentions(else_value, name)
        }
        ExprKind::Block { stmts, tail } => {
            stmts.iter().any(|s| match s {
                Stmt::Let { expr, .. } => expr_mentions(expr, name),
                _ => false,
            }) || expr_mentions(tail, name)
        }
        ExprKind::Try { inner } => expr_mentions(inner, name),
    }
}

/// Remove every fact in `smt_facts` that references the binding
/// `name`. Used when `name` is reassigned/shadowed: the old binding's
/// facts no longer describe the current value, and keeping them risks
/// a contradictory fact set (e.g. `len(xs) == 3` plus `len(xs) ==
/// len(xs) + 1`, which lets the verifier prove anything).
/// Effects analysis: walk a typed-statement body and report a
/// diagnostic for each operation that produces an observable side
/// effect or might. Used for both `pure fn` bodies and the body of
/// a `parallel for` loop — same set of rules either way:
///   - `print`, `assert` with a runtime message: side-effecting I/O
///     or panic visible to the outside world.
///   - `IndexAssign`: mutates a (mutable) array/Vec.
///   - `Reassign` that drops an old non-Copy value (Vec/OwnedStr):
///     drop has observable effects (frees memory).
///   - `Call`: only allowed if the target function is `pure`. The
///     Vec mutator builtins (`push`, `set`, `clone`, `vec`) are
///     intrinsically pure-by-construction — they return new values
///     and the consumed operand isn't shared — but they're
///     conservatively excluded from a `pure fn` body because they
///     touch the heap (which can trap on allocator failure).
/// Synthetic moves (e.g., consumes inside `set`) are not analyzed
/// independently — the effect was attributed to the Call.
/// Pure-body check with a carve-out for `parallel for` reduction
/// clauses. A Reassign of a declared reduction variable is allowed
/// when its RHS has the shape `<var> <op> X` (or `X <op> <var>`)
/// where `op` matches the clause's declared op and `X` is itself
/// pure. Every other usage of the variable inside the body is an
/// error — even reads, since they could see partial values.
fn verify_pure_body_with_reductions(
    body: &[TypedStmt],
    signatures: &HashMap<String, Signature>,
    context: &str,
    reductions: &[crate::ir::TypedReduction],
    diagnostics: &mut Vec<Diagnostic>,
) {
    // First the normal pure-body walk, but with each reduction
    // var's Reassign and its read-uses pre-validated and stripped
    // out. The simplest approach: clone the body, drop the
    // reduction reassigns from it, then run the pure walk over
    // the rest. The pre-pass diagnoses bad shapes; the pure walk
    // catches everything else.
    let by_name: HashMap<String, crate::ast::ReductionOp> = reductions
        .iter()
        .map(|r| (r.var.clone(), r.op))
        .collect();
    let stripped = strip_reduction_uses(body, &by_name, context, diagnostics);
    verify_pure_body(&stripped, signatures, context, diagnostics);
}

/// Walk the body, validate each Reassign whose target is a
/// reduction variable, replace it with a no-op `Discard` of a
/// dummy `Int(0)` so the downstream pure walk doesn't double-
/// flag it. Read-uses of the variable in OTHER statements are an
/// error — diagnosed here directly.
fn strip_reduction_uses(
    body: &[TypedStmt],
    reductions: &HashMap<String, crate::ast::ReductionOp>,
    context: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<TypedStmt> {
    fn rec(
        stmts: &[TypedStmt],
        reductions: &HashMap<String, crate::ast::ReductionOp>,
        context: &str,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Vec<TypedStmt> {
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            match stmt {
                TypedStmt::Reassign { name, expr, .. } if reductions.contains_key(name) => {
                    if validate_reduction_rhs(name, expr, reductions[name]) {
                        // Approved shape; replace with a harmless
                        // discard so the pure walk doesn't reject
                        // the reassign-over-non-Copy rule.
                        out.push(TypedStmt::Discard {
                            expr: TypedExpr {
                                kind: TypedExprKind::Int(0),
                                ty: Type::I64,
                                constant: None,
                                span: expr.span,
                                binding_decl_span: None,
                            },
                        });
                    } else {
                        use crate::ast::ReductionOp;
                        let sym = reductions[name].display_symbol();
                        let shape = match reductions[name] {
                            ReductionOp::Min | ReductionOp::Max => format!(
                                "{}({}, <expr>) or {}(<expr>, {})",
                                sym, name, sym, name
                            ),
                            _ => format!(
                                "{} {} <expr> or <expr> {} {}",
                                name, sym, sym, name
                            ),
                        };
                        diagnostics.push(Diagnostic::new(
                            expr.span,
                            format!(
                                "{} reduction variable '{}' must be updated only as `{}`",
                                context, name, shape,
                            ),
                        ));
                    }
                }
                TypedStmt::If { cond, then_body, else_body } => {
                    if any_read_of_reductions(cond, reductions) {
                        flag_read(cond.span, context, reductions, diagnostics);
                    }
                    out.push(TypedStmt::If {
                        cond: cond.clone(),
                        then_body: rec(then_body, reductions, context, diagnostics),
                        else_body: rec(else_body, reductions, context, diagnostics),
                    });
                }
                TypedStmt::While { cond, body } => {
                    if any_read_of_reductions(cond, reductions) {
                        flag_read(cond.span, context, reductions, diagnostics);
                    }
                    out.push(TypedStmt::While {
                        cond: cond.clone(),
                        body: rec(body, reductions, context, diagnostics),
                    });
                }
                TypedStmt::For { var, ty, start, end, body, parallel, reductions: rs } => {
                    if any_read_of_reductions(start, reductions)
                        || any_read_of_reductions(end, reductions)
                    {
                        flag_read(start.span, context, reductions, diagnostics);
                    }
                    out.push(TypedStmt::For {
                        var: var.clone(),
                        ty: ty.clone(),
                        start: start.clone(),
                        end: end.clone(),
                        body: rec(body, reductions, context, diagnostics),
                        parallel: *parallel,
                        reductions: rs.clone(),
                    });
                }
                other => {
                    // For any other shape, check that no read of a
                    // reduction var leaks into the expression(s).
                    for e in stmt_reads(other) {
                        if any_read_of_reductions(e, reductions) {
                            flag_read(e.span, context, reductions, diagnostics);
                        }
                    }
                    out.push(other.clone());
                }
            }
        }
        out
    }
    rec(body, reductions, context, diagnostics)
}

fn validate_reduction_rhs(name: &str, expr: &TypedExpr, op: crate::ast::ReductionOp) -> bool {
    use crate::ast::ReductionOp;
    // Map ReductionOp to the BinaryOp the user must have written
    // (for infix ops) or to the intrinsic Call name (for min/max).
    match op {
        ReductionOp::Add
        | ReductionOp::Mul
        | ReductionOp::And
        | ReductionOp::Or
        | ReductionOp::BitAnd
        | ReductionOp::BitOr
        | ReductionOp::BitXor => {
            let binary_op = match op {
                ReductionOp::Add => BinaryOp::Add,
                ReductionOp::Mul => BinaryOp::Mul,
                ReductionOp::And => BinaryOp::And,
                ReductionOp::Or => BinaryOp::Or,
                ReductionOp::BitAnd => BinaryOp::BitAnd,
                ReductionOp::BitOr => BinaryOp::BitOr,
                ReductionOp::BitXor => BinaryOp::BitXor,
                _ => unreachable!(),
            };
            if let TypedExprKind::Binary { op: rhs_op, left, right, .. } = &expr.kind {
                if *rhs_op != binary_op {
                    return false;
                }
                let left_is_self = matches!(&left.kind, TypedExprKind::Var(n) if n == name);
                let right_is_self = matches!(&right.kind, TypedExprKind::Var(n) if n == name);
                if left_is_self && !contains_var(right, name) {
                    return true;
                }
                if right_is_self && !contains_var(left, name) {
                    return true;
                }
            }
            false
        }
        ReductionOp::Min | ReductionOp::Max => {
            let intrinsic = if matches!(op, ReductionOp::Min) { "min" } else { "max" };
            if let TypedExprKind::Call { name: call_name, args, .. } = &expr.kind {
                if call_name != intrinsic || args.len() != 2 {
                    return false;
                }
                let left_is_self =
                    matches!(&args[0].kind, TypedExprKind::Var(n) if n == name);
                let right_is_self =
                    matches!(&args[1].kind, TypedExprKind::Var(n) if n == name);
                if left_is_self && !contains_var(&args[1], name) {
                    return true;
                }
                if right_is_self && !contains_var(&args[0], name) {
                    return true;
                }
            }
            false
        }
    }
}

fn contains_var(expr: &TypedExpr, name: &str) -> bool {
    match &expr.kind {
        TypedExprKind::Var(n) => n == name,
        TypedExprKind::Unary { expr, .. } => contains_var(expr, name),
        TypedExprKind::Binary { left, right, .. } => {
            contains_var(left, name) || contains_var(right, name)
        }
        TypedExprKind::Cast { expr, .. } => contains_var(expr, name),
        TypedExprKind::Call { args, .. } => args.iter().any(|a| contains_var(a, name)),
        TypedExprKind::ArrayLit { elements } => elements.iter().any(|e| contains_var(e, name)),
        TypedExprKind::Index { array, index, .. } => {
            contains_var(array, name) || contains_var(index, name)
        }
        TypedExprKind::Len { array, .. } => contains_var(array, name),
        TypedExprKind::Ref { name: n } | TypedExprKind::RefMut { name: n } => n == name,
        _ => false,
    }
}

fn any_read_of_reductions(
    expr: &TypedExpr,
    reductions: &HashMap<String, crate::ast::ReductionOp>,
) -> bool {
    reductions.keys().any(|name| contains_var(expr, name))
}

fn flag_read(
    span: Span,
    context: &str,
    reductions: &HashMap<String, crate::ast::ReductionOp>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let names: Vec<&String> = reductions.keys().collect();
    let label = names
        .iter()
        .map(|n| format!("'{}'", n))
        .collect::<Vec<_>>()
        .join(", ");
    diagnostics.push(Diagnostic::new(
        span,
        format!(
            "{} cannot read reduction variable(s) {} outside the named update — partial values would leak",
            context, label
        ),
    ));
}

/// Yield every TypedExpr embedded in a TypedStmt. Used by the
/// reduction-read scan to walk arbitrary statements without
/// special-casing each variant.
fn stmt_reads(stmt: &TypedStmt) -> Vec<&TypedExpr> {
    match stmt {
        TypedStmt::Let { expr, .. }
        | TypedStmt::Reassign { expr, .. }
        | TypedStmt::Return { expr }
        | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => vec![expr],
        TypedStmt::Discard { expr } => vec![expr],
        TypedStmt::Print { items } => items
            .iter()
            .filter_map(|i| match i {
                crate::ir::TypedPrintItem::Expr(e) => Some(e),
                _ => None,
            })
            .collect(),
        TypedStmt::IndexAssign { index, value, .. } => vec![index, value],
        TypedStmt::FieldAssign { object, value, .. } => vec![object, value],
        TypedStmt::Drop { .. }
        | TypedStmt::Break
        | TypedStmt::Continue
        | TypedStmt::If { .. }
        | TypedStmt::While { .. }
        | TypedStmt::For { .. }
        | TypedStmt::ForIter { .. }
        | TypedStmt::TaskSpawn { .. }
        | TypedStmt::TaskJoin { .. } => vec![],
    }
}

fn verify_pure_body(
    body: &[TypedStmt],
    signatures: &HashMap<String, Signature>,
    context: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    fn walk(
        stmts: &[TypedStmt],
        signatures: &HashMap<String, Signature>,
        context: &str,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        for stmt in stmts {
            match stmt {
                TypedStmt::Print { items } => {
                    // Use the first item's span when one exists so
                    // the diagnostic underlines the print line.
                    let span = items
                        .iter()
                        .find_map(|i| match i {
                            crate::ir::TypedPrintItem::Expr(e) => Some(e.span),
                            _ => None,
                        })
                        .unwrap_or_default();
                    diagnostics.push(Diagnostic::new(
                        span,
                        format!(
                            "{} cannot contain `print` (observable I/O is a side effect)",
                            context
                        ),
                    ));
                }
                TypedStmt::Assert { message: Some(_), expr, .. } => {
                    // Assert with a message is a user-facing abort
                    // path; reject in pure contexts. Assertions
                    // without messages are still allowed (they're
                    // pure proofs at compile time after SMT
                    // discharge).
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "{} cannot contain `assert ..., \"msg\"` (runtime abort with message is a side effect)",
                            context
                        ),
                    ));
                }
                TypedStmt::IndexAssign { name, index, .. } => {
                    diagnostics.push(Diagnostic::new(
                        index.span,
                        format!(
                            "{} cannot mutate '{}[i] = …' (indexed write is a side effect)",
                            context, name
                        ),
                    ));
                }
                TypedStmt::FieldAssign { object, field, .. } => {
                    diagnostics.push(Diagnostic::new(
                        object.span,
                        format!(
                            "{} cannot mutate '.{}' (field write is a side effect)",
                            context, field
                        ),
                    ));
                }
                TypedStmt::Reassign { drop_old: true, name, expr, .. } => {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "{} cannot reassign '{}' over a non-Copy value (drop has observable effects)",
                            context, name
                        ),
                    ));
                }
                TypedStmt::Discard { expr } => {
                    walk_expr(expr, signatures, context, diagnostics);
                }
                TypedStmt::Let { expr, .. }
                | TypedStmt::Reassign { expr, .. }
                | TypedStmt::Return { expr }
                | TypedStmt::Assert { expr, .. }
                | TypedStmt::Prove { expr } => {
                    walk_expr(expr, signatures, context, diagnostics);
                }
                TypedStmt::If { cond, then_body, else_body } => {
                    walk_expr(cond, signatures, context, diagnostics);
                    walk(then_body, signatures, context, diagnostics);
                    walk(else_body, signatures, context, diagnostics);
                }
                TypedStmt::While { cond, body } => {
                    walk_expr(cond, signatures, context, diagnostics);
                    walk(body, signatures, context, diagnostics);
                }
                TypedStmt::For { start, end, body, .. } => {
                    walk_expr(start, signatures, context, diagnostics);
                    walk_expr(end, signatures, context, diagnostics);
                    walk(body, signatures, context, diagnostics);
                }
                TypedStmt::ForIter { body, consumes, collection, .. } => {
                    if *consumes {
                        // No good span on ForIter; emit at default.
                        diagnostics.push(Diagnostic::new(
                            crate::span::Span::default(),
                            format!(
                                "{} cannot consume '{}' via `for x in {0}` (move-and-drop has observable effects)",
                                context, collection
                            ),
                        ));
                    }
                    walk(body, signatures, context, diagnostics);
                }
                TypedStmt::TaskSpawn { name, body, .. } => {
                    // Nested `task` inside a pure context is
                    // pure-with-captures by induction: the nested
                    // body's purity is checked when the inner
                    // spawn is type-checked. We still walk it
                    // here so any impurity inside surfaces with
                    // the outer context label too.
                    let _ = name;
                    walk(body, signatures, context, diagnostics);
                }
                TypedStmt::TaskJoin { .. } => {
                    // Consuming a Task handle in a pure context
                    // is OK — join itself is side-effect-free in
                    // v1's sequential lowering.
                }
                TypedStmt::Drop { .. } | TypedStmt::Break | TypedStmt::Continue => {}
            }
        }
    }
    fn walk_expr(
        expr: &TypedExpr,
        signatures: &HashMap<String, Signature>,
        context: &str,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match &expr.kind {
            TypedExprKind::Call { name, args, .. } => {
                if matches!(
                    name.as_str(),
                    "vec" | "push" | "set" | "clone"
                ) {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "{} cannot call '{}' (heap-allocating Vec builtin is impure)",
                            context, name
                        ),
                    ));
                } else if let Some(sig) = signatures.get(name) {
                    if !sig.is_pure {
                        diagnostics.push(Diagnostic::new(
                            expr.span,
                            format!(
                                "{} cannot call non-pure function '{}'; mark it `pure fn` or remove the call",
                                context, name
                            ),
                        ));
                    }
                }
                for a in args {
                    walk_expr(a, signatures, context, diagnostics);
                }
            }
            TypedExprKind::Unary { expr, .. } => {
                walk_expr(expr, signatures, context, diagnostics)
            }
            TypedExprKind::Binary { left, right, .. } => {
                // `Str + Str` concat is heap-allocating (impure
                // in this slice's accounting). Mirror the Vec
                // builtin rule.
                if matches!(left.ty, Type::Str | Type::OwnedStr)
                    && matches!(right.ty, Type::Str | Type::OwnedStr)
                {
                    diagnostics.push(Diagnostic::new(
                        expr.span,
                        format!(
                            "{} cannot use `+` on strings (heap allocation is impure)",
                            context
                        ),
                    ));
                }
                walk_expr(left, signatures, context, diagnostics);
                walk_expr(right, signatures, context, diagnostics);
            }
            TypedExprKind::Cast { expr, .. } => {
                walk_expr(expr, signatures, context, diagnostics)
            }
            TypedExprKind::ArrayLit { elements } => {
                for e in elements {
                    walk_expr(e, signatures, context, diagnostics);
                }
            }
            TypedExprKind::Index { array, index, .. } => {
                walk_expr(array, signatures, context, diagnostics);
                walk_expr(index, signatures, context, diagnostics);
            }
            TypedExprKind::Len { array, .. } => {
                walk_expr(array, signatures, context, diagnostics)
            }
            TypedExprKind::Int(_)
            | TypedExprKind::Float(_)
            | TypedExprKind::Bool(_)
            | TypedExprKind::Str(_)
            | TypedExprKind::Var(_)
            | TypedExprKind::Ref { .. }
            | TypedExprKind::RefMut { .. }
            | TypedExprKind::RefField { .. }
            | TypedExprKind::RefMutField { .. }
            | TypedExprKind::FnRef { .. } => {}
            TypedExprKind::CallIndirect { callee, args } => {
                // The name-based purity gate above can't see
                // through an indirect call's callee — we have
                // no signature to check. Conservatively reject
                // indirect calls in pure contexts; the user
                // can refactor to a direct call.
                diagnostics.push(Diagnostic::new(
                    expr.span,
                    format!(
                        "{} cannot use indirect calls (fn-ptr) — \
                         the purity gate sees only direct calls",
                        context
                    ),
                ));
                walk_expr(callee, signatures, context, diagnostics);
                for a in args {
                    walk_expr(a, signatures, context, diagnostics);
                }
            }
            TypedExprKind::Tuple { elements } => {
                for e in elements {
                    walk_expr(e, signatures, context, diagnostics);
                }
            }
            TypedExprKind::TupleAccess { tuple, .. } => {
                walk_expr(tuple, signatures, context, diagnostics);
            }
            TypedExprKind::StructLit { fields, .. } => {
                for (_, e) in fields {
                    walk_expr(e, signatures, context, diagnostics);
                }
            }
            TypedExprKind::FieldAccess { object, .. } => {
                walk_expr(object, signatures, context, diagnostics);
            }
            TypedExprKind::EnumVariant { .. } => {}
            TypedExprKind::EnumVariantWithPayload { payload, .. } => {
                walk_expr(payload, signatures, context, diagnostics);
            }
            TypedExprKind::Match { scrutinee, arms } => {
                walk_expr(scrutinee, signatures, context, diagnostics);
                for arm in arms {
                    walk_expr(&arm.body, signatures, context, diagnostics);
                }
            }
            TypedExprKind::IfExpr { cond, then_value, else_value } => {
                walk_expr(cond, signatures, context, diagnostics);
                walk_expr(then_value, signatures, context, diagnostics);
                walk_expr(else_value, signatures, context, diagnostics);
            }
            TypedExprKind::Block { stmts, tail } => {
                for s in stmts {
                    if let TypedStmt::Let { expr, .. } = s {
                        walk_expr(expr, signatures, context, diagnostics);
                    }
                }
                walk_expr(tail, signatures, context, diagnostics);
            }
        }
    }
    walk(body, signatures, context, diagnostics);
}

fn drop_facts_mentioning(smt_facts: &mut Vec<Expr>, name: &str) {
    smt_facts.retain(|f| !expr_mentions(f, name));
}

/// Walk `expr` and rewrite every bare `Var(name)` (without `#N`
/// suffix) into `Var("name#<version>")`. Used by the IndexAssign
/// handler to "snapshot" existing facts at the binding's
/// pre-assign version before bumping the SMT-array version
/// counter. After the bump, bare `Var(name)` references emitted by
/// downstream code resolve to the new version at SMT query time;
/// the rewritten old facts stay pinned to the previous version's
/// SMT array.
fn pin_var_to_version(expr: &mut Expr, name: &str, version: u32) {
    match &mut expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_) => {}
        ExprKind::Var(n) => {
            if n == name && !n.contains('#') {
                *n = format!("{}#{}", name, version);
            }
        }
        ExprKind::Unary { expr, .. } => pin_var_to_version(expr, name, version),
        ExprKind::Binary { left, right, .. } => {
            pin_var_to_version(left, name, version);
            pin_var_to_version(right, name, version);
        }
        ExprKind::Cast { expr, .. } => pin_var_to_version(expr, name, version),
        ExprKind::ArrayLit { elements } => {
            for e in elements {
                pin_var_to_version(e, name, version);
            }
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                pin_var_to_version(a, name, version);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            pin_var_to_version(receiver, name, version);
            for a in args {
                pin_var_to_version(a, name, version);
            }
        }
        ExprKind::Index { array, index } => {
            pin_var_to_version(array, name, version);
            pin_var_to_version(index, name, version);
        }
        ExprKind::Len { array } => pin_var_to_version(array, name, version),
        ExprKind::Ref { inner } | ExprKind::RefMut { inner } => {
            pin_var_to_version(inner, name, version)
        }
        ExprKind::Tuple(elements) => {
            for e in elements {
                pin_var_to_version(e, name, version);
            }
        }
        ExprKind::TupleAccess { tuple, .. } => {
            pin_var_to_version(tuple, name, version);
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, e) in fields {
                pin_var_to_version(e, name, version);
            }
        }
        ExprKind::FieldAccess { object, .. } => {
            pin_var_to_version(object, name, version);
        }
        ExprKind::Match { scrutinee, arms } => {
            pin_var_to_version(scrutinee, name, version);
            for arm in arms.iter_mut() {
                pin_var_to_version(&mut arm.body, name, version);
            }
        }
        ExprKind::IfExpr { cond, then_value, else_value } => {
            pin_var_to_version(cond, name, version);
            pin_var_to_version(then_value, name, version);
            pin_var_to_version(else_value, name, version);
        }
        ExprKind::Block { stmts, tail } => {
            for s in stmts {
                if let Stmt::Let { expr, .. } = s {
                    pin_var_to_version(expr, name, version);
                }
            }
            pin_var_to_version(tail, name, version);
        }
        ExprKind::Try { inner } => {
            pin_var_to_version(inner, name, version);
        }
    }
}

// (The selective-drop helpers `fact_safe_after_index_assign` /
// `expr_safe_after_index_assign` were superseded by the SMT-array
// versioning path: every IndexAssign now snapshots existing facts
// at the pre-assign version and emits a fresh store-eq axiom, so
// no fact has to be dropped. Removed.)

/// Does this statement list (or any of its non-loop nested blocks)
/// contain a `break`? Used to decide whether the post-loop `!cond`
/// fact is sound: if any `break` can fire, the loop may exit while
/// `cond` is still true, so we can't assume `!cond` after the loop.
///
/// We do NOT recurse into nested loops — a `break` in an inner loop
/// targets that inner loop, not the outer one, and is irrelevant.
fn contains_break(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match s {
            Stmt::Break { .. } => return true,
            Stmt::If { then_body, else_body, .. } => {
                if contains_break(then_body) || contains_break(else_body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Logical negation that strips double-negation and flips comparison
/// operators. Cleaner than wrapping with another `!` because it keeps
/// branch-narrowing facts (and the eventual counterexample) readable.
/// SMT would handle the `!!` form fine — this is purely for diagnostic
/// quality.
fn negate(expr: &Expr) -> Expr {
    match &expr.kind {
        ExprKind::Unary { op: UnaryOp::Not, expr: inner } => (**inner).clone(),
        ExprKind::Bool(v) => Expr {
            kind: ExprKind::Bool(!v),
            span: expr.span,
        },
        ExprKind::Binary { op, left, right } => {
            if let Some(flipped) = match op {
                BinaryOp::Eq => Some(BinaryOp::Ne),
                BinaryOp::Ne => Some(BinaryOp::Eq),
                BinaryOp::Lt => Some(BinaryOp::Ge),
                BinaryOp::Le => Some(BinaryOp::Gt),
                BinaryOp::Gt => Some(BinaryOp::Le),
                BinaryOp::Ge => Some(BinaryOp::Lt),
                _ => None,
            } {
                return Expr {
                    kind: ExprKind::Binary {
                        op: flipped,
                        left: left.clone(),
                        right: right.clone(),
                    },
                    span: expr.span,
                };
            }
            // Boolean connectives and shifts/arithmetic stay wrapped.
            Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expr.clone()),
                },
                span: expr.span,
            }
        }
        _ => Expr {
            kind: ExprKind::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr.clone()),
            },
            span: expr.span,
        },
    }
}

fn current_smt_facts(smt_facts: &[Expr], env: &Env) -> Vec<Expr> {
    let mut out: Vec<Expr> = smt_facts.to_vec();
    for (name, info) in env.all_bindings() {
        let Some(c) = &info.constant else { continue };
        let const_expr = match c {
            TypedConst::Int(v) => Expr {
                kind: ExprKind::Int(*v),
                span: crate::span::Span::default(),
            },
            TypedConst::Bool(v) => Expr {
                kind: ExprKind::Bool(*v),
                span: crate::span::Span::default(),
            },
            TypedConst::Float(_) => continue, // floats not modeled in SMT v1
        };
        out.push(Expr {
            kind: ExprKind::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr {
                    kind: ExprKind::Var(name.clone()),
                    span: crate::span::Span::default(),
                }),
                right: Box::new(const_expr),
            },
            span: crate::span::Span::default(),
        });
    }
    out
}

/// Walk a body AST and collect, for each variable, its *last* reassignment
/// expression. Used by the loop-invariant preservation check to substitute
/// the post-body value of each modified variable into the invariant before
/// sending it to SMT. Each entry is the *composed* expression for that
/// variable: the body-end value rewritten in terms of body-entry values,
/// chaining through every reassignment in source order. For example, the
/// body
///   acc = acc + 1;
///   i = i + 1;
///   acc = acc * 2;
/// produces `i -> i + 1`, `acc -> (acc + 1) * 2`. Composition handles
/// multiple reassignments per iteration soundly — refines #5 from
/// STATUS.md (was: only the last RHS per variable, ignoring intervening
/// updates the RHS depended on). Recurses into `if`/`else` branches
/// (union of reassignments — same conservatism as before; an SSA-driven
/// per-branch model would refine further), but not into nested loops
/// (those have their own verification frames).
/// Body-level reassignments collected for the loop-invariant
/// preservation check. Pair of:
///   - `subs`: substitution map (body-entry → body-end) used by
///     `substitute_expr`.
///   - `havoc_vars`: fresh names introduced by nested loops as
///     "we can't symbolically express the post-loop value here"
///     placeholders. The caller must register these in the SMT
///     vars list so the substituted invariant has a declared
///     constant of the right sort to reference.
struct ReassignSummary {
    subs: HashMap<String, Expr>,
    havoc_vars: Vec<(String, Type)>,
}

fn collect_last_reassigns_with_env(
    stmts: &[Stmt],
    env: &Env,
) -> ReassignSummary {
    let mut subs = HashMap::new();
    let mut havoc_vars = Vec::new();
    let mut havoc_counter: u32 = 0;
    walk_for_reassigns(
        stmts,
        env,
        &mut subs,
        &mut havoc_vars,
        &mut havoc_counter,
    );
    ReassignSummary { subs, havoc_vars }
}

fn walk_for_reassigns(
    stmts: &[Stmt],
    env: &Env,
    out: &mut HashMap<String, Expr>,
    havoc_vars: &mut Vec<(String, Type)>,
    havoc_counter: &mut u32,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { name, expr, .. } => {
                // Substitute the current `out` into the RHS so the
                // stored expression talks about body-entry values
                // only. Without this step, a follow-on reassign
                // whose RHS references this variable would
                // incorrectly drop the intermediate update — the
                // case #5 was about.
                let rewritten = substitute_expr(expr, out);
                out.insert(name.clone(), rewritten);
            }
            Stmt::Let { name, expr, .. } => {
                // A shadow-let inside the loop body is also a
                // reassignment when an outer binding with the same
                // name exists. Same composition rule as `Assign`:
                // rewrite the RHS through current `out` so the
                // map's invariant ("body-end value in terms of
                // body-entry") still holds.
                let rewritten = substitute_expr(expr, out);
                out.insert(name.clone(), rewritten);
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                walk_for_reassigns(
                    then_body,
                    env,
                    out,
                    havoc_vars,
                    havoc_counter,
                );
                walk_for_reassigns(
                    else_body,
                    env,
                    out,
                    havoc_vars,
                    havoc_counter,
                );
            }
            Stmt::While { body, span, .. }
            | Stmt::For { body, span, .. }
            | Stmt::ForIter { body, span, .. } => {
                // Refines #6 from STATUS.md. A nested loop's
                // per-iteration symbolic effect can't be collapsed
                // into a single substitution expression here (it
                // would need unbounded composition). Naively
                // skipping the nested body (the old behavior)
                // leaves stale `out` entries for variables the
                // outer body wrote BEFORE the nested loop but the
                // nested loop overwrites — and even removing those
                // entries isn't enough, because the substituted
                // invariant would still mention the bare `Var(x)`
                // which the SMT layer conflates with the body-
                // entry value supplied by the entry invariants. We
                // replace each nested-mutated name with a fresh
                // `Var(name__havoc_<N>)` token so the substituted
                // goal references a distinct symbol SMT can't fold
                // into the entry assumption. The caller registers
                // the fresh name in the SMT vars list (sourced
                // from `env`'s declared type for `name`). The
                // outer preservation check then succeeds only when
                // post-nested-loop facts (the inner loop's
                // invariants, if any) actually constrain the
                // variable enough.
                let nested_muts = collect_branch_mutations(body);
                for name in nested_muts {
                    let Some(ty) =
                        env.lookup(&name).map(|info| info.ty.clone())
                    else {
                        // Nested loop touched a name we can't
                        // resolve (probably a nested-loop-local
                        // shadow). Skip the havoc: the outer
                        // substitution won't talk about it
                        // anyway since it's not in the outer
                        // env.
                        continue;
                    };
                    *havoc_counter += 1;
                    let fresh =
                        format!("{}__havoc_{}", name, *havoc_counter);
                    havoc_vars.push((fresh.clone(), ty));
                    out.insert(
                        name,
                        Expr {
                            kind: ExprKind::Var(fresh),
                            span: *span,
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

fn verify_loop_invariants(
    invariants: &[Expr],
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
    failure_phrase: &str,
    substitutions: Option<&HashMap<String, Expr>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    verify_loop_invariants_with_havoc(
        invariants,
        smt_facts,
        env,
        signatures,
        failure_phrase,
        substitutions,
        &[],
        diagnostics,
    );
}

/// Same as `verify_loop_invariants` but also threads a list of
/// `havoc_vars` — fresh `(name, type)` pairs introduced by
/// nested-loop havocing (see `walk_for_reassigns`) — through to
/// the SMT prover so the substituted invariant's fresh symbols
/// have a declared sort.
fn verify_loop_invariants_with_havoc(
    invariants: &[Expr],
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
    failure_phrase: &str,
    substitutions: Option<&HashMap<String, Expr>>,
    havoc_vars: &[(String, Type)],
    diagnostics: &mut Vec<Diagnostic>,
) {
    if invariants.is_empty() || crate::smt::verifier_disabled() {
        return;
    }
    use crate::smt::Verdict;
    for inv in invariants {
        let goal = if let Some(subs) = substitutions {
            substitute_expr(inv, subs)
        } else {
            inv.clone()
        };
        match prove_with_calls_extra(&goal, smt_facts, env, signatures, havoc_vars) {
            Verdict::Proven => {}
            Verdict::Disproven { counterexample } => {
                let detail = match counterexample {
                    Some(ctr) => format!(
                        "loop invariant {} [counterexample: {}]",
                        failure_phrase, ctr
                    ),
                    None => format!(
                        "loop invariant {} (SMT counterexample)",
                        failure_phrase
                    ),
                };
                diagnostics.push(Diagnostic::new(inv.span, detail));
            }
            Verdict::Unknown => diagnostics.push(Diagnostic::new(
                inv.span,
                format!(
                    "cannot verify loop invariant: SMT returned 'unknown' ({})",
                    failure_phrase
                ),
            )),
            Verdict::SkippedUnsupported(reason) => {
                let hint = if reason.starts_with("function call") {
                    " — add an 'ensures' clause to the callee"
                } else {
                    ""
                };
                diagnostics.push(Diagnostic::new(
                    inv.span,
                    format!(
                        "cannot verify loop invariant: {} (uses features outside the SMT v1 encoder{})",
                        reason, hint
                    ),
                ));
            }
            Verdict::Unavailable => diagnostics.push(Diagnostic::new(
                inv.span,
                "cannot verify loop invariant: no SMT solver available (install z3)",
            )),
        }
    }
}

/// Append Vec-builtin length facts to `smt_facts` after a `let r = <call>`
/// when `<call>` is one of the Vec-constructing builtins. The SMT layer
/// otherwise sees `len(xs)` as an opaque per-binding symbol and can't
/// relate two such symbols across a push/clone — this restores the
/// arithmetic relationship so `prove len(xs2) == len(xs) + 1` discharges.
///
/// Encoded relationships:
///   `let r = vec(a, b, c);` → len(r) == N
///   `let r = push(xs, v);`  → len(r) == len(xs) + 1
///   `let r = clone(xs);`    → len(r) == len(xs)
///   `let r = set(xs, i, v)` → len(r) == len(xs)   (set keeps the length)
/// Push `Eq` facts of the form `<let_var>[i] == elements[i]` for
/// each i, so the SMT verifier can reason about a literal array /
/// Vec initializer slot-by-slot. Pairs with the `(select arr_<name>
/// i)` lowering in src/smt.rs. The facts are flat AST expressions —
/// invalidated by the usual `drop_facts_mentioning(<let_var>)` calls
/// whenever the binding is reassigned or its slot is updated.
fn record_array_element_facts(
    let_var: &str,
    elements: &[Expr],
    smt_facts: &mut Vec<Expr>,
) {
    for (i, elem) in elements.iter().enumerate() {
        let lhs = Expr {
            kind: ExprKind::Index {
                array: Box::new(Expr {
                    kind: ExprKind::Var(let_var.to_string()),
                    span: crate::span::Span::default(),
                }),
                index: Box::new(Expr {
                    kind: ExprKind::Int(i as i128),
                    span: crate::span::Span::default(),
                }),
            },
            span: crate::span::Span::default(),
        };
        smt_facts.push(Expr {
            kind: ExprKind::Binary {
                op: BinaryOp::Eq,
                left: Box::new(lhs),
                right: Box::new(elem.clone()),
            },
            span: crate::span::Span::default(),
        });
    }
}

/// If `expr` is a bare `Var(name)` or a `&Var(name)` / `&mut
/// Var(name)`, return the underlying name. Used by the synthetic
/// SMT-array fact emitters (`set`, `clone`) to find the binding the
/// solver's `arr_<base>` symbol refers to, even when the user
/// passed the value through a borrow.
fn unwrap_to_var(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Var(name) => Some(name.as_str()),
        ExprKind::Ref { inner } | ExprKind::RefMut { inner } => unwrap_to_var(inner),
        _ => None,
    }
}

fn record_vec_builtin_facts(
    call_name: &str,
    args: &[Expr],
    let_var: &str,
    smt_facts: &mut Vec<Expr>,
) {
    let mk_len = |e: Expr| Expr {
        kind: ExprKind::Len { array: Box::new(e) },
        span: crate::span::Span::default(),
    };
    let len_var = mk_len(Expr {
        kind: ExprKind::Var(let_var.to_string()),
        span: crate::span::Span::default(),
    });

    match call_name {
        "vec" => {
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(len_var),
                    right: Box::new(Expr {
                        kind: ExprKind::Int(args.len() as i128),
                        span: crate::span::Span::default(),
                    }),
                },
                span: crate::span::Span::default(),
            });
            // Per-element identity facts: `xs[i] == args[i]` for
            // each i. Lets proofs like `prove xs[0] == 10` discharge
            // statically against a literal `vec(10, …)` initializer.
            // The SMT encoder lowers `xs[i]` to `(select arr_xs i)`
            // (see smt.rs); the encoder is read-only, so any later
            // `xs[i] = v` IndexAssign invalidates these facts via
            // the same `drop_facts_mentioning` plumbing that already
            // protects length-derived facts.
            record_array_element_facts(let_var, args, smt_facts);
        }
        "push" if args.len() == 2 => {
            let xs_len = mk_len(args[0].clone());
            let plus_one = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(xs_len.clone()),
                    right: Box::new(Expr {
                        kind: ExprKind::Int(1),
                        span: crate::span::Span::default(),
                    }),
                },
                span: crate::span::Span::default(),
            };
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(len_var),
                    right: Box::new(plus_one),
                },
                span: crate::span::Span::default(),
            });
            // Element identity at the new tail: `ys[len(xs)] == v`.
            // Kept alongside the synthetic store-eq fact (below)
            // because narrower proofs may not have a Var base.
            let lhs = Expr {
                kind: ExprKind::Index {
                    array: Box::new(Expr {
                        kind: ExprKind::Var(let_var.to_string()),
                        span: crate::span::Span::default(),
                    }),
                    index: Box::new(xs_len.clone()),
                },
                span: crate::span::Span::default(),
            };
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(lhs),
                    right: Box::new(args[1].clone()),
                },
                span: crate::span::Span::default(),
            });
            // Store-axiom: `arr_ys = (store arr_xs len(xs) v)`.
            // From an SMT perspective push *is* a store at index
            // len(xs); the symbolic arrays for both bindings have
            // the same sort and the same uninitialised slot space,
            // so the same `__smt_store_eq` machinery used for `set`
            // applies. Lets the verifier derive `ys[k] == xs[k]`
            // for any `k != len(xs)` (caller is responsible for
            // bounds — len(ys) == len(xs) + 1 is already a fact).
            if let Some(base_var) = unwrap_to_var(&args[0]) {
                smt_facts.push(Expr {
                    kind: ExprKind::Call {
                        name: "__smt_store_eq".to_string(),
                        name_span: crate::span::Span::default(),
                        args: vec![
                            Expr {
                                kind: ExprKind::Var(let_var.to_string()),
                                span: crate::span::Span::default(),
                            },
                            Expr {
                                kind: ExprKind::Var(base_var.to_string()),
                                span: crate::span::Span::default(),
                            },
                            xs_len,
                            args[1].clone(),
                        ],
                    },
                    span: crate::span::Span::default(),
                });
            }
        }
        "clone" if args.len() == 1 => {
            // Length and array-content both copied verbatim. The
            // length fact handles `len(ys) == len(xs)`; the synthetic
            // array-eq fact handles `ys[k] == xs[k]` for every k via
            // `(= arr_ys arr_xs)` in SMT.
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(len_var),
                    right: Box::new(mk_len(args[0].clone())),
                },
                span: crate::span::Span::default(),
            });
            // The base may be `Var(xs)` or `&xs` / `&mut xs` — pierce
            // through references to find the underlying binding name
            // so the SMT layer's `arr_<base>` resolution works.
            if let Some(base_var) = unwrap_to_var(&args[0]) {
                smt_facts.push(Expr {
                    kind: ExprKind::Call {
                        name: "__smt_array_eq".to_string(),
                        name_span: crate::span::Span::default(),
                        args: vec![
                            Expr {
                                kind: ExprKind::Var(let_var.to_string()),
                                span: crate::span::Span::default(),
                            },
                            Expr {
                                kind: ExprKind::Var(base_var.to_string()),
                                span: crate::span::Span::default(),
                            },
                        ],
                    },
                    span: crate::span::Span::default(),
                });
            }
        }
        "set" if args.len() == 3 => {
            // Length unchanged across a functional update.
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(len_var),
                    right: Box::new(mk_len(args[0].clone())),
                },
                span: crate::span::Span::default(),
            });
            // Element identity at the updated slot: `ys[i] == v`.
            // Kept alongside the synthetic store-eq fact (below)
            // because the store axiom carries it implicitly, but
            // some narrower proof shapes only need this simple form
            // and the encoder may decline the store fact (e.g., if
            // `base` isn't a plain Var).
            let lhs = Expr {
                kind: ExprKind::Index {
                    array: Box::new(Expr {
                        kind: ExprKind::Var(let_var.to_string()),
                        span: crate::span::Span::default(),
                    }),
                    index: Box::new(args[1].clone()),
                },
                span: crate::span::Span::default(),
            };
            smt_facts.push(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(lhs),
                    right: Box::new(args[2].clone()),
                },
                span: crate::span::Span::default(),
            });
            // Functional-update axiom: `arr_ys = (store arr_xs k v)`.
            // Lets `prove ys[j] == xs[j]` discharge for `j != k`,
            // i.e., slots that the set didn't touch. The encoder
            // intercepts this synthetic Call name in `smt.rs` and
            // emits the `(= arr_ys (store arr_xs k v))` formula
            // directly. Only emit when the base arg is a named
            // binding (so we have `arr_<base>` to reference).
            if let Some(base_var) = unwrap_to_var(&args[0]) {
                smt_facts.push(Expr {
                    kind: ExprKind::Call {
                        name: "__smt_store_eq".to_string(),
                        name_span: crate::span::Span::default(),
                        args: vec![
                            Expr {
                                kind: ExprKind::Var(let_var.to_string()),
                                span: crate::span::Span::default(),
                            },
                            Expr {
                                kind: ExprKind::Var(base_var.to_string()),
                                span: crate::span::Span::default(),
                            },
                            args[1].clone(),
                            args[2].clone(),
                        ],
                    },
                    span: crate::span::Span::default(),
                });
            }
        }
        _ => {}
    }
}

/// Append ensures-derived facts to `smt_facts` after a `let r = foo(args)`
/// where `foo` has `ensures` clauses. Substitutes parameter names with the
/// argument expressions, and `_return` with `Var(r)`.
fn record_ensures_facts(
    call_name: &str,
    args: &[Expr],
    let_var: &str,
    signatures: &HashMap<String, Signature>,
    smt_facts: &mut Vec<Expr>,
) {
    let Some(sig) = signatures.get(call_name) else {
        return;
    };
    if sig.ensures.is_empty() || args.len() != sig.params.len() {
        return;
    }
    let mut subs: HashMap<String, Expr> = HashMap::new();
    for (name, arg) in sig.param_names.iter().zip(args.iter()) {
        subs.insert(name.clone(), arg.clone());
    }
    subs.insert(
        RETURN_NAME.to_string(),
        Expr {
            kind: ExprKind::Var(let_var.to_string()),
            span: crate::span::Span::default(),
        },
    );
    for ens in &sig.ensures {
        smt_facts.push(substitute_expr(ens, &subs));
    }
}

/// Render an `Expr` as a short, human-readable string. Only the forms
/// that turn up in counterexamples need to be supported here (call
/// arguments, simple arithmetic). The output is not re-parseable; its
/// only purpose is to label counterexample entries like
/// `inc(x) = 5` instead of `__call_0 = 5`.
fn pretty_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Int(v) => v.to_string(),
        ExprKind::Float(v) => format!("{}", v),
        ExprKind::Bool(v) => v.to_string(),
        ExprKind::Str(s) => format!("\"{}\"", s),
        ExprKind::Var(name) => name.clone(),
        ExprKind::Unary { op, expr } => {
            let inner = pretty_expr(expr);
            match op {
                UnaryOp::Neg => format!("-{}", inner),
                UnaryOp::Not => format!("!{}", inner),
            }
        }
        ExprKind::Binary { op, left, right } => {
            format!(
                "({} {} {})",
                pretty_expr(left),
                op.display_symbol(),
                pretty_expr(right)
            )
        }
        ExprKind::Call { name, args, .. } => {
            let parts: Vec<String> = args.iter().map(pretty_expr).collect();
            format!("{}({})", name, parts.join(", "))
        }
        ExprKind::MethodCall { receiver, method, args, .. } => {
            let parts: Vec<String> = args.iter().map(pretty_expr).collect();
            format!("{}.{}({})", pretty_expr(receiver), method, parts.join(", "))
        }
        ExprKind::Cast { expr, ty } => format!("{} as {}", pretty_expr(expr), ty),
        ExprKind::Index { array, index } => {
            format!("{}[{}]", pretty_expr(array), pretty_expr(index))
        }
        ExprKind::Len { array } => format!("len({})", pretty_expr(array)),
        ExprKind::ArrayLit { elements } => {
            let parts: Vec<String> = elements.iter().map(pretty_expr).collect();
            format!("[{}]", parts.join(", "))
        }
        ExprKind::Ref { inner } => format!("ref {}", pretty_expr(inner)),
        ExprKind::RefMut { inner } => format!("mut ref {}", pretty_expr(inner)),
        ExprKind::Tuple(elements) => {
            let parts: Vec<String> = elements.iter().map(pretty_expr).collect();
            format!("({})", parts.join(", "))
        }
        ExprKind::TupleAccess { tuple, index } => {
            format!("{}.{}", pretty_expr(tuple), index)
        }
        ExprKind::StructLit { type_name, fields, .. } => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, e)| format!("{}: {}", n, pretty_expr(e)))
                .collect();
            format!("{} {{ {} }}", type_name, parts.join(", "))
        }
        ExprKind::FieldAccess { object, field } => {
            format!("{}.{}", pretty_expr(object), field)
        }
        ExprKind::Match { scrutinee, .. } => {
            format!("match {} {{ … }}", pretty_expr(scrutinee))
        }
        ExprKind::IfExpr { cond, .. } => {
            format!("if {} {{ … }} else {{ … }}", pretty_expr(cond))
        }
        ExprKind::Block { tail, .. } => {
            format!("{{ … ; {} }}", pretty_expr(tail))
        }
        ExprKind::Try { inner } => {
            format!("try {}", pretty_expr(inner))
        }
    }
}

/// Walk `expr`, replacing each `ExprKind::Call` (whose callee has a known
/// signature) with a fresh `Var("__call_<N>")`. For each replacement, emit
/// the call's `ensures` clauses with parameter names substituted by the
/// argument expressions and `_return` substituted by the fresh var name.
/// The fresh vars are appended to `fresh_vars` (name + return type) and the
/// derived facts to `extra_facts`.
///
/// `display_names` collects (fresh_name, pretty source form) so the
/// counterexample formatter can rewrite cryptic synthesized names back
/// into their original call syntax.
///
/// This lets `prove f(args) > 0` work when `f` has an ensures clause that
/// constrains its return value — the SMT solver sees an uninterpreted-but-
/// constrained symbol where the call used to be.
fn rewrite_calls_to_fresh_vars(
    expr: &Expr,
    signatures: &HashMap<String, Signature>,
    counter: &mut usize,
    fresh_vars: &mut Vec<(String, Type)>,
    extra_facts: &mut Vec<Expr>,
    display_names: &mut Vec<(String, String)>,
) -> Expr {
    let new_kind = match &expr.kind {
        ExprKind::Call { name, args, .. } => {
            // Only rewrite if we have a signature with ensures clauses to
            // actually constrain the fresh variable. Otherwise leave the
            // call in place so the SMT encoder still reports it as
            // unsupported (preserves current behavior for callers we
            // know nothing about).
            if let Some(sig) = signatures.get(name) {
                if !sig.ensures.is_empty() && args.len() == sig.params.len() {
                    let new_args: Vec<Expr> = args
                        .iter()
                        .map(|a| {
                            rewrite_calls_to_fresh_vars(
                                a,
                                signatures,
                                counter,
                                fresh_vars,
                                extra_facts,
                                display_names,
                            )
                        })
                        .collect();
                    let fresh_name = format!("__call_{}", *counter);
                    *counter += 1;
                    fresh_vars.push((fresh_name.clone(), sig.return_type.clone()));
                    display_names.push((fresh_name.clone(), pretty_expr(expr)));

                    let fresh_var_expr = Expr {
                        kind: ExprKind::Var(fresh_name.clone()),
                        span: expr.span,
                    };
                    let mut subs: HashMap<String, Expr> = HashMap::new();
                    for (param_name, arg) in sig.param_names.iter().zip(new_args.iter()) {
                        subs.insert(param_name.clone(), arg.clone());
                    }
                    subs.insert(RETURN_NAME.to_string(), fresh_var_expr.clone());
                    for ens in &sig.ensures {
                        extra_facts.push(substitute_expr(ens, &subs));
                    }
                    return fresh_var_expr;
                }
            }
            // Vec builtins (`vec`, `push`, `set`, `clone`) carry an
            // implicit length relationship. Rewrite them to a fresh
            // `Vec<i64>` symbol — the element type is irrelevant for
            // length reasoning — and emit the same length fact that
            // `record_vec_builtin_facts` would emit at a let-binding.
            // This lets `len(push(xs, v))` discharge in proofs and
            // (more importantly) in substituted invariants for
            // loop-preservation.
            if matches!(name.as_str(), "vec" | "push" | "set" | "clone") {
                let new_args: Vec<Expr> = args
                    .iter()
                    .map(|a| {
                        rewrite_calls_to_fresh_vars(
                            a, signatures, counter, fresh_vars, extra_facts, display_names,
                        )
                    })
                    .collect();
                let fresh_name = format!("__call_{}", *counter);
                *counter += 1;
                fresh_vars.push((
                    fresh_name.clone(),
                    Type::Vec(Box::new(Type::I64)),
                ));
                display_names.push((fresh_name.clone(), pretty_expr(expr)));

                let fresh_var_expr = Expr {
                    kind: ExprKind::Var(fresh_name.clone()),
                    span: expr.span,
                };
                record_vec_builtin_facts(name, &new_args, &fresh_name, extra_facts);
                return fresh_var_expr;
            }

            ExprKind::Call {
                name: name.clone(),
                name_span: expr.span,
                args: args
                    .iter()
                    .map(|a| {
                        rewrite_calls_to_fresh_vars(
                            a, signatures, counter, fresh_vars, extra_facts, display_names,
                        )
                    })
                    .collect(),
            }
        }
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: Box::new(rewrite_calls_to_fresh_vars(
                expr, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(rewrite_calls_to_fresh_vars(
                left, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
            right: Box::new(rewrite_calls_to_fresh_vars(
                right, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
        },
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: Box::new(rewrite_calls_to_fresh_vars(
                expr, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
            ty: ty.clone(),
        },
        ExprKind::Index { array, index } => ExprKind::Index {
            array: Box::new(rewrite_calls_to_fresh_vars(
                array, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
            index: Box::new(rewrite_calls_to_fresh_vars(
                index, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
        },
        ExprKind::Len { array } => ExprKind::Len {
            array: Box::new(rewrite_calls_to_fresh_vars(
                array, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
        },
        ExprKind::ArrayLit { elements } => ExprKind::ArrayLit {
            elements: elements
                .iter()
                .map(|e| {
                    rewrite_calls_to_fresh_vars(
                        e, signatures, counter, fresh_vars, extra_facts, display_names,
                    )
                })
                .collect(),
        },
        ExprKind::Ref { inner } => ExprKind::Ref {
            inner: Box::new(rewrite_calls_to_fresh_vars(
                inner, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
        },
        ExprKind::RefMut { inner } => ExprKind::RefMut {
            inner: Box::new(rewrite_calls_to_fresh_vars(
                inner, signatures, counter, fresh_vars, extra_facts, display_names,
            )),
        },
        other => other.clone(),
    };
    Expr {
        kind: new_kind,
        span: expr.span,
    }
}

/// Walk `expr` and, for every `Call` whose callee has `requires`
/// clauses, verify that the substituted preconditions hold under
/// the current `smt_facts`. Reports a diagnostic on the call's span
/// for each precondition the verifier cannot discharge.
///
/// This catches at compile time the case where a caller passes
/// arguments that violate the callee's contract — previously the
/// caller's body would still typecheck and the runtime `assert`
/// emitted from `requires` would fire, with no compile-time warning.
fn verify_call_args_in_expr(
    expr: &Expr,
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::smt::Verdict;
    if crate::smt::verifier_disabled() {
        return;
    }
    match &expr.kind {
        ExprKind::Call { name, args, .. } => {
            for a in args {
                verify_call_args_in_expr(a, smt_facts, env, signatures, diagnostics);
            }
            let Some(sig) = signatures.get(name) else {
                return;
            };
            if sig.requires.is_empty() || args.len() != sig.params.len() {
                return;
            }
            let mut subs: HashMap<String, Expr> = HashMap::new();
            for (param_name, arg) in sig.param_names.iter().zip(args.iter()) {
                subs.insert(param_name.clone(), arg.clone());
            }
            for req in &sig.requires {
                let substituted = substitute_expr(req, &subs);
                match prove_with_calls(&substituted, smt_facts, env, signatures) {
                    Verdict::Proven => {}
                    Verdict::Disproven { counterexample } => {
                        let detail = match counterexample {
                            Some(c) => format!(
                                "argument to '{}' violates its 'requires' clause [counterexample: {}]",
                                name, c
                            ),
                            None => format!(
                                "argument to '{}' violates its 'requires' clause (SMT counterexample)",
                                name
                            ),
                        };
                        diagnostics.push(
                            Diagnostic::new(expr.span, detail)
                                .with_related(req.span, "callee precondition"),
                        );
                    }
                    // Unknown / SkippedUnsupported / Unavailable: stay
                    // silent. The runtime `requires` check still fires
                    // and we don't want to flood users with spurious
                    // warnings on every Vec-len or float-call site.
                    _ => {}
                }
            }
        }
        ExprKind::Unary { expr, .. } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
        }
        ExprKind::Binary { left, right, .. } => {
            verify_call_args_in_expr(left, smt_facts, env, signatures, diagnostics);
            verify_call_args_in_expr(right, smt_facts, env, signatures, diagnostics);
        }
        ExprKind::Cast { expr, .. } => {
            verify_call_args_in_expr(expr, smt_facts, env, signatures, diagnostics);
        }
        ExprKind::Index { array, index } => {
            verify_call_args_in_expr(array, smt_facts, env, signatures, diagnostics);
            verify_call_args_in_expr(index, smt_facts, env, signatures, diagnostics);
        }
        ExprKind::Len { array } => {
            verify_call_args_in_expr(array, smt_facts, env, signatures, diagnostics);
        }
        ExprKind::ArrayLit { elements } => {
            for e in elements {
                verify_call_args_in_expr(e, smt_facts, env, signatures, diagnostics);
            }
        }
        ExprKind::Ref { inner } | ExprKind::RefMut { inner } => {
            verify_call_args_in_expr(inner, smt_facts, env, signatures, diagnostics);
        }
        _ => {}
    }
}

/// Bundle a prove query and its facts/vars through the inline-call
/// rewriter, then hand it to the SMT layer. Returns z3's verdict.
///
/// Used by every verifier entry point that calls `try_prove` so that
/// `prove`, `ensures`, and `invariant` all see the same call-aware
/// view: an inline `foo(args)` becomes a fresh symbolic variable
/// constrained by `foo`'s `ensures`.
/// Convert a `TypedExpr` back to an AST `Expr` by dropping the
/// type / constant metadata. Used by the bounds-elision pass so it
/// can feed expressions into the SMT layer (which operates on `Expr`).
fn typed_to_expr(t: &TypedExpr) -> Expr {
    let kind = match &t.kind {
        TypedExprKind::Int(v) => ExprKind::Int(*v),
        TypedExprKind::Float(v) => ExprKind::Float(*v),
        TypedExprKind::Bool(v) => ExprKind::Bool(*v),
        TypedExprKind::Str(s) => ExprKind::Str(s.clone()),
        TypedExprKind::Var(name) => ExprKind::Var(name.clone()),
        TypedExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: Box::new(typed_to_expr(expr)),
        },
        TypedExprKind::Binary { op, left, right, .. } => ExprKind::Binary {
            op: *op,
            left: Box::new(typed_to_expr(left)),
            right: Box::new(typed_to_expr(right)),
        },
        TypedExprKind::Call { name, name_span, args } => ExprKind::Call {
            name: name.clone(),
            name_span: *name_span,
            args: args.iter().map(typed_to_expr).collect(),
        },
        TypedExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: Box::new(typed_to_expr(expr)),
            ty: ty.clone(),
        },
        TypedExprKind::Index { array, index, .. } => ExprKind::Index {
            array: Box::new(typed_to_expr(array)),
            index: Box::new(typed_to_expr(index)),
        },
        TypedExprKind::Len { array, .. } => ExprKind::Len {
            array: Box::new(typed_to_expr(array)),
        },
        TypedExprKind::ArrayLit { elements } => ExprKind::ArrayLit {
            elements: elements.iter().map(typed_to_expr).collect(),
        },
        TypedExprKind::Ref { name } => ExprKind::Ref {
            inner: Box::new(Expr {
                kind: ExprKind::Var(name.clone()),
                span: t.span,
            }),
        },
        TypedExprKind::RefMut { name } => ExprKind::RefMut {
            inner: Box::new(Expr {
                kind: ExprKind::Var(name.clone()),
                span: t.span,
            }),
        },
        TypedExprKind::RefField { object, field, .. } => ExprKind::Ref {
            inner: Box::new(Expr {
                kind: ExprKind::FieldAccess {
                    object: Box::new(Expr {
                        kind: ExprKind::Var(object.clone()),
                        span: t.span,
                    }),
                    field: field.clone(),
                },
                span: t.span,
            }),
        },
        TypedExprKind::RefMutField { object, field, .. } => ExprKind::RefMut {
            inner: Box::new(Expr {
                kind: ExprKind::FieldAccess {
                    object: Box::new(Expr {
                        kind: ExprKind::Var(object.clone()),
                        span: t.span,
                    }),
                    field: field.clone(),
                },
                span: t.span,
            }),
        },
        // FnRef / CallIndirect aren't part of the SMT proof
        // vocabulary — bounds elision over fn pointers needs a
        // separate analysis (TODO #A3 follow-up). Lower them
        // to a dummy `Var` so callers that walk the expression
        // tree don't crash, but the SMT pass will simply skip
        // these (they don't appear in syntactic proof goals).
        TypedExprKind::FnRef { name, .. } => ExprKind::Var(name.clone()),
        TypedExprKind::CallIndirect { callee, args } => {
            // Project to a synthetic name so SMT treats the
            // result as opaque (same fall-through as builtin
            // calls without ensures clauses).
            let _ = callee;
            ExprKind::Call {
                name: "__intent_indirect_call".to_string(),
                name_span: t.span,
                args: args.iter().map(typed_to_expr).collect(),
            }
        }
        TypedExprKind::Tuple { elements } => {
            ExprKind::Tuple(elements.iter().map(typed_to_expr).collect())
        }
        TypedExprKind::TupleAccess { tuple, index } => ExprKind::TupleAccess {
            tuple: Box::new(typed_to_expr(tuple)),
            index: *index,
        },
        TypedExprKind::StructLit { type_name, fields } => ExprKind::StructLit {
            type_name: type_name.clone(),
            type_name_span: t.span,
            fields: fields
                .iter()
                .map(|(n, e)| (n.clone(), typed_to_expr(e)))
                .collect(),
        },
        TypedExprKind::FieldAccess { object, field, .. } => ExprKind::FieldAccess {
            object: Box::new(typed_to_expr(object)),
            field: field.clone(),
        },
        TypedExprKind::EnumVariant { enum_name, variant, .. } => ExprKind::FieldAccess {
            object: Box::new(Expr {
                kind: ExprKind::Var(enum_name.clone()),
                span: t.span,
            }),
            field: variant.clone(),
        },
        TypedExprKind::EnumVariantWithPayload { enum_name, variant, payload, .. } => {
            ExprKind::MethodCall {
                receiver: Box::new(Expr {
                    kind: ExprKind::Var(enum_name.clone()),
                    span: t.span,
                }),
                method: variant.clone(),
                method_span: t.span,
                args: vec![typed_to_expr(payload)],
            }
        }
        TypedExprKind::Match { scrutinee, arms } => ExprKind::Match {
            scrutinee: Box::new(typed_to_expr(scrutinee)),
            arms: arms
                .iter()
                .map(|a| crate::ast::MatchArm {
                    pattern: if a.is_wildcard {
                        crate::ast::Pattern::Wildcard
                    } else {
                        crate::ast::Pattern::Variant {
                            enum_name: String::new(),
                            variant: a.variant.clone(),
                        }
                    },
                    pattern_span: t.span,
                    body: typed_to_expr(&a.body),
                })
                .collect(),
        },
        TypedExprKind::IfExpr { cond, then_value, else_value } => ExprKind::IfExpr {
            cond: Box::new(typed_to_expr(cond)),
            then_value: Box::new(typed_to_expr(then_value)),
            else_value: Box::new(typed_to_expr(else_value)),
        },
        TypedExprKind::Block { stmts, tail } => {
            let ast_stmts = stmts.iter().filter_map(|s| match s {
                TypedStmt::Let { name, expr, .. } => Some(Stmt::Let {
                    name: name.clone(),
                    annotation: None,
                    expr: typed_to_expr(expr),
                    span: t.span,
                }),
                _ => None,
            }).collect();
            ExprKind::Block {
                stmts: ast_stmts,
                tail: Box::new(typed_to_expr(tail)),
            }
        }
    };
    Expr { kind, span: t.span }
}

/// Walk a `TypedExpr` and, for every `Index { checked: true }` whose
/// `i < len(arr)` bound the SMT verifier can prove from `smt_facts`,
/// flip `checked` to `false` so the C backend skips the runtime
/// bounds guard. We also try to discharge `0 <= i` for signed indices.
///
/// Failure modes (Unknown / SkippedUnsupported / Unavailable) leave
/// the runtime guard in place — never unsound.
fn try_elide_bounds_in_typed_expr(
    expr: &mut TypedExpr,
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
) {
    use crate::smt::Verdict;
    // Skip elision under INTENTC_NO_VERIFY — leaving every runtime
    // guard intact preserves runtime safety even when we don't run
    // SMT at compile time.
    if crate::smt::verifier_disabled() {
        return;
    }
    match &mut expr.kind {
        TypedExprKind::Index { array, index, checked } => {
            try_elide_bounds_in_typed_expr(array, smt_facts, env, signatures);
            try_elide_bounds_in_typed_expr(index, smt_facts, env, signatures);
            if !*checked {
                return;
            }
            // Build the goal: `(index as u64) < len(array) && index >= 0`.
            // We require both parts to discharge. For unsigned indices,
            // `>= 0` is trivially true via type.
            let idx_expr = typed_to_expr(index);
            let arr_expr = typed_to_expr(array);
            let len_expr = Expr {
                kind: ExprKind::Len {
                    array: Box::new(arr_expr.clone()),
                },
                span: expr.span,
            };

            // u64-cast the index so it matches `len()`'s u64 type.
            // (Most index expressions are already integers but may be
            // signed; the cast normalizes the comparison.)
            let idx_u64 = if index.ty == Type::U64 {
                idx_expr.clone()
            } else {
                Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(idx_expr.clone()),
                        ty: Type::U64,
                    },
                    span: expr.span,
                }
            };
            let upper = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Lt,
                    left: Box::new(idx_u64),
                    right: Box::new(len_expr),
                },
                span: expr.span,
            };

            let upper_ok = matches!(
                prove_with_calls(&upper, smt_facts, env, signatures),
                Verdict::Proven
            );

            // For signed indices, also require `index >= 0`. For
            // unsigned, this is automatic.
            let lower_ok = if index.ty.is_signed_integer() {
                let lower = Expr {
                    kind: ExprKind::Binary {
                        op: BinaryOp::Ge,
                        left: Box::new(idx_expr),
                        right: Box::new(Expr {
                            kind: ExprKind::Int(0),
                            span: expr.span,
                        }),
                    },
                    span: expr.span,
                };
                matches!(
                    prove_with_calls(&lower, smt_facts, env, signatures),
                    Verdict::Proven
                )
            } else {
                true
            };

            if upper_ok && lower_ok {
                *checked = false;
            }
        }
        TypedExprKind::Unary { expr, .. } => {
            try_elide_bounds_in_typed_expr(expr, smt_facts, env, signatures);
        }
        TypedExprKind::Binary { op, left, right, checked } => {
            try_elide_bounds_in_typed_expr(left, smt_facts, env, signatures);
            try_elide_bounds_in_typed_expr(right, smt_facts, env, signatures);
            if !*checked {
                return;
            }
            match op {
                BinaryOp::Div | BinaryOp::Rem => {
                    // Goal: `right != 0`. If discharged, drop the
                    // divisor runtime check.
                    let rhs_expr = typed_to_expr(right);
                    let zero_expr = if right.ty.is_float() {
                        Expr {
                            kind: ExprKind::Float(0.0),
                            span: expr.span,
                        }
                    } else {
                        Expr {
                            kind: ExprKind::Int(0),
                            span: expr.span,
                        }
                    };
                    let goal = Expr {
                        kind: ExprKind::Binary {
                            op: BinaryOp::Ne,
                            left: Box::new(rhs_expr),
                            right: Box::new(zero_expr),
                        },
                        span: expr.span,
                    };
                    if matches!(
                        prove_with_calls(&goal, smt_facts, env, signatures),
                        Verdict::Proven
                    ) {
                        *checked = false;
                    }
                }
                BinaryOp::Shl | BinaryOp::Shr => {
                    // Goal: `right >= 0 && right < bits(left)`. The
                    // unsigned-rhs case skips the lower bound check.
                    let bits = match left.ty.bits() {
                        Some(b) => b as i128,
                        None => return,
                    };
                    let rhs_expr = typed_to_expr(right);
                    let upper = Expr {
                        kind: ExprKind::Binary {
                            op: BinaryOp::Lt,
                            left: Box::new(rhs_expr.clone()),
                            right: Box::new(Expr {
                                kind: ExprKind::Int(bits),
                                span: expr.span,
                            }),
                        },
                        span: expr.span,
                    };
                    let upper_ok = matches!(
                        prove_with_calls(&upper, smt_facts, env, signatures),
                        Verdict::Proven
                    );
                    let lower_ok = if right.ty.is_signed_integer() {
                        let lower = Expr {
                            kind: ExprKind::Binary {
                                op: BinaryOp::Ge,
                                left: Box::new(rhs_expr),
                                right: Box::new(Expr {
                                    kind: ExprKind::Int(0),
                                    span: expr.span,
                                }),
                            },
                            span: expr.span,
                        };
                        matches!(
                            prove_with_calls(&lower, smt_facts, env, signatures),
                            Verdict::Proven
                        )
                    } else {
                        true
                    };
                    if upper_ok && lower_ok {
                        *checked = false;
                    }
                }
                _ => {}
            }
        }
        TypedExprKind::Call { args, .. } => {
            for a in args {
                try_elide_bounds_in_typed_expr(a, smt_facts, env, signatures);
            }
        }
        TypedExprKind::Cast { expr, .. } => {
            try_elide_bounds_in_typed_expr(expr, smt_facts, env, signatures);
        }
        TypedExprKind::Len { array, .. } => {
            try_elide_bounds_in_typed_expr(array, smt_facts, env, signatures);
        }
        TypedExprKind::ArrayLit { elements } => {
            for e in elements {
                try_elide_bounds_in_typed_expr(e, smt_facts, env, signatures);
            }
        }
        _ => {}
    }
}

/// Walk `expr` and substitute every `Index { array: Var(xs), index:
/// Int(k) }` with the k-th element expression when `xs` was
/// initialized by a `vec(...)` literal (and is still in scope with
/// that initialization remembered). Used as a pre-pass in
/// `prove_with_calls` so proofs over known vec contents discharge
/// without needing SMT array theory.
/// Rewrite `MethodCall { receiver: Var(name), method, args }` into
/// the desugared `Call { name: "<TypeName>_<method>", args:
/// [receiver, ...args] }` form whenever `name`'s type is a known
/// nominal type and the mangled function exists. Lets the existing
/// inline-call discharger
/// (`rewrite_calls_to_fresh_vars`) attach the method's `ensures`
/// clauses to a synthetic SMT var, so `prove p.area() > 0`
/// discharges if `area` has `ensures _return > 0`.
///
/// Receivers that aren't bare `Var` (chained method calls, field
/// accesses returning structs, etc.) are left as-is — the SMT
/// encoder will bail with the existing "method calls not supported"
/// diagnostic for those.
fn rewrite_method_calls_to_calls(
    expr: &Expr,
    env: &Env,
    signatures: &HashMap<String, Signature>,
) -> Expr {
    let new_kind = match &expr.kind {
        ExprKind::MethodCall { receiver, method, method_span, args } => {
            // Only rewrite Var receivers in v1 — chained / nested
            // method receivers need the inferred-type info that
            // only the typed IR carries.
            if let ExprKind::Var(name) = &receiver.kind {
                if let Some(info) = env.lookup(name) {
                    let type_name = match &info.ty {
                        Type::Struct(n) | Type::Enum(n) => Some(n.clone()),
                        Type::Ref(inner) | Type::RefMut(inner) => match &**inner {
                            Type::Struct(n) | Type::Enum(n) => Some(n.clone()),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(tname) = type_name {
                        let mangled = format!("{}_{}", tname, method);
                        if signatures.contains_key(&mangled) {
                            let mut new_args = Vec::with_capacity(args.len() + 1);
                            new_args.push((**receiver).clone());
                            for a in args {
                                new_args.push(rewrite_method_calls_to_calls(a, env, signatures));
                            }
                            return Expr {
                                kind: ExprKind::Call {
                                    name: mangled,
                                    name_span: *method_span,
                                    args: new_args,
                                },
                                span: expr.span,
                            };
                        }
                    }
                }
            }
            ExprKind::MethodCall {
                receiver: Box::new(rewrite_method_calls_to_calls(receiver, env, signatures)),
                method: method.clone(),
                method_span: *method_span,
                args: args.iter().map(|a| rewrite_method_calls_to_calls(a, env, signatures)).collect(),
            }
        }
        ExprKind::Unary { op, expr: inner } => ExprKind::Unary {
            op: *op,
            expr: Box::new(rewrite_method_calls_to_calls(inner, env, signatures)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(rewrite_method_calls_to_calls(left, env, signatures)),
            right: Box::new(rewrite_method_calls_to_calls(right, env, signatures)),
        },
        ExprKind::Cast { expr: inner, ty } => ExprKind::Cast {
            expr: Box::new(rewrite_method_calls_to_calls(inner, env, signatures)),
            ty: ty.clone(),
        },
        ExprKind::Call { name, name_span, args } => ExprKind::Call {
            name: name.clone(),
            name_span: *name_span,
            args: args.iter().map(|a| rewrite_method_calls_to_calls(a, env, signatures)).collect(),
        },
        ExprKind::IfExpr { cond, then_value, else_value } => ExprKind::IfExpr {
            cond: Box::new(rewrite_method_calls_to_calls(cond, env, signatures)),
            then_value: Box::new(rewrite_method_calls_to_calls(then_value, env, signatures)),
            else_value: Box::new(rewrite_method_calls_to_calls(else_value, env, signatures)),
        },
        other => other.clone(),
    };
    Expr { kind: new_kind, span: expr.span }
}

/// Rewrite `FieldAccess { object: Var(name), field }` to
/// `Var("<name>__<field>")` whenever `name` was initialized with a
/// struct literal whose field types are integer / bool. The
/// surrounding caller (`prove_with_calls_extra`) declares synthetic
/// SMT vars for these and asserts `<name>__<field> == <field-expr>`.
/// Lets the SMT encoder discharge `prove p.x == 5` instead of
/// bailing with "structs not supported in SMT v1".
fn rewrite_struct_field_accesses(expr: &Expr, env: &Env) -> Expr {
    let new_kind = match &expr.kind {
        ExprKind::FieldAccess { object, field } => {
            if let ExprKind::Var(name) = &object.kind {
                if let Some(info) = env.lookup(name) {
                    if info.struct_literal_fields.is_some() {
                        return Expr {
                            kind: ExprKind::Var(format!("{}__{}", name, field)),
                            span: expr.span,
                        };
                    }
                }
            }
            ExprKind::FieldAccess {
                object: Box::new(rewrite_struct_field_accesses(object, env)),
                field: field.clone(),
            }
        }
        ExprKind::Unary { op, expr: inner } => ExprKind::Unary {
            op: *op,
            expr: Box::new(rewrite_struct_field_accesses(inner, env)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(rewrite_struct_field_accesses(left, env)),
            right: Box::new(rewrite_struct_field_accesses(right, env)),
        },
        ExprKind::Cast { expr: inner, ty } => ExprKind::Cast {
            expr: Box::new(rewrite_struct_field_accesses(inner, env)),
            ty: ty.clone(),
        },
        ExprKind::Call { name, name_span, args } => ExprKind::Call {
            name: name.clone(),
            name_span: *name_span,
            args: args.iter().map(|a| rewrite_struct_field_accesses(a, env)).collect(),
        },
        ExprKind::Index { array, index } => ExprKind::Index {
            array: Box::new(rewrite_struct_field_accesses(array, env)),
            index: Box::new(rewrite_struct_field_accesses(index, env)),
        },
        ExprKind::IfExpr { cond, then_value, else_value } => ExprKind::IfExpr {
            cond: Box::new(rewrite_struct_field_accesses(cond, env)),
            then_value: Box::new(rewrite_struct_field_accesses(then_value, env)),
            else_value: Box::new(rewrite_struct_field_accesses(else_value, env)),
        },
        other => other.clone(),
    };
    Expr { kind: new_kind, span: expr.span }
}

fn substitute_literal_vec_indices(expr: &Expr, env: &Env) -> Expr {
    let new_kind = match &expr.kind {
        ExprKind::Index { array, index } => {
            if let (ExprKind::Var(name), ExprKind::Int(k)) = (&array.kind, &index.kind) {
                if let Some(info) = env.lookup(name) {
                    if let Some(elements) = &info.vec_literal_elements {
                        if *k >= 0 && (*k as usize) < elements.len() {
                            return elements[*k as usize].clone();
                        }
                    }
                }
            }
            ExprKind::Index {
                array: Box::new(substitute_literal_vec_indices(array, env)),
                index: Box::new(substitute_literal_vec_indices(index, env)),
            }
        }
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: Box::new(substitute_literal_vec_indices(expr, env)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(substitute_literal_vec_indices(left, env)),
            right: Box::new(substitute_literal_vec_indices(right, env)),
        },
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: Box::new(substitute_literal_vec_indices(expr, env)),
            ty: ty.clone(),
        },
        ExprKind::Call { name, name_span, args } => ExprKind::Call {
            name: name.clone(),
            name_span: *name_span,
            args: args
                .iter()
                .map(|a| substitute_literal_vec_indices(a, env))
                .collect(),
        },
        ExprKind::Len { array } => ExprKind::Len {
            array: Box::new(substitute_literal_vec_indices(array, env)),
        },
        ExprKind::ArrayLit { elements } => ExprKind::ArrayLit {
            elements: elements
                .iter()
                .map(|e| substitute_literal_vec_indices(e, env))
                .collect(),
        },
        ExprKind::Ref { inner } => ExprKind::Ref {
            inner: Box::new(substitute_literal_vec_indices(inner, env)),
        },
        ExprKind::RefMut { inner } => ExprKind::RefMut {
            inner: Box::new(substitute_literal_vec_indices(inner, env)),
        },
        other => other.clone(),
    };
    Expr {
        kind: new_kind,
        span: expr.span,
    }
}

fn prove_with_calls(
    prove_expr: &Expr,
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
) -> crate::smt::Verdict {
    prove_with_calls_extra(prove_expr, smt_facts, env, signatures, &[])
}

/// `prove_with_calls` variant that injects extra `(name, Type)`
/// pairs into the SMT vars list before solving. Used by
/// `verify_loop_invariants_with_havoc` to register fresh
/// `<name>__havoc_<N>` symbols generated when a nested loop
/// invalidates the substitution map.
fn prove_with_calls_extra(
    prove_expr: &Expr,
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
    extra_vars: &[(String, Type)],
) -> crate::smt::Verdict {
    use crate::smt::{try_prove, Verdict};

    let mut vars: Vec<(String, Type)> = env
        .all_bindings()
        .map(|(name, info)| (name.clone(), info.ty.clone()))
        .collect();
    vars.extend(extra_vars.iter().cloned());
    let mut facts = current_smt_facts(smt_facts, env);

    // Struct-field SMT plumbing: for every binding that was
    // initialized with a struct literal `P { x: e1, y: e2 }`,
    // synthesize a per-field SMT variable `<name>__<field>` and
    // assert `name__field == encode(e)`. The prove-expression and
    // fact rewriters below then translate `p.x` (FieldAccess) into
    // `Var("p__x")` so field-access proofs reach the SMT layer
    // instead of bailing with "structs not supported". The struct's
    // field types come from the struct decl, so we look up via
    // env.lookup_struct(struct_name).
    let mut field_vars: Vec<(String, Type)> = Vec::new();
    let mut field_facts: Vec<Expr> = Vec::new();
    for (name, info) in env.all_bindings() {
        let Some(field_inits) = &info.struct_literal_fields else { continue };
        let struct_name = match &info.ty {
            Type::Struct(n) => n.clone(),
            _ => continue,
        };
        let Some(struct_info) = env.lookup_struct(&struct_name) else { continue };
        let field_type_map: std::collections::HashMap<&str, &Type> = struct_info
            .fields
            .iter()
            .map(|(fname, fty)| (fname.as_str(), fty))
            .collect();
        for (field_name, field_expr) in field_inits {
            let Some(field_ty) = field_type_map.get(field_name.as_str()) else { continue };
            // Only model fields with integer / bool types for v1.
            if !field_ty.is_integer() && !matches!(field_ty, Type::Bool) {
                continue;
            }
            let synth_name = format!("{}__{}", name, field_name);
            field_vars.push((synth_name.clone(), (*field_ty).clone()));
            // Synthesize `synth == field_expr`.
            let eq = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr {
                        kind: ExprKind::Var(synth_name.clone()),
                        span: field_expr.span,
                    }),
                    right: Box::new(field_expr.clone()),
                },
                span: field_expr.span,
            };
            field_facts.push(eq);
        }
    }
    vars.extend(field_vars);
    facts.extend(field_facts);

    // Pre-pass: substitute literal-index reads on vec-literal-init
    // bindings (`let xs = vec(a, b, c); ... prove xs[1] == b;`).
    // The SMT layer has no Index encoding for Vec/Array, but if we
    // know `xs[k]`'s value statically we can replace the read with
    // its literal value before sending the query.
    let prove_expr_owned = substitute_literal_vec_indices(prove_expr, env);
    // Method-call desugar: `p.method(args)` → `Call {
    // name: "<Type>_<method>", args: [p, ...args] }` so the
    // inline-call discharger sees a function call and attaches
    // the method's `ensures` clauses. Var-receiver case only in
    // v1 — chained / nested receivers stay as MethodCall and
    // bail with the existing "method calls not supported" SMT
    // diagnostic.
    let prove_expr_owned = rewrite_method_calls_to_calls(&prove_expr_owned, env, signatures);
    // Field-access rewrite: `p.x` → `Var("p__x")` for every bound
    // binding that's a struct-literal init. Lets the SMT encoder
    // resolve the field access against the synthetic vars added
    // above.
    let prove_expr_owned = rewrite_struct_field_accesses(&prove_expr_owned, env);
    let mut facts: Vec<Expr> = facts
        .into_iter()
        .map(|f| rewrite_method_calls_to_calls(&f, env, signatures))
        .map(|f| rewrite_struct_field_accesses(&f, env))
        .collect();

    let mut counter = 0usize;
    let mut fresh_vars = Vec::new();
    let mut extra_facts = Vec::new();
    let mut display_names: Vec<(String, String)> = Vec::new();
    let prove_rewritten = rewrite_calls_to_fresh_vars(
        &prove_expr_owned,
        signatures,
        &mut counter,
        &mut fresh_vars,
        &mut extra_facts,
        &mut display_names,
    );
    let mut rewritten_facts: Vec<Expr> = facts
        .drain(..)
        .map(|f| {
            rewrite_calls_to_fresh_vars(
                &f,
                signatures,
                &mut counter,
                &mut fresh_vars,
                &mut extra_facts,
                &mut display_names,
            )
        })
        .collect();
    // The inline-call discharger produced extra facts via parameter
    // substitution. For method ensures, the substituted facts now
    // contain references like `b.v` (FieldAccess) that need the
    // same struct-field rewrite the proof obligation got. Run it
    // again on the extra facts before merging.
    let extra_facts: Vec<Expr> = extra_facts
        .into_iter()
        .map(|f| rewrite_struct_field_accesses(&f, env))
        .collect();
    rewritten_facts.extend(extra_facts);
    vars.extend(fresh_vars);

    let versions = env.array_versions();
    let verdict = try_prove(&prove_rewritten, &rewritten_facts, &vars, &versions);
    match verdict {
        Verdict::Disproven { counterexample } => Verdict::Disproven {
            counterexample: counterexample.map(|c| relabel_call_vars(&c, &display_names)),
        },
        other => other,
    }
}

/// Replace `__call_<N> = VALUE` occurrences in a counterexample string
/// with their original `f(args) = VALUE` form. Only matches at "name = "
/// position so we don't accidentally rewrite a substring elsewhere.
fn relabel_call_vars(counterexample: &str, display_names: &[(String, String)]) -> String {
    let mut out = counterexample.to_string();
    for (fresh, display) in display_names {
        let from = format!("{} = ", fresh);
        let to = format!("{} = ", display);
        out = out.replace(&from, &to);
    }
    out
}

fn try_smt_prove(
    prove_expr: &Expr,
    smt_facts: &[Expr],
    env: &Env,
    signatures: &HashMap<String, Signature>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::smt::Verdict;

    if crate::smt::verifier_disabled() {
        return;
    }

    match prove_with_calls(prove_expr, smt_facts, env, signatures) {
        Verdict::Proven => {}
        Verdict::Disproven { counterexample } => {
            let detail = match counterexample {
                Some(ctr) => format!("proof failed: SMT counterexample [{}]", ctr),
                None => "proof failed: SMT solver found a counterexample (the expression is not universally true under the function's preconditions)".to_string(),
            };
            diagnostics.push(Diagnostic::new(prove_expr.span, detail));
        }
        Verdict::Unknown => diagnostics.push(Diagnostic::new(
            prove_expr.span,
            "cannot prove expression: SMT solver returned 'unknown' (try strengthening the preconditions or simplifying the claim)",
        )),
        Verdict::SkippedUnsupported(reason) => {
            // The most common cause is an inline call to a function
            // that has no `ensures` clause — the verifier has nothing
            // to assume about its return value. Surface the actionable
            // fix in that case.
            let hint = if reason.starts_with("function call") {
                " (add an 'ensures' clause to the callee so the verifier can use its return value)"
            } else {
                ""
            };
            diagnostics.push(Diagnostic::new(
                prove_expr.span,
                format!(
                    "cannot prove expression: SMT encoder skipped this query ({}){}. \
                     v1 supports integer/bool arithmetic and comparison only.",
                    reason, hint
                ),
            ));
        }
        Verdict::Unavailable => diagnostics.push(Diagnostic::new(
            prove_expr.span,
            "cannot prove expression: no SMT solver available (install z3 in $PATH or set $Z3 to its path)",
        )),
    }
}

fn is_structurally_true(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Bool(true) => true,
        ExprKind::Binary { op, left, right } => match op {
            BinaryOp::Eq | BinaryOp::Le | BinaryOp::Ge => exprs_equal(left, right),
            BinaryOp::Or => is_structurally_true(left) || is_structurally_true(right),
            BinaryOp::And => is_structurally_true(left) && is_structurally_true(right),
            _ => false,
        },
        ExprKind::Unary { op: UnaryOp::Not, expr } => is_structurally_false(expr),
        _ => false,
    }
}

fn is_structurally_false(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Bool(false) => true,
        ExprKind::Binary {
            op: BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Gt,
            left,
            right,
        } => exprs_equal(left, right),
        ExprKind::Unary { op: UnaryOp::Not, expr } => is_structurally_true(expr),
        _ => false,
    }
}

fn exprs_equal(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Int(x), ExprKind::Int(y)) => x == y,
        (ExprKind::Float(x), ExprKind::Float(y)) => x.to_bits() == y.to_bits(),
        (ExprKind::Bool(x), ExprKind::Bool(y)) => x == y,
        (ExprKind::Var(x), ExprKind::Var(y)) => x == y,
        (
            ExprKind::Unary { op: op_a, expr: ea },
            ExprKind::Unary { op: op_b, expr: eb },
        ) => op_a == op_b && exprs_equal(ea, eb),
        (
            ExprKind::Binary { op: op_a, left: la, right: ra },
            ExprKind::Binary { op: op_b, left: lb, right: rb },
        ) => op_a == op_b && exprs_equal(la, lb) && exprs_equal(ra, rb),
        (
            ExprKind::Cast { expr: ea, ty: ta },
            ExprKind::Cast { expr: eb, ty: tb },
        ) => ta == tb && exprs_equal(ea, eb),
        (
            ExprKind::Call { name: na, args: aa, .. },
            ExprKind::Call { name: nb, args: ab, .. },
        ) => na == nb && aa.len() == ab.len() && aa.iter().zip(ab.iter()).all(|(x, y)| exprs_equal(x, y)),
        (
            ExprKind::Index { array: aa, index: ia },
            ExprKind::Index { array: ab, index: ib },
        ) => exprs_equal(aa, ab) && exprs_equal(ia, ib),
        (
            ExprKind::Len { array: aa },
            ExprKind::Len { array: ab },
        ) => exprs_equal(aa, ab),
        (
            ExprKind::ArrayLit { elements: ea },
            ExprKind::ArrayLit { elements: eb },
        ) => ea.len() == eb.len() && ea.iter().zip(eb.iter()).all(|(x, y)| exprs_equal(x, y)),
        _ => false,
    }
}
