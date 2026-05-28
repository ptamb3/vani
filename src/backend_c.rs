//! C backend.
//!
//! # TODO(deprecate): C backend slated for removal.
//!
//! LLVM is the project's default and primary backend
//! ([src/backend_llvm.rs](backend_llvm.rs)). The C backend is kept
//! for back-compat — `intentc emit-c` and `intentc emit --backend=c`
//! still work, and the `llvm_backend_run_produces_same_output_as_c`
//! integration test diffs the two on every example to guard against
//! divergence.
//!
//! When the LLVM backend has had enough run time in production:
//! - Remove `CBackend` and this module.
//! - Drop the `emit-c` subcommand alias from `src/main.rs`.
//! - Drop the `--backend=c` path from `parse_emit_args`.
//! - Retire the cross-backend equivalence test (it'll have no C path
//!   to compare against).
//! - Audit and remove the C-pinned tests in `lib.rs` that assert on
//!   `intent_check_*` / `v_*` C-specific identifiers.

use crate::ast::{BinaryOp, Type, UnaryOp};
use crate::backend::Backend;
use crate::ir::{TypedExpr, TypedExprKind, TypedFunction, TypedProgram, TypedStmt};
use std::collections::BTreeSet;

pub struct CBackend;

impl Backend for CBackend {
    fn name(&self) -> &'static str {
        "c"
    }

    fn emit(&self, program: &TypedProgram) -> String {
        emit_c(program)
    }
}

thread_local! {
    /// Per-program buffer for outlined task bodies. emit_stmt
    /// for `TypedStmt::TaskSpawn` appends one `static void*
    /// intent_task_<n>(void* ctx_raw) { … }` per spawn site
    /// here; emit_c prepends the buffer between the runtime
    /// preamble and the user functions so the outline name
    /// is visible at the spawn-site call.
    static TASK_OUTLINES: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    /// Monotonic counter assigning outline IDs. Reset at the
    /// start of every `emit_c` call.
    static TASK_OUTLINE_COUNTER: std::cell::Cell<u32> = std::cell::Cell::new(0);
    /// Closure #269: set of `extern "C"` fn names. Populated at
    /// the start of `emit_c` from any `is_extern` function in
    /// the program. Consulted by the Call emitter to choose the
    /// bare C-ABI name (no `fn_` prefix).
    pub(crate) static C_EXTERN_FN_REGISTRY:
        std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// Per-program registry of enum payload types. Populated
    /// at the start of `emit_c` from `program.enums`. Maps
    /// each enum name → `Some(payload_ty)` if any variant has
    /// a payload (v1 requires all payloaded variants to share
    /// the same payload type), or `None` for plain enums.
    /// Consulted by `c_type_name(Type::Enum)` so payloaded
    /// enums route to the tagged-union struct typedef instead
    /// of the bare `int32_t` tag. T1.3 phase 2b.
    static ENUM_PAYLOAD_REGISTRY: std::cell::RefCell<std::collections::HashMap<String, Type>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Per-program registry of struct field lists. Populated at
    /// the start of `emit_c` from `program.structs` and consulted
    /// by the `TypedStmt::Drop` handler to free each owning
    /// (`OwnedStr`) field when the struct binding goes out of
    /// scope. T1.2 phase 2b.
    static STRUCT_FIELDS_REGISTRY: std::cell::RefCell<std::collections::HashMap<String, Vec<(String, Type)>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Names of structs / enums that have an `implement Drop
    /// for T` impl in the program (hoisted to `T_drop`).
    /// Populated at the start of `emit_c` from the function
    /// table. Consulted by the `TypedStmt::Drop` handler to
    /// auto-call the user's `drop(self)` method at scope exit
    /// when the type has no owning fields. T2.7 phase 2.
    static USER_DROP_REGISTRY: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// Per-enum list of variant tags that carry a payload.
    /// Populated alongside `ENUM_PAYLOAD_REGISTRY` at the start
    /// of `emit_c`. The Drop handler reads this to switch on
    /// the active tag and free the heap payload only when one
    /// of the listed variants is in scope. T1.3 + T1.2 phase 2b.
    static ENUM_PAYLOAD_TAGS_REGISTRY:
        std::cell::RefCell<std::collections::HashMap<String, Vec<u32>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Closure #283: per-variant payload-type registry,
    /// keyed by enum name. Stores `Vec<(variant_name,
    /// Option<Type>)>` in declaration order. Populated for
    /// every enum that has at least one payloaded variant.
    /// Enables mixed-payload-type enums (e.g. `Result<T,
    /// E> { Ok(T), Err(E) }`) by routing each variant's
    /// payload through its own union member, rather than
    /// forcing all variants to share a single payload type.
    pub(crate) static ENUM_VARIANT_PAYLOADS_REGISTRY:
        std::cell::RefCell<
            std::collections::HashMap<String, Vec<(String, Option<Type>)>>
        > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Per-name C struct typedef for a payloaded enum. Prefixed
/// with `Enum_` so the emitted C identifier is distinct from
/// any builtin. T1.3 phase 2b.
pub(crate) fn enum_c_name(name: &str) -> String {
    format!("Enum_{}", name)
}

/// Return true if any variant of this enum carries a payload.
/// T1.3 phase 2b.
fn enum_has_payload(decl: &crate::ir::TypedEnumDecl) -> bool {
    decl.payload_types.iter().any(|p| p.is_some())
}

/// Closure #283: true if this enum carries variants with
/// payloads of differing types (e.g. `Result<i64, OwnedStr>`
/// where Ok carries i64 and Err carries OwnedStr). Triggers
/// the new union-layout codegen path. Single-payload-type
/// enums stay on the legacy `{ tag; T payload; }` layout for
/// back-compat (no test breakage).
fn enum_has_mixed_payloads(decl: &crate::ir::TypedEnumDecl) -> bool {
    let payloaded: Vec<&Type> = decl
        .payload_types
        .iter()
        .filter_map(|p| p.as_ref())
        .collect();
    if payloaded.len() < 2 {
        return false;
    }
    let first = payloaded[0];
    payloaded[1..].iter().any(|t| *t != first)
}

/// Closure #283: C identifier for the per-variant union
/// member name. `v_Ok`, `v_Err`, etc. Mirrored on the LLVM
/// side via `intent_enum_v_<variant>`-style globals.
pub(crate) fn enum_variant_member(variant: &str) -> String {
    format!("v_{}", variant)
}

/// Common payload type across all payloaded variants of the
/// enum. Returns None for payload-less enums. Assumes the
/// checker has already validated uniformity. T1.3 phase 2b.
fn enum_common_payload_ty(decl: &crate::ir::TypedEnumDecl) -> Option<Type> {
    decl.payload_types.iter().find_map(|p| p.clone())
}

pub fn emit_c(program: &TypedProgram) -> String {
    TASK_OUTLINES.with(|b| b.borrow_mut().clear());
    TASK_OUTLINE_COUNTER.with(|c| c.set(0));
    // Closure #269: populate the extern-fn registry from the
    // program's extern declarations. The Call emitter consults
    // this to skip the `fn_` prefix on calls to FFI symbols.
    C_EXTERN_FN_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for f in &program.functions {
            if f.is_extern {
                reg.insert(f.name.clone());
            }
        }
    });
    // Populate the enum payload registry from the program's
    // enum decls so `c_type_name(Type::Enum)` routes
    // payloaded enums to their tagged-union struct typedef.
    // T1.3 phase 2b.
    ENUM_PAYLOAD_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for decl in &program.enums {
            if let Some(payload_ty) = enum_common_payload_ty(decl) {
                reg.insert(decl.name.clone(), payload_ty);
            }
        }
    });
    ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for decl in &program.enums {
            let tags: Vec<u32> = decl
                .payload_types
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.as_ref().map(|_| i as u32))
                .collect();
            if !tags.is_empty() {
                reg.insert(decl.name.clone(), tags);
            }
        }
    });
    // Closure #283: per-variant payload registry. Populated
    // for every payloaded enum (uniform OR mixed) so the
    // codegen sites can choose the right access pattern.
    ENUM_VARIANT_PAYLOADS_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for decl in &program.enums {
            if !enum_has_payload(decl) {
                continue;
            }
            let pairs: Vec<(String, Option<Type>)> = decl
                .variants
                .iter()
                .zip(decl.payload_types.iter())
                .map(|(name, pty)| (name.clone(), pty.clone()))
                .collect();
            reg.insert(decl.name.clone(), pairs);
        }
    });
    STRUCT_FIELDS_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for decl in &program.structs {
            reg.insert(decl.name.clone(), decl.fields.clone());
        }
    });
    USER_DROP_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for f in &program.functions {
            if let Some(type_name) = f.name.strip_suffix("_drop") {
                reg.insert(type_name.to_string());
            }
        }
    });
    // Emit the body first (Vec bundles + intents + functions + main),
    // then prepend includes + only the runtime helpers it actually
    // references. Keeps the generated C tidy when SMT elision discharges
    // all the runtime guards.
    let mut body = String::new();

    let mut vec_elements = BTreeSet::<String>::new();
    let mut element_types: Vec<Type> = Vec::new();
    let mut channel_seen = BTreeSet::<String>::new();
    let mut channel_specs: Vec<(Type, u64)> = Vec::new();
    let mut tuple_seen = BTreeSet::<String>::new();
    let mut tuple_shapes: Vec<Vec<Type>> = Vec::new();
    for function in &program.functions {
        collect_vec_elements(&function.return_type, &mut vec_elements, &mut element_types);
        collect_channel_specs(
            &function.return_type,
            &mut channel_seen,
            &mut channel_specs,
        );
        collect_tuple_shapes(
            &function.return_type,
            &mut tuple_seen,
            &mut tuple_shapes,
        );
        for param in &function.params {
            collect_vec_elements(&param.ty, &mut vec_elements, &mut element_types);
            collect_channel_specs(&param.ty, &mut channel_seen, &mut channel_specs);
            collect_tuple_shapes(&param.ty, &mut tuple_seen, &mut tuple_shapes);
        }
        for stmt in &function.body {
            collect_vec_elements_in_stmt(stmt, &mut vec_elements, &mut element_types);
            collect_channel_specs_in_stmt(stmt, &mut channel_seen, &mut channel_specs);
            collect_tuple_shapes_in_stmt(stmt, &mut tuple_seen, &mut tuple_shapes);
        }
    }
    // Collect any Vec element types referenced from struct
    // fields and emit those Vec bundles BEFORE the struct
    // typedefs, so a `struct Bag { contents: Vec<i64> }`
    // resolves `intent_vec_int64_t` at its own declaration.
    // Track the early-emitted set so the post-struct pass
    // doesn't re-emit the same bundle. T1.2 phase 2b.
    let mut struct_field_vec_seen = BTreeSet::<String>::new();
    let mut struct_field_vec_elements: Vec<Type> = Vec::new();
    for decl in &program.structs {
        for (_, fty) in &decl.fields {
            collect_vec_elements(fty, &mut struct_field_vec_seen, &mut struct_field_vec_elements);
        }
    }
    // Enum payload types may also be Vec<T>. Walk
    // `program.enums` for each payloaded variant and queue
    // any Vec element types so the bundle is in scope when
    // the `typedef struct { int32_t tag; intent_vec_<T>
    // payload; } Enum_<Name>;` line lands further below.
    // Closure #118.
    for decl in &program.enums {
        for payload in &decl.payload_types {
            if let Some(ty) = payload {
                collect_vec_elements(ty, &mut struct_field_vec_seen, &mut struct_field_vec_elements);
            }
        }
    }
    let mut emitted_vec_bundles: BTreeSet<String> = BTreeSet::new();
    for element in &struct_field_vec_elements {
        emit_vec_bundle(element, &mut body);
        emitted_vec_bundles.insert(element_tag(element));
    }
    if !struct_field_vec_elements.is_empty() {
        body.push('\n');
    }
    // Vtables Phase 4: forward-declare the per-Iface vtable
    // tag + `intent_dyn_<Iface>` fat-pointer typedef BEFORE
    // struct typedefs so structs can carry `dyn Iface`
    // fields. The full vtable struct body is emitted AFTER
    // struct typedefs by `emit_dyn_iface_vtable_bodies`
    // (its fn-ptr slots may reference `Struct_<T>` types).
    let used_dyn_ifaces_forward = collect_used_dyn_ifaces(program);
    emit_dyn_iface_typedefs(&mut body, &used_dyn_ifaces_forward);
    if !used_dyn_ifaces_forward.is_empty() {
        body.push('\n');
    }
    // Emit user-declared struct typedefs. Topologically sort
    // first so a struct that references another by value
    // (direct field of `Struct(S)` or `[S; N]`) is emitted
    // AFTER that other struct's typedef. C requires the full
    // type to be visible before use; LLVM forward-declares
    // named types so it doesn't need this. Source order
    // would otherwise emit `struct Outer { Struct_Inner inner; }`
    // before `Struct_Inner` is declared. Vec/Ref/RefMut
    // /Atomic/Mutex/Guard/Channel/Tuple all introduce
    // pointer-shaped indirection through their own typedef
    // bundles, so they don't drive struct dependencies.
    // Closure #164.
    fn struct_deps(ty: &Type, out: &mut Vec<String>) {
        match ty {
            Type::Struct(name) => out.push(name.clone()),
            Type::Array { element, .. } => struct_deps(element, out),
            _ => {}
        }
    }
    fn visit(
        name: &str,
        by_name: &std::collections::HashMap<&str, &crate::ir::TypedStructDecl>,
        emitted: &mut std::collections::HashSet<String>,
        on_stack: &mut std::collections::HashSet<String>,
        body: &mut String,
    ) {
        if emitted.contains(name) || on_stack.contains(name) {
            return;
        }
        let Some(decl) = by_name.get(name) else {
            return;
        };
        on_stack.insert(name.to_string());
        let mut deps: Vec<String> = Vec::new();
        for (_, fty) in &decl.fields {
            struct_deps(fty, &mut deps);
        }
        for dep in deps {
            visit(&dep, by_name, emitted, on_stack, body);
        }
        emit_struct_bundle(decl, body);
        emitted.insert(name.to_string());
        on_stack.remove(name);
    }
    let mut by_name: std::collections::HashMap<&str, &crate::ir::TypedStructDecl> =
        std::collections::HashMap::new();
    for d in &program.structs {
        by_name.insert(d.name.as_str(), d);
    }
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut on_stack: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in &program.structs {
        visit(&d.name, &by_name, &mut emitted, &mut on_stack, &mut body);
    }
    if !program.structs.is_empty() {
        body.push('\n');
    }
    // Emit a per-name C struct typedef for each payloaded
    // enum. Layout: `typedef struct { int32_t tag; T payload;
    // } Enum_<Name>;` where T is the shared payload type for
    // all payload-bearing variants. Plain enums stay as
    // bare `int32_t` tags (no typedef needed). T1.3 phase 2b.
    let mut any_enum_emitted = false;
    for decl in &program.enums {
        if !enum_has_payload(decl) {
            continue;
        }
        // Closure #283: mixed-payload-type enums lay out
        // each variant's payload through its own union
        // member, keyed by variant name (`u.v_<variant>`).
        // Single-payload-type enums keep the legacy
        // `{ tag; T payload; }` layout for back-compat.
        if enum_has_mixed_payloads(decl) {
            body.push_str(&format!(
                "typedef struct {{ int32_t tag; union {{\n",
            ));
            for (variant, pty) in decl.variants.iter().zip(decl.payload_types.iter()) {
                let Some(payload_ty) = pty.as_ref() else {
                    continue;
                };
                let member = enum_variant_member(variant);
                // Closure #283: `c_type_name` resolves
                // payload-less enums (Type::Enum without
                // `Enum_` typedef) to their `int32_t` tag
                // form, avoiding "Enum_AllocError
                // undeclared" errors when a mixed-payload
                // variant's payload is itself a payload-less
                // enum. `c_element_storage` is even safer for
                // struct payloads since it routes nested
                // Vec / nested-struct types correctly.
                let payload_decl = match payload_ty {
                    Type::Array { .. } => format_declarator(payload_ty, &member),
                    _ => format!("{} {}", c_element_storage(payload_ty), member),
                };
                body.push_str(&format!("    {};\n", payload_decl));
            }
            body.push_str(&format!("}} u; }} {};\n", enum_c_name(&decl.name)));
            any_enum_emitted = true;
            continue;
        }
        let payload_ty = match enum_common_payload_ty(decl) {
            Some(ty) => ty,
            None => continue,
        };
        // Array payloads need the `T name[N]` declarator
        // form rather than `intent_arr<N>_<T> name` (which
        // would require the typedef and complicate the
        // initializer story). Mirrors the struct-field array
        // handling from closure #100. Closure #119.
        let payload_decl = match &payload_ty {
            Type::Array { .. } => format_declarator(&payload_ty, "payload"),
            _ => format!("{} payload", c_type_name(&payload_ty)),
        };
        body.push_str(&format!(
            "typedef struct {{ int32_t tag; {}; }} {};\n",
            payload_decl,
            enum_c_name(&decl.name)
        ));
        any_enum_emitted = true;
    }
    if any_enum_emitted {
        body.push('\n');
    }
    // Emit tuple typedefs BEFORE vec / array typedefs so a
    // `Vec<(i64, i64)>` element can reference the tuple
    // struct. Inner-first dedup keeps nested tuples (when
    // we lift the Copy-only restriction later) ordered
    // correctly. T1.1.
    for shape in &tuple_shapes {
        emit_tuple_bundle(shape, &mut body);
    }
    if !tuple_shapes.is_empty() {
        body.push('\n');
    }
    // Per-shape array typedefs for any `Array<T, N>` that
    // appears as a Vec element (a `Vec<[i64; 4]>` needs
    // `typedef int64_t intent_arr4_int64_t[4];` in scope
    // before its helper bundle). Refines #7 phase 2c. Walks
    // only the Vec-element axis since arrays NOT inside Vecs
    // stay inlined in their declarators.
    let mut array_typedefs_seen = BTreeSet::<String>::new();
    for element in &element_types {
        emit_array_typedefs_for(element, &mut array_typedefs_seen, &mut body);
    }
    if !array_typedefs_seen.is_empty() {
        body.push('\n');
    }

    // Closure #239: array-return struct wrappers. C can't
    // return values of bare array type (arrays decay to
    // pointers in return position), so we wrap each array
    // return type in a per-shape `typedef struct { T data[N]; }
    // intent_arr_ret_<N>_<T>;` and emit at fn boundaries:
    // - Prototype/definition use the struct as the return
    //   type spelling (instead of the placeholder
    //   `/* array */`).
    // - Return statement wraps the array value in the struct
    //   compound literal.
    // - Let from an array-returning call unwraps via
    //   `__tmp.data` + memcpy to populate the local array.
    let mut array_return_seen = BTreeSet::<String>::new();
    for function in &program.functions {
        if let Type::Array { element, length } = &function.return_type {
            let name = array_return_struct_name(element, *length);
            if array_return_seen.insert(name.clone()) {
                body.push_str(&format!(
                    "typedef struct {{ {} data[{}]; }} {};\n",
                    c_element_storage(element),
                    length,
                    name,
                ));
            }
        }
    }
    if !array_return_seen.is_empty() {
        body.push('\n');
    }
    for element in &element_types {
        // Skip Vec bundles already emitted in the pre-struct
        // pass for fields like `struct Bag { contents: Vec<i64> }`.
        // T1.2 phase 2b.
        if emitted_vec_bundles.contains(&element_tag(element)) {
            continue;
        }
        emit_vec_bundle(element, &mut body);
    }

    for intent in &program.intents {
        body.push_str("/* intent: ");
        body.push_str(&escape_comment(intent));
        body.push_str(" */\n");
    }
    if !program.intents.is_empty() {
        body.push('\n');
    }

    let used_dyn_ifaces = collect_used_dyn_ifaces(program);
    // Phase 4: the full vtable struct body needs `Struct_<T>`
    // visible, so emit it AFTER struct typedefs (line below).
    // The body is appended to a separate buffer that gets
    // spliced in after structs are declared.
    emit_dyn_iface_vtable_bodies(&mut body, &used_dyn_ifaces);

    for function in &program.functions {
        emit_prototype(function, &mut body);
    }
    body.push('\n');

    emit_dyn_iface_vtables(&mut body, &used_dyn_ifaces);

    // Emit function bodies into a separate buffer so the
    // task-outlining side-effect (TASK_OUTLINES) can be
    // spliced between the prototypes and the bodies. Task
    // outlines call user functions, so they need to see the
    // prototypes but be defined before the function bodies
    // that reference the outline names.
    let mut function_bodies = String::new();
    for function in &program.functions {
        emit_function(function, &mut function_bodies);
        function_bodies.push('\n');
    }
    // Splice outlines between prototypes and function bodies.
    TASK_OUTLINES.with(|b| {
        let outlines = std::mem::take(&mut *b.borrow_mut());
        body.push_str(&outlines);
    });
    body.push_str(&function_bodies);

    body.push_str("int main(void) {\n");
    body.push_str("  return (int)fn_main();\n");
    body.push_str("}\n");

    let mut out = String::new();
    out.push_str("#include <assert.h>\n");
    out.push_str("#include <stdatomic.h>\n");
    out.push_str("#include <stdbool.h>\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include <stdio.h>\n");
    out.push_str("#include <stdlib.h>\n");
    out.push_str("#include <string.h>\n");
    // INTENT_UNUSED is referenced by every Vec helper and
    // by the threading wrappers below, so define it
    // unconditionally even if no runtime guard helpers
    // survived the SMT-elision pass.
    out.push_str("#if defined(__GNUC__) || defined(__clang__)\n");
    out.push_str("#define INTENT_UNUSED __attribute__((unused))\n");
    out.push_str("#else\n");
    out.push_str("#define INTENT_UNUSED\n");
    out.push_str("#endif\n\n");
    emit_intent_thread_wrappers_c(&mut out);
    emit_runtime_helpers(&mut out, &body);
    emit_intent_str_concat_c(&mut out);
    emit_concurrency_runtime_helpers(&mut out, &body, &channel_specs);
    out.push_str(&body);
    out
}

/// Emit the runtime helpers for `Channel<i64>` and `Mutex<i64>`
/// when the generated body references them. Both helpers are
/// header-only (static inline) so they participate in linkage
/// without an out-of-tree runtime library; the C11 atomics
/// `<stdatomic.h>` include is already in the preamble. The
/// substring check on `body` keeps the helpers from showing up
/// in programs that don't use them.
/// Emit a per-(T, N) Vyukov MPSC ring buffer struct + the
/// three operation helpers (new / send / recv). The struct
/// layout mirrors what the previous i64/16-only emit produced;
/// the element width and capacity are now substituted in.
fn emit_channel_bundle(element: &Type, capacity: u64, out: &mut String) {
    let struct_name = c_channel_storage(element, capacity);
    let struct_name_upper = struct_name.to_uppercase();
    let cap_macro = format!("{}_CAP", struct_name_upper);
    let mask_expr = format!("({} - 1)", cap_macro);
    let elem_c = c_leaf_type(element);
    let new_fn = c_channel_helper(element, capacity, "new");
    let send_fn = c_channel_helper(element, capacity, "send");
    let recv_fn = c_channel_helper(element, capacity, "recv");
    out.push_str(&format!(
        "#define {cap} {capacity}\n\
         typedef struct {{\n\
         \x20 {elem} buf[{cap}];\n\
         \x20 /* Per-slot publication sequence number — Vyukov MPSC.\n\
         \x20    seq[i]==n means slot i is in round n. Producer enters round\n\
         \x20    t when seq[t & MASK]==t and publishes via seq=t+1; consumer\n\
         \x20    enters round h when seq==h+1 and releases via seq=h+CAP. */\n\
         \x20 _Atomic int64_t seq[{cap}];\n\
         \x20 _Atomic int64_t head;\n\
         \x20 _Atomic int64_t tail;\n\
         }} {struct_name};\n\
         static {elem} {send}({struct_name}* c, {elem} v) INTENT_UNUSED;\n\
         static {elem} {send}({struct_name}* c, {elem} v) {{\n\
         \x20 int64_t t;\n\
         \x20 while (1) {{\n\
         \x20   t = atomic_load_explicit(&c->tail, memory_order_seq_cst);\n\
         \x20   int64_t s = atomic_load_explicit(&c->seq[t & {mask}], memory_order_seq_cst);\n\
         \x20   int64_t diff = s - t;\n\
         \x20   if (diff == 0) {{\n\
         \x20     int64_t expected = t;\n\
         \x20     if (atomic_compare_exchange_strong_explicit(&c->tail, &expected, t + 1, memory_order_seq_cst, memory_order_seq_cst)) {{\n\
         \x20       break;\n\
         \x20     }}\n\
         \x20   }} else if (diff < 0) {{\n\
         \x20     /* channel full — slot t still holds round t-CAP data */\n\
         \x20   }}\n\
         \x20   /* else: another producer raced ahead; reload tail */\n\
         \x20 }}\n\
         \x20 c->buf[t & {mask}] = v;\n\
         \x20 atomic_store_explicit(&c->seq[t & {mask}], t + 1, memory_order_seq_cst);\n\
         \x20 return v;\n\
         }}\n\
         static {elem} {recv}({struct_name}* c) INTENT_UNUSED;\n\
         static {elem} {recv}({struct_name}* c) {{\n\
         \x20 int64_t h = atomic_load_explicit(&c->head, memory_order_seq_cst);\n\
         \x20 while (1) {{\n\
         \x20   int64_t s = atomic_load_explicit(&c->seq[h & {mask}], memory_order_seq_cst);\n\
         \x20   if (s == h + 1) break;\n\
         \x20 }}\n\
         \x20 {elem} v = c->buf[h & {mask}];\n\
         \x20 atomic_store_explicit(&c->seq[h & {mask}], h + {cap}, memory_order_seq_cst);\n\
         \x20 atomic_store_explicit(&c->head, h + 1, memory_order_seq_cst);\n\
         \x20 return v;\n\
         }}\n\
         static {struct_name} {new}(void) INTENT_UNUSED;\n\
         static {struct_name} {new}(void) {{\n\
         \x20 {struct_name} c;\n\
         \x20 for (int i = 0; i < {cap}; i++) c.buf[i] = ({elem})0;\n\
         \x20 for (int i = 0; i < {cap}; i++) atomic_store_explicit(&c.seq[i], (int64_t)i, memory_order_seq_cst);\n\
         \x20 atomic_store_explicit(&c.head, 0, memory_order_seq_cst);\n\
         \x20 atomic_store_explicit(&c.tail, 0, memory_order_seq_cst);\n\
         \x20 return c;\n\
         }}\n\n",
        cap = cap_macro,
        capacity = capacity,
        elem = elem_c,
        mask = mask_expr,
        struct_name = struct_name,
        new = new_fn,
        send = send_fn,
        recv = recv_fn,
    ));
}

fn emit_concurrency_runtime_helpers(
    out: &mut String,
    body: &str,
    channel_specs: &[(Type, u64)],
) {
    let needs_mutex = body.contains("intent_mutex_i64") || body.contains("intent_guard_i64");
    let needs_tasks = body.contains("intent_task_handle");
    if needs_tasks {
        // Handle: pthread thread id + ctx pointer (for free
        // at join time). Task body lowering emits an outline
        // function per spawn site whose signature is
        // `void* fn(void* ctx)`.
        out.push_str(
            "typedef struct { intent_thread_t thread; void* ctx; } intent_task_handle;\n\n",
        );
    }
    for (element, capacity) in channel_specs {
        emit_channel_bundle(element, *capacity, out);
    }
    if needs_mutex {
        emit_intent_mutex_helpers_c(out);
    }
}

/// Emit the i64-only `Mutex` / `Guard` runtime helpers
/// (Drepper three-state futex lock on Linux,
/// WaitOnAddress/WakeByAddress on Windows, sched_yield
/// fallback elsewhere). Shared between tree-C and SSA-C —
/// always-safe to emit, but typically only fires when the
/// program actually uses `Mutex<i64>` / `Guard<i64>` (the
/// caller does the substring check).
pub(crate) fn emit_intent_mutex_helpers_c(out: &mut String) {
    out.push_str(
        "/* Drepper-style three-state futex lock. State 0 = unlocked, 1 =\n\
             \x20  locked-no-waiters, 2 = locked-waiters-present. Lock attempts\n\
             \x20  CAS 0->1 for the uncontended fast path; on contention it\n\
             \x20  marks state=2 (atomic_exchange) then parks in the kernel via\n\
             \x20  the host's kernel-wait primitive (futex on Linux,\n\
             \x20  WaitOnAddress on Windows) until the unlocker stores 0 and\n\
             \x20  wakes it. Unlock optimizes for the no-waiters case: an\n\
             \x20  `atomic_fetch_sub` of 1 against state returns 1 on the\n\
             \x20  fast path (was-1, now-0; nothing to wake); on the slow\n\
             \x20  path it returns 2, the unlocker resets state to 0 and\n\
             \x20  wakes one waiter. Other platforms fall back to the\n\
             \x20  intent_thread_yield backoff. */\n\
             #if defined(__linux__)\n\
             # include <linux/futex.h>\n\
             # include <sys/syscall.h>\n\
             # include <unistd.h>\n\
             static long intent_mutex_futex_wait(_Atomic int* p, int v) INTENT_UNUSED;\n\
             static long intent_mutex_futex_wait(_Atomic int* p, int v) {\n\
             \x20 return syscall(SYS_futex, (int*)p, FUTEX_WAIT_PRIVATE, v, (void*)0, (void*)0, 0);\n\
             }\n\
             static long intent_mutex_futex_wake(_Atomic int* p, int n) INTENT_UNUSED;\n\
             static long intent_mutex_futex_wake(_Atomic int* p, int n) {\n\
             \x20 return syscall(SYS_futex, (int*)p, FUTEX_WAKE_PRIVATE, n, (void*)0, (void*)0, 0);\n\
             }\n\
             #elif defined(_WIN32)\n\
             static long intent_mutex_futex_wait(_Atomic int* p, int v) INTENT_UNUSED;\n\
             static long intent_mutex_futex_wait(_Atomic int* p, int v) {\n\
             \x20 int compare = v;\n\
             \x20 WaitOnAddress((volatile VOID*)p, &compare, sizeof(int), INFINITE);\n\
             \x20 return 0;\n\
             }\n\
             static long intent_mutex_futex_wake(_Atomic int* p, int n) INTENT_UNUSED;\n\
             static long intent_mutex_futex_wake(_Atomic int* p, int n) {\n\
             \x20 if (n == 1) WakeByAddressSingle((PVOID)p);\n\
             \x20 else WakeByAddressAll((PVOID)p);\n\
             \x20 return 0;\n\
             }\n\
             #endif\n\
             typedef struct { int64_t value; _Atomic int locked; } intent_mutex_i64;\n\
             typedef struct { intent_mutex_i64* m; } intent_guard_i64;\n\
             static intent_mutex_i64 intent_mutex_i64_new(int64_t initial) INTENT_UNUSED;\n\
             static intent_mutex_i64 intent_mutex_i64_new(int64_t initial) {\n\
             \x20 intent_mutex_i64 m;\n\
             \x20 m.value = initial;\n\
             \x20 atomic_store_explicit(&m.locked, 0, memory_order_seq_cst);\n\
             \x20 return m;\n\
             }\n\
             static intent_guard_i64 intent_mutex_i64_lock(intent_mutex_i64* m) INTENT_UNUSED;\n\
             static intent_guard_i64 intent_mutex_i64_lock(intent_mutex_i64* m) {\n\
             #if defined(__linux__) || defined(_WIN32)\n\
             \x20 int c = 0;\n\
             \x20 if (!atomic_compare_exchange_strong_explicit(&m->locked, &c, 1, memory_order_seq_cst, memory_order_seq_cst)) {\n\
             \x20   /* Slow path: mark state=2 (waiter present) then park. */\n\
             \x20   if (c != 2) c = atomic_exchange_explicit(&m->locked, 2, memory_order_seq_cst);\n\
             \x20   while (c != 0) {\n\
             \x20     intent_mutex_futex_wait(&m->locked, 2);\n\
             \x20     c = atomic_exchange_explicit(&m->locked, 2, memory_order_seq_cst);\n\
             \x20   }\n\
             \x20 }\n\
             #else\n\
             \x20 /* Other platforms: intent_thread_yield backoff (less efficient\n\
             \x20    but correct). See the futex/WaitOnAddress branch above. */\n\
             \x20 int expected = 0;\n\
             \x20 int spins = 0;\n\
             \x20 while (!atomic_compare_exchange_weak_explicit(&m->locked, &expected, 1, memory_order_seq_cst, memory_order_seq_cst)) {\n\
             \x20   expected = 0;\n\
             \x20   spins++;\n\
             \x20   if (spins >= 32) { intent_thread_yield(); spins = 0; }\n\
             \x20 }\n\
             #endif\n\
             \x20 intent_guard_i64 g;\n\
             \x20 g.m = m;\n\
             \x20 return g;\n\
             }\n\
             static int64_t intent_guard_i64_get(const intent_guard_i64* g) INTENT_UNUSED;\n\
             static int64_t intent_guard_i64_get(const intent_guard_i64* g) {\n\
             \x20 return g->m->value;\n\
             }\n\
             static int64_t intent_guard_i64_set(const intent_guard_i64* g, int64_t v) INTENT_UNUSED;\n\
             static int64_t intent_guard_i64_set(const intent_guard_i64* g, int64_t v) {\n\
             \x20 g->m->value = v;\n\
             \x20 return v;\n\
             }\n\
             static void intent_guard_i64_unlock(intent_guard_i64* g) INTENT_UNUSED;\n\
             static void intent_guard_i64_unlock(intent_guard_i64* g) {\n\
             #if defined(__linux__) || defined(_WIN32)\n\
             \x20 /* If the previous state was 1 (no waiters), the fetch_sub\n\
             \x20    leaves state at 0 and there's nothing to wake.  If it was\n\
             \x20    2 (waiters), reset state to 0 and wake one. */\n\
             \x20 if (atomic_fetch_sub_explicit(&g->m->locked, 1, memory_order_seq_cst) != 1) {\n\
             \x20   atomic_store_explicit(&g->m->locked, 0, memory_order_seq_cst);\n\
             \x20   intent_mutex_futex_wake(&g->m->locked, 1);\n\
             \x20 }\n\
             #else\n\
             \x20 atomic_store_explicit(&g->m->locked, 0, memory_order_seq_cst);\n\
             #endif\n\
             }\n\n",
    );
}

fn collect_vec_elements(
    ty: &Type,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    match ty {
        Type::Vec(element) => {
            // Recurse FIRST so inner element types are
            // pushed before the outer. The emit loop relies
            // on this order: emitting `intent_vec_vec_int64_t`
            // needs `intent_vec_int64_t`'s typedef already in
            // scope. Refines #7's #7c.
            collect_vec_elements(element, seen, out);
            // Dedup key must distinguish nested element types
            // (was: `c_leaf_type` which collapses every
            // Vec-of-X to `"/* vec */"`).
            let key = element_tag(element);
            if seen.insert(key) {
                out.push((**element).clone());
            }
        }
        Type::Array { element, .. } => collect_vec_elements(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => collect_vec_elements(inner, seen, out),
        _ => {}
    }
}

fn collect_vec_elements_in_stmt(
    stmt: &TypedStmt,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    match stmt {
        TypedStmt::Let { ty, expr, .. } | TypedStmt::Reassign { ty, expr, .. } => {
            collect_vec_elements(ty, seen, out);
            collect_vec_elements_in_expr(expr, seen, out);
        }
        TypedStmt::Drop { ty, .. } => collect_vec_elements(ty, seen, out),
        TypedStmt::Discard { expr } => collect_vec_elements_in_expr(expr, seen, out),
        TypedStmt::Return { expr }
        | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => collect_vec_elements_in_expr(expr, seen, out),
        TypedStmt::Print { items } => {
            for it in items {
                if let crate::ir::TypedPrintItem::Expr(e) = it {
                    collect_vec_elements_in_expr(e, seen, out);
                }
            }
        }
        TypedStmt::If {
            cond,
            then_body,
            else_body,
        } => {
            collect_vec_elements_in_expr(cond, seen, out);
            for s in then_body {
                collect_vec_elements_in_stmt(s, seen, out);
            }
            for s in else_body {
                collect_vec_elements_in_stmt(s, seen, out);
            }
        }
        TypedStmt::While { cond, body } => {
            collect_vec_elements_in_expr(cond, seen, out);
            for s in body {
                collect_vec_elements_in_stmt(s, seen, out);
            }
        }
        TypedStmt::Break | TypedStmt::Continue => {}
        TypedStmt::IndexAssign { index, value, .. } => {
            collect_vec_elements_in_expr(index, seen, out);
            collect_vec_elements_in_expr(value, seen, out);
        }
        TypedStmt::FieldAssign { object, value, .. } => {
            collect_vec_elements_in_expr(object, seen, out);
            collect_vec_elements_in_expr(value, seen, out);
        }
        TypedStmt::For {
            start, end, body, ..
        } => {
            collect_vec_elements_in_expr(start, seen, out);
            collect_vec_elements_in_expr(end, seen, out);
            for s in body {
                collect_vec_elements_in_stmt(s, seen, out);
            }
        }
        TypedStmt::ForIter {
            element_ty,
            collection_ty,
            body,
            ..
        } => {
            collect_vec_elements(element_ty, seen, out);
            collect_vec_elements(collection_ty, seen, out);
            for s in body {
                collect_vec_elements_in_stmt(s, seen, out);
            }
        }
        TypedStmt::TaskSpawn { body, .. } => {
            for s in body {
                collect_vec_elements_in_stmt(s, seen, out);
            }
        }
        TypedStmt::TaskJoin { .. } => {}
    }
}

fn collect_vec_elements_in_expr(
    expr: &TypedExpr,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    collect_vec_elements(&expr.ty, seen, out);
    match &expr.kind {
        TypedExprKind::Unary { expr, .. } => collect_vec_elements_in_expr(expr, seen, out),
        TypedExprKind::Binary { left, right, .. } => {
            collect_vec_elements_in_expr(left, seen, out);
            collect_vec_elements_in_expr(right, seen, out);
        }
        TypedExprKind::Call { args, .. } | TypedExprKind::ArrayLit { elements: args } => {
            for arg in args {
                collect_vec_elements_in_expr(arg, seen, out);
            }
        }
        TypedExprKind::Cast { expr, .. } => collect_vec_elements_in_expr(expr, seen, out),
        TypedExprKind::Index { array, index, .. } => {
            collect_vec_elements_in_expr(array, seen, out);
            collect_vec_elements_in_expr(index, seen, out);
        }
        TypedExprKind::Len { array, .. } => collect_vec_elements_in_expr(array, seen, out),
        TypedExprKind::Tuple { elements } => {
            for e in elements {
                collect_vec_elements_in_expr(e, seen, out);
            }
        }
        TypedExprKind::StructLit { fields, .. } => {
            for (_, e) in fields {
                collect_vec_elements_in_expr(e, seen, out);
            }
        }
        TypedExprKind::FieldAccess { object, .. } => {
            collect_vec_elements_in_expr(object, seen, out);
        }
        TypedExprKind::TupleAccess { tuple, .. } => {
            collect_vec_elements_in_expr(tuple, seen, out);
        }
        TypedExprKind::EnumVariantWithPayload { payload, .. } => {
            collect_vec_elements_in_expr(payload, seen, out);
        }
        TypedExprKind::Match { scrutinee, arms } => {
            collect_vec_elements_in_expr(scrutinee, seen, out);
            for arm in arms {
                collect_vec_elements_in_expr(&arm.body, seen, out);
            }
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            collect_vec_elements_in_expr(cond, seen, out);
            collect_vec_elements_in_expr(then_value, seen, out);
            collect_vec_elements_in_expr(else_value, seen, out);
        }
        TypedExprKind::Block { stmts, tail } => {
            for s in stmts {
                collect_vec_elements_in_stmt(s, seen, out);
            }
            collect_vec_elements_in_expr(tail, seen, out);
        }
        _ => {}
    }
}

/// Walk every type position reachable from `ty` and record
/// distinct tuple shapes (keyed on `tuple_c_struct` name) into
/// `out`. Inner-first so a future nested-tuple shape appears
/// before any outer that references it. T1.1.
fn collect_tuple_shapes(
    ty: &Type,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Vec<Type>>,
) {
    match ty {
        Type::Tuple(elements) => {
            for e in elements {
                collect_tuple_shapes(e, seen, out);
            }
            let key = tuple_c_struct(elements);
            if seen.insert(key) {
                out.push(elements.clone());
            }
        }
        Type::Vec(inner)
        | Type::Ref(inner)
        | Type::RefMut(inner)
        | Type::Atomic(inner)
        | Type::Mutex(inner)
        | Type::Guard(inner) => collect_tuple_shapes(inner, seen, out),
        Type::Array { element, .. } => collect_tuple_shapes(element, seen, out),
        Type::Channel(element, _) => collect_tuple_shapes(element, seen, out),
        Type::FnPtr(params, ret) => {
            for p in params {
                collect_tuple_shapes(p, seen, out);
            }
            collect_tuple_shapes(ret, seen, out);
        }
        _ => {}
    }
}

fn collect_tuple_shapes_in_stmt(
    stmt: &TypedStmt,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Vec<Type>>,
) {
    match stmt {
        TypedStmt::Let { ty, expr, .. } | TypedStmt::Reassign { ty, expr, .. } => {
            collect_tuple_shapes(ty, seen, out);
            collect_tuple_shapes_in_expr(expr, seen, out);
        }
        TypedStmt::Drop { ty, .. } => collect_tuple_shapes(ty, seen, out),
        TypedStmt::Discard { expr } => collect_tuple_shapes_in_expr(expr, seen, out),
        TypedStmt::Return { expr }
        | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => collect_tuple_shapes_in_expr(expr, seen, out),
        TypedStmt::Print { items } => {
            for it in items {
                if let crate::ir::TypedPrintItem::Expr(e) = it {
                    collect_tuple_shapes_in_expr(e, seen, out);
                }
            }
        }
        TypedStmt::If { cond, then_body, else_body } => {
            collect_tuple_shapes_in_expr(cond, seen, out);
            for s in then_body {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
            for s in else_body {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
        }
        TypedStmt::While { cond, body } => {
            collect_tuple_shapes_in_expr(cond, seen, out);
            for s in body {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
        }
        TypedStmt::For { start, end, body, .. } => {
            collect_tuple_shapes_in_expr(start, seen, out);
            collect_tuple_shapes_in_expr(end, seen, out);
            for s in body {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
        }
        TypedStmt::ForIter { body, .. } => {
            for s in body {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
        }
        TypedStmt::IndexAssign { index, value, .. } => {
            collect_tuple_shapes_in_expr(index, seen, out);
            collect_tuple_shapes_in_expr(value, seen, out);
        }
        TypedStmt::FieldAssign { object, value, .. } => {
            collect_tuple_shapes_in_expr(object, seen, out);
            collect_tuple_shapes_in_expr(value, seen, out);
        }
        TypedStmt::TaskSpawn { body, .. } => {
            for s in body {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
        }
        _ => {}
    }
}

fn collect_tuple_shapes_in_expr(
    expr: &TypedExpr,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<Vec<Type>>,
) {
    collect_tuple_shapes(&expr.ty, seen, out);
    match &expr.kind {
        TypedExprKind::Tuple { elements } => {
            for e in elements {
                collect_tuple_shapes_in_expr(e, seen, out);
            }
        }
        TypedExprKind::TupleAccess { tuple, .. } => {
            collect_tuple_shapes_in_expr(tuple, seen, out);
        }
        TypedExprKind::Unary { expr, .. } => {
            collect_tuple_shapes_in_expr(expr, seen, out)
        }
        TypedExprKind::Binary { left, right, .. } => {
            collect_tuple_shapes_in_expr(left, seen, out);
            collect_tuple_shapes_in_expr(right, seen, out);
        }
        TypedExprKind::Call { args, .. } | TypedExprKind::ArrayLit { elements: args } => {
            for a in args {
                collect_tuple_shapes_in_expr(a, seen, out);
            }
        }
        TypedExprKind::Cast { expr, .. } => collect_tuple_shapes_in_expr(expr, seen, out),
        TypedExprKind::Index { array, index, .. } => {
            collect_tuple_shapes_in_expr(array, seen, out);
            collect_tuple_shapes_in_expr(index, seen, out);
        }
        TypedExprKind::Len { array, .. } => collect_tuple_shapes_in_expr(array, seen, out),
        TypedExprKind::CallIndirect { callee, args } => {
            collect_tuple_shapes_in_expr(callee, seen, out);
            for a in args {
                collect_tuple_shapes_in_expr(a, seen, out);
            }
        }
        // Closure #198: control-flow expressions can carry
        // tuple-typed sub-expressions (e.g. inner Lets of a
        // Block-expr, branch values of an IfExpr/Match). The
        // collector previously fell through `_ => {}` for
        // these shapes, so a tuple type that only appeared
        // inside a Block-expr inner Let never got its
        // `intent_tuple_<…>` typedef emitted and cc rejected
        // with `unknown type name intent_tuple_<…>`.
        TypedExprKind::Block { stmts, tail } => {
            for s in stmts {
                collect_tuple_shapes_in_stmt(s, seen, out);
            }
            collect_tuple_shapes_in_expr(tail, seen, out);
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            collect_tuple_shapes_in_expr(cond, seen, out);
            collect_tuple_shapes_in_expr(then_value, seen, out);
            collect_tuple_shapes_in_expr(else_value, seen, out);
        }
        TypedExprKind::Match { scrutinee, arms } => {
            collect_tuple_shapes_in_expr(scrutinee, seen, out);
            for arm in arms {
                collect_tuple_shapes_in_expr(&arm.body, seen, out);
            }
        }
        _ => {}
    }
}

/// Emit the C runtime helper `intent_str_concat` used by both
/// the tree-C backend and the SSA-C backend for Str/OwnedStr
/// `+` lowering. Allocates a fresh buffer, copies both
/// operands, NUL-terminates, and frees each operand whose
/// `_owned` flag is non-zero.
/// Emit the cross-platform `intent_thread_t` typedef plus
/// `intent_thread_create` / `intent_thread_join` /
/// `intent_thread_yield` wrappers. Dispatches on
/// `#if defined(_WIN32)` so the same C source links on
/// Linux/macOS (pthread) and Windows (CreateThread/
/// WaitForSingleObject/SwitchToThread). Shared between the
/// tree-C backend and the SSA-C backend (the SSA-C task
/// outlining references `intent_thread_create`/
/// `intent_thread_join`). Always emitted; small footprint.
pub(crate) fn emit_intent_thread_wrappers_c(out: &mut String) {
    out.push_str("#if defined(_WIN32)\n");
    out.push_str("# include <windows.h>\n");
    out.push_str("# include <synchapi.h>\n");
    out.push_str("typedef HANDLE intent_thread_t;\n");
    out.push_str("static int intent_thread_create(intent_thread_t* th, void* (*fn)(void*), void* arg) INTENT_UNUSED;\n");
    out.push_str("static int intent_thread_create(intent_thread_t* th, void* (*fn)(void*), void* arg) {\n");
    out.push_str("  *th = CreateThread(NULL, 0, (LPTHREAD_START_ROUTINE)fn, arg, 0, NULL);\n");
    out.push_str("  return *th == NULL ? -1 : 0;\n");
    out.push_str("}\n");
    out.push_str("static int intent_thread_join(intent_thread_t th) INTENT_UNUSED;\n");
    out.push_str("static int intent_thread_join(intent_thread_t th) {\n");
    out.push_str("  WaitForSingleObject(th, INFINITE);\n");
    out.push_str("  CloseHandle(th);\n");
    out.push_str("  return 0;\n");
    out.push_str("}\n");
    out.push_str("static void intent_thread_yield(void) INTENT_UNUSED;\n");
    out.push_str("static void intent_thread_yield(void) { SwitchToThread(); }\n");
    out.push_str("#else\n");
    out.push_str("# include <pthread.h>\n");
    out.push_str("# include <sched.h>\n");
    out.push_str("typedef pthread_t intent_thread_t;\n");
    out.push_str("static int intent_thread_create(intent_thread_t* th, void* (*fn)(void*), void* arg) INTENT_UNUSED;\n");
    out.push_str("static int intent_thread_create(intent_thread_t* th, void* (*fn)(void*), void* arg) {\n");
    out.push_str("  return pthread_create(th, NULL, fn, arg);\n");
    out.push_str("}\n");
    out.push_str("static int intent_thread_join(intent_thread_t th) INTENT_UNUSED;\n");
    out.push_str("static int intent_thread_join(intent_thread_t th) {\n");
    out.push_str("  return pthread_join(th, NULL);\n");
    out.push_str("}\n");
    out.push_str("static void intent_thread_yield(void) INTENT_UNUSED;\n");
    out.push_str("static void intent_thread_yield(void) { sched_yield(); }\n");
    out.push_str("#endif\n\n");
}

pub(crate) fn emit_intent_str_concat_c(out: &mut String) {
    out.push_str(
        "static char* intent_str_concat(const char* l, int l_owned, const char* r, int r_owned) INTENT_UNUSED;\n\
         static char* intent_str_concat(const char* l, int l_owned, const char* r, int r_owned) {\n\
         \x20 size_t ln = strlen(l), rn = strlen(r);\n\
         \x20 char* out = (char*)malloc(ln + rn + 1);\n\
         \x20 memcpy(out, l, ln);\n\
         \x20 memcpy(out + ln, r, rn);\n\
         \x20 out[ln + rn] = 0;\n\
         \x20 if (l_owned) free((void*)l);\n\
         \x20 if (r_owned) free((void*)r);\n\
         \x20 return out;\n\
         }\n\n",
    );
}

pub(crate) fn vec_c_struct(element: &Type) -> String {
    format!("intent_vec_{}", element_tag(element))
}

/// Build a C-identifier-safe tag for an element type. The tag
/// is used as the suffix on per-type helper names (e.g. `vec_int64_t`,
/// `vec_vec_int64_t`, `vec_arr4_int64_t`). Composable so that
/// nested aggregates (`Vec<Vec<i64>>`, `Vec<[i64; 4]>`) get
/// distinct, deterministic identifiers — refines #7 from
/// STATUS.md (was: returned `"/*_vec_*/"` for any `Vec<_>`
/// element, collapsing every nested Vec type to the same tag).
pub(crate) fn element_tag(element: &Type) -> String {
    match element {
        Type::Vec(inner) => format!("vec_{}", element_tag(inner)),
        Type::Array { element: inner, length } => {
            format!("arr{}_{}", length, element_tag(inner))
        }
        // Nominal types route through their per-name C
        // struct spelling so `Vec<Point>` becomes
        // `intent_vec_Struct_Point` rather than the
        // opaque `/*_struct_*/` placeholder. T1.2 +
        // Vec<Struct> support.
        Type::Struct(name) => struct_c_name(name),
        // Payloaded enums lower to the `Enum_<Name>`
        // tagged-union struct (see ENUM_PAYLOAD_REGISTRY).
        // For `Vec<Msg>` the per-shape Vec typedef must
        // reference that struct, not the int32_t tag
        // — closure #151 (was emitting `intent_vec_int32_t`
        // for any enum element and then trying to store
        // `Enum_<Name>` struct literals into int32_t
        // slots, failing at cc). Tag-only enums keep the
        // int32_t spelling via the fallback below since
        // they don't appear in ENUM_PAYLOAD_REGISTRY.
        Type::Enum(name)
            if ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name)) =>
        {
            enum_c_name(name)
        }
        Type::Tuple(elements) => tuple_c_struct(elements),
        // `Str` / `OwnedStr` both spell as a `char*` /
        // `const char*` in C — neither is a valid C
        // identifier suffix. Spell them as `str` /
        // `owned_str` so `Vec<OwnedStr>` becomes
        // `intent_vec_owned_str` and friends. Closure
        // #136 (was emitting `intent_vec_char*` and
        // failing to compile).
        Type::Str => "str".to_string(),
        Type::OwnedStr => "owned_str".to_string(),
        // Closure #211: `Atomic<T>` / `Channel<T, N>` must
        // include the element type (and capacity) in the
        // element-tag so per-shape Vec typedefs stay
        // distinct. The c_leaf_type fallback returned the
        // hardcoded `_Atomic int64_t` / `intent_channel_int64_t_16`
        // strings for any Atomic/Channel, collapsing every
        // `Vec<Atomic<T>>` (or `Vec<Channel<T,N>>`) to the
        // same typedef name. With two distinct (T, …) shapes
        // in the same program, the second `vec()` call used
        // the first's typedef which had the wrong element
        // type — ASan-detected stack-buffer-overflow on
        // memcpy when widths differed (u32 vs u8).
        Type::Atomic(element) => format!("atomic_{}", element_tag(element)),
        Type::Channel(element, capacity) => {
            format!("channel_{}_{}", element_tag(element), capacity)
        }
        // Closure #214: `fn(T1, T2) -> R` falls through to
        // `c_leaf_type(FnPtr) = "void*"`, and the `*` in the
        // typedef name (`intent_vec_void*`) breaks C parsing.
        // Spell it as `fnptr` — distinct from any scalar
        // type, identifier-safe. All fn-ptrs share the same
        // C representation (`void*` cast in/out for indirect
        // calls), so a single per-element-tag typedef is
        // correct regardless of parameter/return types.
        Type::FnPtr(_, _) => "fnptr".to_string(),
        _ => c_leaf_type(element).replace(' ', "_"),
    }
}

pub(crate) fn vec_helper(element: &Type, op: &str) -> String {
    format!("{}__{}", vec_c_struct(element), op)
}

/// Storage struct name for `Channel<T, N>` in the C backend.
/// Combines the element's C spelling (sanitized) with the
/// capacity so each (T, N) used in the program gets its own
/// struct + runtime helpers. e.g. `Channel<i32, 32>` →
/// `intent_channel_int32_t_32`.
pub(crate) fn c_channel_storage(element: &Type, capacity: u64) -> String {
    format!("intent_channel_{}_{}", element_tag(element), capacity)
}

/// Per-(T, N) channel helper name: e.g. `_send` / `_recv` /
/// `_new`.
pub(crate) fn c_channel_helper(element: &Type, capacity: u64, op: &str) -> String {
    format!("{}_{}", c_channel_storage(element, capacity), op)
}

/// Recover the `(T, N)` shape from a `&Channel<T, N>` /
/// `&mut Channel<T, N>` operand type. Shared with SSA-C.
pub(crate) fn channel_inner_from_ref_pub(ty: &Type) -> (Type, u64) {
    channel_inner_from_ref(ty)
}

/// Emit one per-(T, N) channel bundle (struct + helpers).
/// Shared with SSA-C.
pub(crate) fn emit_channel_bundle_pub(
    element: &Type,
    capacity: u64,
    out: &mut String,
) {
    emit_channel_bundle(element, capacity, out)
}

/// Collect every unique `(T, N)` `Channel` spec reachable
/// from `ty`. `seen` dedups by the channel's struct name so
/// nested types (`Vec<Channel<i64, 8>>`, `Ref<Channel<…>>`)
/// don't emit the same bundle twice. Used during preamble
/// emission to generate exactly the per-(T, N) runtime
/// helpers the program references.
pub(crate) fn collect_channel_specs(
    ty: &Type,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<(Type, u64)>,
) {
    match ty {
        Type::Channel(element, capacity) => {
            let key = c_channel_storage(element, *capacity);
            if seen.insert(key) {
                out.push(((**element).clone(), *capacity));
            }
            collect_channel_specs(element, seen, out);
        }
        Type::Vec(element) | Type::Atomic(element) | Type::Mutex(element) | Type::Guard(element) => {
            collect_channel_specs(element, seen, out);
        }
        Type::Array { element, .. } => collect_channel_specs(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => collect_channel_specs(inner, seen, out),
        _ => {}
    }
}

pub(crate) fn collect_channel_specs_in_stmt(
    stmt: &TypedStmt,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<(Type, u64)>,
) {
    match stmt {
        TypedStmt::Let { ty, expr, .. } | TypedStmt::Reassign { ty, expr, .. } => {
            collect_channel_specs(ty, seen, out);
            collect_channel_specs_in_expr(expr, seen, out);
        }
        TypedStmt::Drop { ty, .. } => collect_channel_specs(ty, seen, out),
        TypedStmt::Discard { expr } => collect_channel_specs_in_expr(expr, seen, out),
        TypedStmt::Return { expr }
        | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => collect_channel_specs_in_expr(expr, seen, out),
        TypedStmt::Print { items } => {
            for it in items {
                if let crate::ir::TypedPrintItem::Expr(e) = it {
                    collect_channel_specs_in_expr(e, seen, out);
                }
            }
        }
        TypedStmt::If { cond, then_body, else_body } => {
            collect_channel_specs_in_expr(cond, seen, out);
            for s in then_body {
                collect_channel_specs_in_stmt(s, seen, out);
            }
            for s in else_body {
                collect_channel_specs_in_stmt(s, seen, out);
            }
        }
        TypedStmt::While { cond, body } => {
            collect_channel_specs_in_expr(cond, seen, out);
            for s in body {
                collect_channel_specs_in_stmt(s, seen, out);
            }
        }
        TypedStmt::Break | TypedStmt::Continue => {}
        TypedStmt::IndexAssign { index, value, base_ty, .. } => {
            collect_channel_specs(base_ty, seen, out);
            collect_channel_specs_in_expr(index, seen, out);
            collect_channel_specs_in_expr(value, seen, out);
        }
        TypedStmt::FieldAssign { object, value, .. } => {
            collect_channel_specs_in_expr(object, seen, out);
            collect_channel_specs_in_expr(value, seen, out);
        }
        TypedStmt::For { start, end, body, .. } => {
            collect_channel_specs_in_expr(start, seen, out);
            collect_channel_specs_in_expr(end, seen, out);
            for s in body {
                collect_channel_specs_in_stmt(s, seen, out);
            }
        }
        TypedStmt::ForIter { element_ty, collection_ty, body, .. } => {
            collect_channel_specs(element_ty, seen, out);
            collect_channel_specs(collection_ty, seen, out);
            for s in body {
                collect_channel_specs_in_stmt(s, seen, out);
            }
        }
        TypedStmt::TaskSpawn { body, .. } => {
            for s in body {
                collect_channel_specs_in_stmt(s, seen, out);
            }
        }
        TypedStmt::TaskJoin { .. } => {}
    }
}

pub(crate) fn collect_channel_specs_in_expr(
    expr: &TypedExpr,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<(Type, u64)>,
) {
    collect_channel_specs(&expr.ty, seen, out);
    match &expr.kind {
        TypedExprKind::Unary { expr, .. } => collect_channel_specs_in_expr(expr, seen, out),
        TypedExprKind::Binary { left, right, .. } => {
            collect_channel_specs_in_expr(left, seen, out);
            collect_channel_specs_in_expr(right, seen, out);
        }
        TypedExprKind::Call { args, .. } | TypedExprKind::ArrayLit { elements: args } => {
            for arg in args {
                collect_channel_specs_in_expr(arg, seen, out);
            }
        }
        TypedExprKind::Cast { expr, .. } => collect_channel_specs_in_expr(expr, seen, out),
        TypedExprKind::Index { array, index, .. } => {
            collect_channel_specs_in_expr(array, seen, out);
            collect_channel_specs_in_expr(index, seen, out);
        }
        TypedExprKind::Len { array, .. } => collect_channel_specs_in_expr(array, seen, out),
        _ => {}
    }
}

pub(crate) fn emit_vec_bundle(element: &Type, out: &mut String) {
    let struct_name = vec_c_struct(element);
    // Element's full C type spelling. For primitive scalars
    // this is `c_leaf_type` (e.g. `int64_t`). For aggregates
    // (`Vec<T>`, `Array<T, N>`) we route through `c_type_name`
    // / `c_array_type_name` so a `Vec<Vec<i64>>` element spells
    // as `intent_vec_int64_t` (the inner struct typedef
    // emitted earlier in the bundle list). Refines #7 — was
    // emitting `"/* vec */"` for any Vec-element, which the C
    // compiler then choked on.
    let c_element = c_element_storage(element);
    let element_is_copy = element.is_copy();
    // Fixed-size array elements need memcpy-based slot
    // writes (C forbids `arr1 = arr2` via `=`). Phase 2c.
    let element_is_array = matches!(element, Type::Array { .. });

    out.push_str(&format!(
        "typedef struct {{ {ct}* data; uint64_t len; uint64_t capacity; }} {sn};\n",
        ct = c_element,
        sn = struct_name
    ));

    out.push_str(&format!(
        "static INTENT_UNUSED {sn} {sn}__from(uint64_t n, const {ct}* init) {{\
\n    {sn} v;\
\n    v.data = ({ct}*)malloc((n == 0 ? 1 : n) * sizeof({ct}));\
\n    if (!v.data) abort();\
\n    if (n > 0) memcpy(v.data, init, n * sizeof({ct}));\
\n    v.len = n;\
\n    v.capacity = n == 0 ? 1 : n;\
\n    return v;\
\n}}\n",
        sn = struct_name,
        ct = c_element
    ));

    // Array elements need memcpy; struct/scalar elements
    // assign directly. Phase 2c (#7).
    let push_store = if element_is_array {
        format!(
            "    memcpy(xs.data[xs.len], v, sizeof({}));\
\n    xs.len++;",
            c_element,
        )
    } else {
        "    xs.data[xs.len++] = v;".to_string()
    };
    out.push_str(&format!(
        "static INTENT_UNUSED {sn} {sn}__push({sn} xs, {ct} v) {{\
\n    if (xs.len >= xs.capacity) {{\
\n        xs.capacity = xs.capacity ? xs.capacity * 2 : 1;\
\n        xs.data = ({ct}*)realloc(xs.data, xs.capacity * sizeof({ct}));\
\n        if (!xs.data) abort();\
\n    }}\
\n{store}\
\n    return xs;\
\n}}\n",
        sn = struct_name,
        ct = c_element,
        store = push_store,
    ));

    // In-place push for `push(mut ref xs, v)` — operates on a
    // pointer to the Vec struct. Used when the Vec is owned by
    // another binding (e.g. a struct field) and the caller
    // doesn't want to consume + reassign. T1.2 phase 2b
    // follow-up.
    let push_mut_store = if element_is_array {
        format!(
            "    memcpy(xs->data[xs->len], v, sizeof({}));\n    xs->len++;",
            c_element,
        )
    } else {
        "    xs->data[xs->len++] = v;".to_string()
    };
    out.push_str(&format!(
        "static INTENT_UNUSED int64_t {sn}__push_mut({sn}* xs, {ct} v) {{\
\n    if (xs->len >= xs->capacity) {{\
\n        xs->capacity = xs->capacity ? xs->capacity * 2 : 1;\
\n        xs->data = ({ct}*)realloc(xs->data, xs->capacity * sizeof({ct}));\
\n        if (!xs->data) abort();\
\n    }}\
\n{store}\
\n    return (int64_t)xs->len;\
\n}}\n",
        sn = struct_name,
        ct = c_element,
        store = push_mut_store,
    ));

    // Closure #219: in-place `pop(mut ref xs) -> T` — abort on
    // empty, otherwise decrement len and return the last
    // element by-move. For non-Copy element types (OwnedStr,
    // Vec<U>, Struct with owning fields), the returned value
    // carries ownership of the slot's heap; the Vec's
    // scope-exit `__free` walks elements based on the post-
    // pop len so the moved-out slot won't be re-freed.
    // For fixed-size array element types (`[T; N]`), C
    // forbids returning a bare array by value — the helper
    // returns a struct wrapping the array would complicate
    // codegen significantly. Defer Vec<[T;N]> pop to a
    // follow-up; for now reject via the checker if it ever
    // surfaces. Most callers don't need that shape.
    if !element_is_array {
        out.push_str(&format!(
            "static INTENT_UNUSED {ct} {sn}__pop_mut({sn}* xs) {{\
\n    if (xs->len == 0) {{\
\n        fprintf(stderr, \"pop on empty Vec\\n\");\
\n        abort();\
\n    }}\
\n    xs->len--;\
\n    return xs->data[xs->len];\
\n}}\n",
            sn = struct_name,
            ct = c_element,
        ));
    }

    // `__set(xs, i, v)`: store the new value at xs.data[i].
    // For non-Copy elements (Vec<T>, Array<T, N>) the old slot
    // value's resources are released first via the element-
    // specific cleanup (recursive free for `Vec<T>`, no-op for
    // arrays since their backing storage is inline in the
    // outer buffer). Without the cleanup an overwrite would
    // leak the prior inner-Vec's heap buffer.
    let set_cleanup = if element_is_copy {
        String::new()
    } else {
        c_element_drop_old("xs.data[i]", element)
    };
    let set_store = if element_is_array {
        format!(
            "    memcpy(xs.data[i], v, sizeof({}));",
            c_element,
        )
    } else {
        "    xs.data[i] = v;".to_string()
    };
    out.push_str(&format!(
        "static INTENT_UNUSED {sn} {sn}__set({sn} xs, uint64_t i, {ct} v) {{\
\n    assert(i < xs.len);\
{cleanup}\
\n{store}\
\n    return xs;\
\n}}\n",
        sn = struct_name,
        ct = c_element,
        cleanup = set_cleanup,
        store = set_store,
    ));

    // `__clone(xs)`: malloc a new buffer + copy each element.
    // For Copy elements a single memcpy suffices. For non-Copy
    // elements (`Vec<T>`) each slot needs the element's own
    // deep-clone helper so the duplicated buffer doesn't alias
    // the source's inner storage (which would cause double-
    // frees when both Vecs are dropped). Arrays-of-Copy slots
    // are themselves Copy (memcpy is fine).
    let clone_body = if element_is_copy {
        format!(
            "    if (xs.len > 0) memcpy(c.data, xs.data, xs.len * sizeof({ct}));",
            ct = c_element,
        )
    } else if element_is_array {
        // Arrays-of-Copy slots are themselves Copy bytes —
        // memcpy the whole buffer (matches Copy element
        // path). Phase 2c.
        format!(
            "    if (xs.len > 0) memcpy(c.data, xs.data, xs.len * sizeof({ct}));",
            ct = c_element,
        )
    } else {
        format!(
            "    for (uint64_t k = 0; k < xs.len; ++k) {{\
\n        c.data[k] = {dup};\
\n    }}",
            dup = c_element_deep_clone("xs.data[k]", element),
        )
    };
    out.push_str(&format!(
        "static INTENT_UNUSED {sn} {sn}__clone({sn} xs) {{\
\n    {sn} c;\
\n    c.data = ({ct}*)malloc((xs.len == 0 ? 1 : xs.len) * sizeof({ct}));\
\n    if (!c.data) abort();\
\n{body}\
\n    c.len = xs.len;\
\n    c.capacity = xs.len == 0 ? 1 : xs.len;\
\n    return c;\
\n}}\n",
        sn = struct_name,
        ct = c_element,
        body = clone_body,
    ));

    // `__free(xs)`: for Copy elements just free the heap
    // buffer. For non-Copy element types we first walk every
    // live slot and free each element's inner resources (the
    // element's own drop), then free the outer buffer.
    if element_is_copy {
        out.push_str(&format!(
            "static INTENT_UNUSED void {sn}__free({sn} xs) {{ free(xs.data); }}\n\n",
            sn = struct_name
        ));
    } else {
        let inner_drop = c_element_drop_old("xs.data[k]", element);
        out.push_str(&format!(
            "static INTENT_UNUSED void {sn}__free({sn} xs) {{\
\n    for (uint64_t k = 0; k < xs.len; ++k) {{\
{inner}\
\n    }}\
\n    free(xs.data);\
\n}}\n\n",
            sn = struct_name,
            inner = inner_drop,
        ));
    }
}

/// Storage-type C spelling for a value of type `ty`. The
/// difference between this and `c_leaf_type` is aggregate
/// handling: for `Vec<U>` we want the struct typedef
/// (`intent_vec_<U>`), not the placeholder `"/* vec */"`; for
/// `[T; N]` we want the per-shape typedef alias. New for #7;
/// used inside vec bundle bodies where the element type may
/// itself be a Vec (so we'd otherwise emit invalid C).
pub(crate) fn c_element_storage(ty: &Type) -> String {
    match ty {
        Type::Vec(inner) => vec_c_struct(inner),
        Type::Array { .. } => array_c_typedef(ty),
        Type::Tuple(elements) => tuple_c_struct(elements),
        Type::Struct(name) => struct_c_name(name),
        // Payloaded enums spell as `Enum_<Name>`; tag-only
        // enums keep `int32_t` via the c_leaf_type fallback.
        // Closure #151 (parallel to the element_tag fix).
        Type::Enum(name)
            if ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name)) =>
        {
            enum_c_name(name)
        }
        // Closure #208: `Channel<T, N>` is parametric over
        // both element width and capacity. The c_leaf_type
        // fallback returns the hardcoded
        // `intent_channel_int64_t_16` (the comment there
        // explicitly notes callers must special-case
        // Channel). Without this arm, a `Channel<i64, 4>`
        // struct field declared as `intent_channel_int64_t_16`
        // doesn't match the constructor's
        // `intent_channel_int64_t_4_new()` return type and cc
        // rejects with "incompatible types".
        Type::Channel(element, capacity) => c_channel_storage(element, *capacity),
        // Closure #209: same shape for `Atomic<T>`. The
        // c_leaf_type fallback returns `_Atomic int64_t`
        // for any Atomic; an `Atomic<u32>` struct field
        // declared at i64 width would silently use the wrong
        // memory size on platforms where i64 and u32 atomics
        // have different alignment / lock-free behavior.
        // `c_atomic_storage(element)` returns
        // `_Atomic <c_leaf_type(element)>` — the right
        // per-element width.
        Type::Atomic(element) => c_atomic_storage(element),
        // Vtables Phase 4: `dyn Iface` storage spells as
        // `intent_dyn_<Iface>` (the per-Iface fat-pointer
        // typedef emitted in the preamble). Without this arm
        // a struct field / Vec element of `dyn Iface` falls
        // through to the placeholder `intent_dyn` from
        // `c_leaf_type` and cc rejects with "unknown type".
        Type::Object(iface) => format!("intent_dyn_{}", iface),
        _ => c_leaf_type(ty).to_string(),
    }
}

/// C-side typedef name for `[T; N]` used inside helper
/// signatures. Built per-shape so a `Vec<[i64; 4]>` element
/// spells as `intent_arr4_int64_t` — distinct from any
/// scalar/vec spelling. The typedef itself is emitted upstream
/// in `emit_array_typedefs_for`.
pub(crate) fn array_c_typedef(ty: &Type) -> String {
    let Type::Array { element, length } = ty else {
        unreachable!("array_c_typedef called on non-array");
    };
    format!("intent_arr{}_{}", length, element_tag(element))
}

/// Closure #239: per-shape struct wrapping `[T; N]` for use
/// in return position. C arrays can't be values in return
/// position; the struct gets passed by value and the caller
/// memcpys `.data` into a local array.
pub(crate) fn array_return_struct_name(element: &Type, length: u64) -> String {
    format!("intent_arr_ret_{}_{}", length, element_tag(element))
}

/// Walk a Vec-element type and emit a `typedef` for every
/// `Array<T, N>` shape that appears, deduplicated against
/// `seen` (keyed on the typedef name). Recurses through
/// nested aggregates so a `Vec<[[i64; 2]; 3]>` would emit
/// both the inner and outer array typedefs. New for #7 phase
/// 2c.
pub(crate) fn emit_array_typedefs_for(
    ty: &Type,
    seen: &mut BTreeSet<String>,
    out: &mut String,
) {
    match ty {
        Type::Array { element, length } => {
            // Recurse first so nested array shapes are
            // declared before the outer typedef references
            // them (mirrors the inner-first Vec bundle
            // order).
            emit_array_typedefs_for(element, seen, out);
            let name = array_c_typedef(ty);
            if seen.insert(name.clone()) {
                let inner_spelling = match element.as_ref() {
                    Type::Array { .. } => array_c_typedef(element),
                    Type::Vec(_) => vec_c_struct(element),
                    _ => c_leaf_type(element).to_string(),
                };
                out.push_str(&format!(
                    "typedef {} {}[{}];\n",
                    inner_spelling, name, length,
                ));
            }
        }
        Type::Vec(inner) | Type::Ref(inner) | Type::RefMut(inner) => {
            emit_array_typedefs_for(inner, seen, out);
        }
        _ => {}
    }
}

/// Drop-old-slot expression: a C statement (or empty) that
/// releases the resources owned by `slot`, whose value-type
/// is `ty`. For `Vec<U>` we recurse through the inner Vec's
/// `__free` helper. Arrays of Copy contain no heap so they
/// need nothing. Used by `__set` and `__free` to keep the
/// cleanup shape in one place.
pub(crate) fn c_element_drop_old(slot: &str, ty: &Type) -> String {
    match ty {
        Type::Vec(inner) => format!(
            "\n        {helper}({slot});",
            helper = vec_helper(inner, "free"),
            slot = slot,
        ),
        Type::OwnedStr => format!("\n        free((void*){slot});", slot = slot),
        Type::Struct(name) => {
            // Drop each owning field of the struct slot via the
            // shared `emit_struct_field_drops` helper. If the
            // struct has no owning fields (or isn't registered),
            // emit nothing — matches the previous behavior.
            // Closure #127.
            let fields = STRUCT_FIELDS_REGISTRY
                .with(|r| r.borrow().get(name).cloned())
                .unwrap_or_default();
            if fields.is_empty() {
                return String::new();
            }
            let mut body = String::new();
            let empty: std::collections::HashSet<&String> =
                std::collections::HashSet::new();
            emit_struct_field_drops(slot, name, &fields, &empty, &mut body);
            if body.is_empty() {
                return String::new();
            }
            // `emit_struct_field_drops` emits each line with a
            // leading two-space indent. The Vec __free body
            // expects each statement to be indented by 8 spaces
            // (inside a 4-space-indented `for` block in a 4-space
            // indented helper). Re-indent and prepend a leading
            // newline so we slot cleanly in.
            let mut reindented = String::new();
            for line in body.lines() {
                let trimmed = line.trim_start();
                if trimmed.is_empty() {
                    continue;
                }
                reindented.push_str("\n        ");
                reindented.push_str(trimmed);
            }
            reindented
        }
        Type::Enum(name) => {
            // Drop the enum's payload when the active tag is
            // one of the payloaded variants. Mirrors the
            // scope-exit Drop logic for enums. Closure #151
            // (`Vec<PayloadedEnum>` was leaking each
            // element's payload at outer __free time).
            let payload_ty = ENUM_PAYLOAD_REGISTRY
                .with(|r| r.borrow().get(name).cloned());
            let free_expr: Option<String> = match &payload_ty {
                Some(Type::OwnedStr) => {
                    Some(format!("free((void*){slot}.payload);", slot = slot))
                }
                Some(Type::Vec(element)) => Some(format!(
                    "{helper}({slot}.payload);",
                    helper = vec_helper(element, "free"),
                    slot = slot
                )),
                _ => None,
            };
            let Some(free_call) = free_expr else {
                return String::new();
            };
            let payload_tags: Vec<u32> = ENUM_PAYLOAD_TAGS_REGISTRY
                .with(|r| r.borrow().get(name).cloned().unwrap_or_default());
            if payload_tags.is_empty() {
                return String::new();
            }
            let cases: Vec<String> = payload_tags
                .iter()
                .map(|t| format!("case {}", t))
                .collect();
            format!(
                "\n        switch ({slot}.tag) {{ {}: {} break; default: break; }}",
                cases.join(": "),
                free_call,
                slot = slot
            )
        }
        _ => String::new(),
    }
}

/// Deep-clone expression for a value of type `ty`. For Copy
/// values the original is returned (memcpy semantics are
/// correct). For `Vec<U>` we route through the inner Vec's
/// `__clone`. New for #7.
pub(crate) fn c_element_deep_clone(slot: &str, ty: &Type) -> String {
    match ty {
        Type::Vec(inner) => format!(
            "{helper}({slot})",
            helper = vec_helper(inner, "clone"),
            slot = slot,
        ),
        // Closure #152: `clone(Vec<OwnedStr>)` /
        // `clone(Vec<Enum_with_OwnedStr>)` was shallow-
        // copying the heap pointer, then both source and
        // clone double-freed at scope exit.
        //
        // OwnedStr: round-trip through `intent_str_concat`
        // with an empty literal — the helper mallocs a
        // fresh buffer of the source's length and memcpy's
        // the bytes, giving us a strdup-like deep copy.
        Type::OwnedStr => format!(
            "intent_str_concat({slot}, 0, \"\", 0)",
            slot = slot
        ),
        // Closure #153: `Vec<Struct{heap-field}>` clone was
        // shallow-copying the struct, so every heap-shaped
        // field pointer was shared between source and clone
        // and double-freed at scope exit. Reconstruct the
        // struct with each owning field deep-cloned
        // (recursive call) and Copy fields copied as-is.
        Type::Struct(name) => {
            let fields = STRUCT_FIELDS_REGISTRY
                .with(|r| r.borrow().get(name).cloned())
                .unwrap_or_default();
            let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
            if !has_owning {
                return slot.to_string();
            }
            let mut parts: Vec<String> = Vec::with_capacity(fields.len());
            for (fname, fty) in &fields {
                let field_slot = format!("({}).{}", slot, fname);
                let field_clone = c_element_deep_clone(&field_slot, fty);
                parts.push(format!(".{} = {}", fname, field_clone));
            }
            return format!(
                "(({}){{ {} }})",
                struct_c_name(name),
                parts.join(", ")
            );
        }
        // Enum with OwnedStr payload: tag-switched ternary
        // — for payloaded tags, reconstruct the enum
        // struct with a deep-cloned payload; otherwise
        // keep the struct as-is.
        Type::Enum(name) => {
            let payload_ty = ENUM_PAYLOAD_REGISTRY
                .with(|r| r.borrow().get(name).cloned());
            let payload_tags: Vec<u32> = ENUM_PAYLOAD_TAGS_REGISTRY
                .with(|r| r.borrow().get(name).cloned().unwrap_or_default());
            match (&payload_ty, payload_tags.is_empty()) {
                (Some(Type::OwnedStr), false) => {
                    let mut cond = String::from("0");
                    for t in &payload_tags {
                        cond = format!("({} || {}.tag == {})", cond, slot, t);
                    }
                    format!(
                        "(({}) ? (({}){{ .tag = ({}).tag, .payload = intent_str_concat(({}).payload, 0, \"\", 0) }}) : ({}))",
                        cond,
                        enum_c_name(name),
                        slot,
                        slot,
                        slot
                    )
                }
                _ => slot.to_string(),
            }
        }
        _ => slot.to_string(),
    }
}

fn emit_prototype(function: &TypedFunction, out: &mut String) {
    if function.is_extern {
        out.push_str("extern ");
        out.push_str(&c_type_name(&function.return_type));
        out.push(' ');
        out.push_str(&function.name);
        out.push('(');
        emit_params(function, out);
        out.push_str(");\n");
        return;
    }
    out.push_str("static ");
    out.push_str(&c_type_name(&function.return_type));
    out.push(' ');
    out.push_str(&function_name(&function.name));
    out.push('(');
    emit_params(function, out);
    out.push_str(");\n");
}

fn emit_function(function: &TypedFunction, out: &mut String) {
    // Closure #269: `extern "C" fn name(...) -> R;` emits a
    // forward declaration of the bare C symbol (no `fn_`
    // prefix, no `static` storage class) and returns. The
    // linker provides the body.
    if function.is_extern {
        out.push_str("extern ");
        out.push_str(&c_type_name(&function.return_type));
        out.push(' ');
        out.push_str(&function.name);
        out.push('(');
        emit_params(function, out);
        out.push_str(");\n");
        return;
    }
    // Closure #286: `#[bounded(N)]` attribute emits a
    // thread-local depth counter + bound check at fn entry.
    // GCC's __attribute__((cleanup)) ensures the decrement
    // runs on every exit path (including early returns).
    // Same shape works on clang.
    if let Some(bound) = function.recursion_bound {
        let counter_name = format!("__intent_depth_{}", function.name);
        let dec_helper = format!("__intent_dec_depth_{}", function.name);
        out.push_str(&format!(
            "static __thread int {} = 0;\n", counter_name
        ));
        out.push_str(&format!(
            "static void {}(int* __u) {{ (void)__u; --{}; }}\n",
            dec_helper, counter_name
        ));
        out.push_str("static ");
        out.push_str(&c_type_name(&function.return_type));
        out.push(' ');
        out.push_str(&function_name(&function.name));
        out.push('(');
        emit_params(function, out);
        out.push_str(") {\n");
        out.push_str(&format!(
            "  int __depth_guard __attribute__((cleanup({}))) = 0;\n  (void)__depth_guard;\n",
            dec_helper
        ));
        out.push_str(&format!(
            "  if (++{} > {}) {{ \
              fprintf(stderr, \"recursion bound exceeded in '{}' (#[bounded({})]); aborting\\n\"); \
              abort(); \
            }}\n",
            counter_name, bound, function.name, bound
        ));
        for requirement in &function.requires {
            out.push_str("  assert(");
            out.push_str(&emit_expr(requirement));
            out.push_str(");\n");
        }
        for stmt in &function.body {
            emit_stmt(stmt, out);
        }
        out.push_str("}\n");
        return;
    }
    out.push_str("static ");
    out.push_str(&c_type_name(&function.return_type));
    out.push(' ');
    out.push_str(&function_name(&function.name));
    out.push('(');
    emit_params(function, out);
    out.push_str(") {\n");

    for requirement in &function.requires {
        out.push_str("  assert(");
        out.push_str(&emit_expr(requirement));
        out.push_str(");\n");
    }

    for stmt in &function.body {
        emit_stmt(stmt, out);
    }

    out.push_str("}\n");
}

fn emit_params(function: &TypedFunction, out: &mut String) {
    if function.params.is_empty() {
        out.push_str("void");
        return;
    }

    for (index, param) in function.params.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format_declarator(&param.ty, &local_name(&param.name)));
    }
}

fn emit_stmt(stmt: &TypedStmt, out: &mut String) {
    match stmt {
        TypedStmt::Let { name, ty, expr } => {
            // Closure #276: when the Let RHS is the
            // synthetic-prelude Block emitted by
            // `make_dyn_coerce` (one or more synthetic-let
            // stmts followed by a DynCoerce tail), emit the
            // prelude stmts at the OUTER level so the temps'
            // storage lives for the enclosing block — not just
            // the GCC stmt-expr. Without this, `&__dyn_src`
            // would dangle by the time the fat pointer's data
            // slot is read. Other Block-RHS shapes still go
            // through the regular stmt-expr path so existing
            // closures like #200 don't regress.
            if let TypedExprKind::Block { stmts, tail } = &expr.kind {
                if matches!(tail.kind, TypedExprKind::DynCoerce { .. }) {
                    for s in stmts {
                        emit_stmt(s, out);
                    }
                    emit_stmt(
                        &TypedStmt::Let {
                            name: name.clone(),
                            ty: ty.clone(),
                            expr: (**tail).clone(),
                        },
                        out,
                    );
                    return;
                }
            }
            out.push_str("  ");
            if let Type::Array { element, length } = ty {
                if let TypedExprKind::ArrayLit { elements } = &expr.kind {
                    let element_strs: Vec<String> = elements.iter().map(emit_expr).collect();
                    // Use the per-shape storage spelling for
                    // aggregate elements (`Struct_Point`,
                    // `intent_tuple_…`) so `[Point; 3]` arrays
                    // emit valid C declarations rather than
                    // the `/* struct */` placeholder.
                    out.push_str(&c_element_storage(element));
                    out.push(' ');
                    out.push_str(&local_name(name));
                    out.push('[');
                    out.push_str(&length.to_string());
                    out.push_str("] = { ");
                    out.push_str(&element_strs.join(", "));
                    out.push_str(" };\n");
                } else {
                    // Closure #239: if the RHS is a Call /
                    // Block / other shape whose value-type is
                    // Array, it returns the struct wrapper
                    // (`intent_arr_ret_<N>_<T>`). Spill into a
                    // struct temp first, then memcpy `.data`
                    // into the local array. Plain Var / FieldAccess
                    // / Index sources that emit as an lvalue
                    // (decaying naturally to the element-type
                    // pointer) still work with the original
                    // memcpy-from-array form.
                    let needs_struct_unwrap = matches!(
                        &expr.kind,
                        TypedExprKind::Call { .. }
                            | TypedExprKind::Block { .. }
                            | TypedExprKind::IfExpr { .. }
                            | TypedExprKind::Match { .. }
                    );
                    out.push_str(&c_element_storage(element));
                    out.push(' ');
                    out.push_str(&local_name(name));
                    out.push('[');
                    out.push_str(&length.to_string());
                    out.push_str("];\n");
                    if needs_struct_unwrap {
                        let wrapper = array_return_struct_name(element, *length);
                        out.push_str(&format!(
                            "  {} _intent_ret_{} = {};\n  memcpy({}, _intent_ret_{}.data, sizeof({}));\n",
                            wrapper,
                            name,
                            emit_expr(expr),
                            local_name(name),
                            name,
                            local_name(name),
                        ));
                    } else {
                        out.push_str("  memcpy(");
                        out.push_str(&local_name(name));
                        out.push_str(", ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(", sizeof(");
                        out.push_str(&local_name(name));
                        out.push_str("));\n");
                    }
                }
            } else if matches!(ty, Type::FnPtr(_, _)) {
                // C function-pointer declarators have to wrap
                // the binding name inside `(*name)` so the
                // tokens parse — `int64_t (*v)(int64_t) =
                // expr;`. Reuse format_declarator which knows
                // the syntax.
                out.push_str(&format_declarator(ty, &local_name(name)));
                out.push_str(" = ");
                out.push_str(&emit_expr(expr));
                out.push_str(";\n");
            } else {
                out.push_str(&c_type_name(ty));
                out.push(' ');
                out.push_str(&local_name(name));
                out.push_str(" = ");
                out.push_str(&emit_expr(expr));
                out.push_str(";\n");
            }
        }
        TypedStmt::Reassign {
            name,
            ty,
            expr,
            drop_old,
        } => {
            if *drop_old {
                // Heap-shaped reassign: evaluate the RHS into
                // a temp first, free the OLD value, then move
                // the temp into the binding. The order matters
                // — the RHS may consume the binding itself
                // (e.g. `xs = push(xs, k)` returns a fresh vec
                // that takes ownership of the old buffer), and
                // freeing-before-evaluating would crash. Vec
                // was wired in closure #8 (`drop_old`
                // self-consuming reassign). Closure #133
                // extends the same pattern to OwnedStr — the
                // bare-`x = "b" + ""` case was silently
                // leaking the previous heap string.
                match ty {
                    Type::Vec(element) => {
                        let struct_name = vec_c_struct(element);
                        let tmp = format!("_intent_tmp_{}", name);
                        out.push_str("  {\n");
                        out.push_str("    ");
                        out.push_str(&struct_name);
                        out.push(' ');
                        out.push_str(&tmp);
                        out.push_str(" = ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(";\n    ");
                        out.push_str(&vec_helper(element, "free"));
                        out.push('(');
                        out.push_str(&local_name(name));
                        out.push_str(");\n    ");
                        out.push_str(&local_name(name));
                        out.push_str(" = ");
                        out.push_str(&tmp);
                        out.push_str(";\n  }\n");
                    }
                    Type::OwnedStr => {
                        let tmp = format!("_intent_tmp_{}", name);
                        out.push_str("  {\n");
                        out.push_str("    char* ");
                        out.push_str(&tmp);
                        out.push_str(" = ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(";\n");
                        out.push_str("    free((void*)");
                        out.push_str(&local_name(name));
                        out.push_str(");\n    ");
                        out.push_str(&local_name(name));
                        out.push_str(" = ");
                        out.push_str(&tmp);
                        out.push_str(";\n  }\n");
                    }
                    Type::Struct(struct_name) => {
                        // Closure #147: reassigning a struct
                        // binding that owns heap fields was
                        // leaking the previous fields' heap.
                        // Evaluate RHS into a tmp, walk the
                        // OLD binding's per-field drops, then
                        // move the tmp in.
                        let fields = STRUCT_FIELDS_REGISTRY
                            .with(|r| r.borrow().get(struct_name).cloned())
                            .unwrap_or_default();
                        let tmp = format!("_intent_tmp_{}", name);
                        out.push_str("  {\n");
                        out.push_str("    ");
                        out.push_str(&struct_c_name(struct_name));
                        out.push(' ');
                        out.push_str(&tmp);
                        out.push_str(" = ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(";\n");
                        let empty: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        emit_struct_field_drops(
                            &local_name(name),
                            struct_name,
                            &fields,
                            &empty,
                            out,
                        );
                        out.push_str("    ");
                        out.push_str(&local_name(name));
                        out.push_str(" = ");
                        out.push_str(&tmp);
                        out.push_str(";\n  }\n");
                    }
                    Type::Enum(enum_name) => {
                        // Closure #147: reassigning a
                        // payloaded enum binding was leaking
                        // the previous payload heap. Eval
                        // RHS into a tmp, switch on the OLD
                        // tag to free the payload, then move
                        // the tmp in.
                        let payload_ty = ENUM_PAYLOAD_REGISTRY
                            .with(|r| r.borrow().get(enum_name).cloned());
                        let free_expr: Option<String> = match &payload_ty {
                            Some(Type::OwnedStr) => Some(format!(
                                "free((void*){}.payload)",
                                local_name(name)
                            )),
                            Some(Type::Vec(element)) => Some(format!(
                                "{}({}.payload)",
                                vec_helper(element, "free"),
                                local_name(name)
                            )),
                            _ => None,
                        };
                        let tmp = format!("_intent_tmp_{}", name);
                        out.push_str("  {\n");
                        out.push_str("    ");
                        out.push_str(&enum_c_name(enum_name));
                        out.push(' ');
                        out.push_str(&tmp);
                        out.push_str(" = ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(";\n");
                        if let Some(free_call) = free_expr {
                            let payload_tags: Vec<u32> =
                                ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
                                    r.borrow()
                                        .get(enum_name)
                                        .cloned()
                                        .unwrap_or_default()
                                });
                            if !payload_tags.is_empty() {
                                let cases: Vec<String> = payload_tags
                                    .iter()
                                    .map(|t| format!("case {}", t))
                                    .collect();
                                out.push_str(&format!(
                                    "    switch ({}.tag) {{ {}: {}; break; default: break; }}\n",
                                    local_name(name),
                                    cases.join(": "),
                                    free_call
                                ));
                            }
                        }
                        out.push_str("    ");
                        out.push_str(&local_name(name));
                        out.push_str(" = ");
                        out.push_str(&tmp);
                        out.push_str(";\n  }\n");
                    }
                    _ => {
                        out.push_str("  ");
                        out.push_str(&local_name(name));
                        out.push_str(" = ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(";\n");
                    }
                }
            } else {
                out.push_str("  ");
                out.push_str(&local_name(name));
                out.push_str(" = ");
                out.push_str(&emit_expr(expr));
                out.push_str(";\n");
            }
        }
        TypedStmt::Drop { name, ty, moved_fields } => match ty {
            Type::Vec(element) => {
                out.push_str("  ");
                out.push_str(&vec_helper(element, "free"));
                out.push('(');
                out.push_str(&local_name(name));
                out.push_str(");\n");
            }
            Type::OwnedStr => {
                // Owned strings are heap-allocated by the concat
                // path (malloc); free the buffer here.
                out.push_str("  free((void*)");
                out.push_str(&local_name(name));
                out.push_str(");\n");
            }
            Type::Guard(_) => {
                // RAII: dropping a guard releases the lock.
                // The guard's `m` field still points at the
                // mutex storage; the unlock helper resets the
                // `locked` flag.
                out.push_str("  intent_guard_i64_unlock(&");
                out.push_str(&local_name(name));
                out.push_str(");\n");
            }
            Type::Struct(struct_name) => {
                // Auto-call the user's `Drop` impl when one
                // exists. Two flavors:
                // * `fn drop(self: T)` (by-value) consumes the
                //   binding — valid only when the struct has
                //   no owning fields (otherwise the per-field
                //   pass would double-free what user-drop
                //   already consumed). This is the original
                //   T2.7 phase 2 shape.
                // * `fn drop(self: mut ref T)` (by-ref) runs
                //   user cleanup THEN the per-field pass.
                //   Epic C — unblocks user-Drop for structs
                //   that own OwnedStr / Vec / nested-struct
                //   fields. The user code can read/write
                //   field values; the per-field pass still
                //   reclaims the heap afterwards.
                let fields = STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(struct_name).cloned())
                    .unwrap_or_default();
                let has_user_drop = USER_DROP_REGISTRY
                    .with(|r| r.borrow().contains(struct_name));
                let user_drop_by_ref = crate::ast::user_drop_is_by_ref(struct_name);
                let has_owning_field = fields.iter().any(|(_, ty)| {
                    matches!(ty, Type::OwnedStr | Type::Vec(_) | Type::Struct(_))
                });
                if has_user_drop && user_drop_by_ref {
                    out.push_str("  (void)");
                    out.push_str(&function_name(&format!("{}_drop", struct_name)));
                    out.push_str("(&");
                    out.push_str(&local_name(name));
                    out.push_str(");\n");
                    // Fall through to the per-field drop pass.
                } else if has_user_drop && !has_owning_field {
                    out.push_str("  (void)");
                    out.push_str(&function_name(&format!("{}_drop", struct_name)));
                    out.push_str("(");
                    out.push_str(&local_name(name));
                    out.push_str(");\n");
                    // User drop consumed the value; skip the
                    // per-field free pass below.
                    return;
                }
                // Free every owning field of the struct.
                // OwnedStr fields free their heap buffer; Vec
                // fields go through the per-element-type
                // `intent_vec_<T>__free` helper. Stack-shaped
                // affine fields ([T;N], Task, Atomic) need no
                // runtime drop. Fields are freed in reverse
                // declaration order so destruction mirrors the
                // construction order (Rust's RAII convention).
                // Partial-moved fields are skipped — their
                // value is owned by another binding now.
                // T1.2 phase 2b.
                let moved: std::collections::HashSet<&String> = moved_fields.iter().collect();
                emit_struct_field_drops(
                    &local_name(name),
                    struct_name,
                    &fields,
                    &moved,
                    out,
                );
            }
            Type::Enum(enum_name) => {
                // Payloaded enums with a heap-shaped payload
                // free the payload when the active variant
                // matches. Closure #283: mixed-payload enums
                // route through per-variant `.u.v_<variant>`
                // access (one switch case per variant with
                // owning payload); single-payload enums keep
                // the legacy `.payload` path.
                let variant_payloads = ENUM_VARIANT_PAYLOADS_REGISTRY
                    .with(|r| r.borrow().get(enum_name).cloned());
                let is_mixed_local = variant_payloads.as_ref().map(|v| {
                    let payloads: Vec<&Type> =
                        v.iter().filter_map(|(_, p)| p.as_ref()).collect();
                    payloads.len() >= 2
                        && payloads[1..].iter().any(|t| *t != payloads[0])
                }).unwrap_or(false);
                let local = local_name(name);
                if is_mixed_local {
                    let variants = variant_payloads.unwrap();
                    let mut cases: Vec<String> = Vec::new();
                    for (tag, (vname, pty)) in variants.iter().enumerate() {
                        let Some(pty) = pty.as_ref() else { continue; };
                        let free_for_variant: Option<String> = match pty {
                            Type::OwnedStr => Some(format!(
                                "free((void*){}.u.{})",
                                local, enum_variant_member(vname)
                            )),
                            Type::Vec(element) => Some(format!(
                                "{}({}.u.{})",
                                vec_helper(element, "free"),
                                local, enum_variant_member(vname)
                            )),
                            _ => None,
                        };
                        if let Some(call) = free_for_variant {
                            cases.push(format!(
                                "case {}: {}; break;",
                                tag, call
                            ));
                        }
                    }
                    if !cases.is_empty() {
                        out.push_str(&format!(
                            "  switch ({}.tag) {{ {} default: break; }}\n",
                            local,
                            cases.join(" ")
                        ));
                    }
                    return;
                }
                let payload_ty = ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(enum_name).cloned());
                let free_expr: Option<String> = match &payload_ty {
                    Some(Type::OwnedStr) => Some(format!(
                        "free((void*){}.payload)",
                        local
                    )),
                    Some(Type::Vec(element)) => Some(format!(
                        "{}({}.payload)",
                        vec_helper(element, "free"),
                        local
                    )),
                    _ => None,
                };
                if let Some(free_call) = free_expr {
                    let payload_tags: Vec<u32> =
                        ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
                            r.borrow()
                                .get(enum_name)
                                .cloned()
                                .unwrap_or_default()
                        });
                    if !payload_tags.is_empty() {
                        let cases: Vec<String> = payload_tags
                            .iter()
                            .map(|t| format!("case {}", t))
                            .collect();
                        out.push_str(&format!(
                            "  switch ({}.tag) {{ {}: {}; break; default: break; }}\n",
                            local,
                            cases.join(": "),
                            free_call
                        ));
                    }
                }
            }
            Type::Array { element, length } => {
                // Closure #291 Phase 3 + 4: arrays of
                // non-Copy elements need per-slot drop at
                // scope exit. For Copy element types nothing
                // to do (no heap behind any slot).
                if element.is_copy() {
                    return;
                }
                let local = local_name(name);
                for i in 0..*length {
                    match element.as_ref() {
                        Type::Vec(inner) => {
                            out.push_str(&format!(
                                "  {}({}[{}]);\n",
                                vec_helper(inner, "free"),
                                local, i
                            ));
                        }
                        Type::OwnedStr => {
                            out.push_str(&format!(
                                "  free((void*){}[{}]);\n",
                                local, i
                            ));
                        }
                        Type::Struct(struct_name) => {
                            // Phase 4 (closure #291): walk
                            // each slot's owning fields. The
                            // slot expression is `{local}[i]`;
                            // reuse `emit_struct_field_drops`
                            // which understands the field
                            // registry + per-field free shapes.
                            let fields = STRUCT_FIELDS_REGISTRY
                                .with(|r| r.borrow().get(struct_name).cloned())
                                .unwrap_or_default();
                            let empty: std::collections::HashSet<&String> =
                                std::collections::HashSet::new();
                            let slot_expr = format!("{}[{}]", local, i);
                            emit_struct_field_drops(
                                &slot_expr,
                                struct_name,
                                &fields,
                                &empty,
                                out,
                            );
                        }
                        _ => {
                            // Nested-array / tuple / enum
                            // element types are rare in
                            // practice; if a real test
                            // surfaces, mirror the Struct
                            // arm with the appropriate
                            // per-slot drop sequence.
                        }
                    }
                }
            }
            _ => {
                // Other affine types (Task, Atomic,
                // Channel, Mutex — all stack-allocated structs
                // without heap-owned buffers) emit no runtime
                // drop.
            }
        },
        TypedStmt::Discard { expr } => match &expr.ty {
            Type::Vec(element) => {
                // Bind to a brace-scoped tmp so we can free the buffer. The
                // brace-scope means consecutive `let _ = ...` don't collide.
                let struct_name = vec_c_struct(element);
                out.push_str("  {\n    ");
                out.push_str(&struct_name);
                out.push_str(" _intent_discard = ");
                out.push_str(&emit_expr(expr));
                out.push_str(";\n    ");
                out.push_str(&vec_helper(element, "free"));
                out.push_str("(_intent_discard);\n  }\n");
            }
            Type::OwnedStr => {
                // Closure #134: `let _ = make_owned_str();` must
                // free the returned heap string, otherwise the
                // allocation leaks. Bind to a brace-scoped tmp
                // so consecutive discards don't collide.
                out.push_str("  {\n    char* _intent_discard = ");
                out.push_str(&emit_expr(expr));
                out.push_str(";\n    free((void*)_intent_discard);\n  }\n");
            }
            Type::Array { element, length } => {
                // Arrays have stack lifetime. Still materialize the RHS into
                // a brace-scoped tmp so its side-effecting subexpressions
                // run; C disallows casting an array directly to void.
                out.push_str("  {\n    ");
                if let TypedExprKind::ArrayLit { elements } = &expr.kind {
                    let element_strs: Vec<String> = elements.iter().map(emit_expr).collect();
                    out.push_str(c_leaf_type(element));
                    out.push(' ');
                    out.push_str("_intent_discard[");
                    out.push_str(&length.to_string());
                    out.push_str("] = { ");
                    out.push_str(&element_strs.join(", "));
                    out.push_str(" };\n    (void)_intent_discard;\n  }\n");
                } else {
                    out.push_str(c_leaf_type(element));
                    out.push_str(" _intent_discard[");
                    out.push_str(&length.to_string());
                    out.push_str("];\n    memcpy(_intent_discard, ");
                    out.push_str(&emit_expr(expr));
                    out.push_str(", sizeof(_intent_discard));\n    (void)_intent_discard;\n  }\n");
                }
            }
            Type::Struct(struct_name) => {
                // Closure #145: `let _ = make_struct();` for a
                // struct with heap-shaped fields (OwnedStr,
                // Vec<T>, nested Struct with owning fields)
                // was leaking the per-field heap. Bind to a
                // brace-scoped tmp, walk the struct's fields,
                // and emit the same per-field free chain the
                // scope-exit Drop pass uses. Struct without
                // owning fields → just `(void)(...)`.
                //
                // Closure #277: also fire the user's `Drop`
                // impl when present. Mirrors the `TypedStmt::
                // Drop` arm for `Type::Struct`. Without this,
                // `let _ = make();` silently skipped user-
                // declared cleanup even though end-of-scope
                // drop ran it correctly.
                let fields = STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(struct_name).cloned())
                    .unwrap_or_default();
                let has_owning = fields.iter().any(|(_, ty)| {
                    !ty.is_copy()
                });
                let has_user_drop = USER_DROP_REGISTRY
                    .with(|r| r.borrow().contains(struct_name));
                let user_drop_by_ref = crate::ast::user_drop_is_by_ref(struct_name);
                // By-value user-Drop with no owning fields:
                // user-drop consumes the binding; no per-field
                // pass needed.
                if has_user_drop && !user_drop_by_ref && !has_owning {
                    out.push_str("  (void)");
                    out.push_str(&function_name(&format!("{}_drop", struct_name)));
                    out.push_str("(");
                    out.push_str(&emit_expr(expr));
                    out.push_str(");\n");
                    return;
                }
                if has_owning || has_user_drop {
                    out.push_str("  {\n    ");
                    out.push_str(&struct_c_name(struct_name));
                    out.push_str(" _intent_discard = ");
                    out.push_str(&emit_expr(expr));
                    out.push_str(";\n");
                    if has_user_drop && user_drop_by_ref {
                        out.push_str("    (void)");
                        out.push_str(&function_name(&format!(
                            "{}_drop",
                            struct_name
                        )));
                        out.push_str("(&_intent_discard);\n");
                    }
                    let empty: std::collections::HashSet<&String> =
                        std::collections::HashSet::new();
                    emit_struct_field_drops(
                        "_intent_discard",
                        struct_name,
                        &fields,
                        &empty,
                        out,
                    );
                    out.push_str("  }\n");
                } else {
                    out.push_str("  (void)(");
                    out.push_str(&emit_expr(expr));
                    out.push_str(");\n");
                }
            }
            Type::Enum(enum_name) => {
                // Closure #146: `let _ = make_enum();` for an
                // enum with a heap-shaped payload (OwnedStr,
                // Vec<T>) was leaking. Mirror the scope-exit
                // Drop logic from `TypedStmt::Drop`'s
                // `Type::Enum` arm: bind to a brace-scoped
                // tmp, switch on the tag, and free the
                // payload for variants that carry one.
                let payload_ty = ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(enum_name).cloned());
                let free_expr: Option<String> = match &payload_ty {
                    Some(Type::OwnedStr) => Some(
                        "free((void*)_intent_discard.payload)".to_string(),
                    ),
                    Some(Type::Vec(element)) => Some(format!(
                        "{}(_intent_discard.payload)",
                        vec_helper(element, "free")
                    )),
                    _ => None,
                };
                if let Some(free_call) = free_expr {
                    let payload_tags: Vec<u32> =
                        ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
                            r.borrow()
                                .get(enum_name)
                                .cloned()
                                .unwrap_or_default()
                        });
                    if !payload_tags.is_empty() {
                        let cases: Vec<String> = payload_tags
                            .iter()
                            .map(|t| format!("case {}", t))
                            .collect();
                        out.push_str("  {\n    ");
                        out.push_str(&enum_c_name(enum_name));
                        out.push_str(" _intent_discard = ");
                        out.push_str(&emit_expr(expr));
                        out.push_str(";\n");
                        out.push_str(&format!(
                            "    switch (_intent_discard.tag) {{ {}: {}; break; default: break; }}\n  }}\n",
                            cases.join(": "),
                            free_call
                        ));
                    } else {
                        out.push_str("  (void)(");
                        out.push_str(&emit_expr(expr));
                        out.push_str(");\n");
                    }
                } else {
                    out.push_str("  (void)(");
                    out.push_str(&emit_expr(expr));
                    out.push_str(");\n");
                }
            }
            _ => {
                out.push_str("  (void)(");
                out.push_str(&emit_expr(expr));
                out.push_str(");\n");
            }
        },
        TypedStmt::Return { expr } => {
            // Closure #239: when returning an array-typed
            // value, wrap it in the per-shape struct wrapper.
            // `return [1, 2, 3];` for an `[i64; 3]` return
            // type becomes `return (intent_arr_ret_3_int64_t){
            //   .data = {1, 2, 3}};` so C can pass it by value.
            if let Type::Array { element, length } = &expr.ty {
                let wrapper = array_return_struct_name(element, *length);
                // Inline-array-literal path emits the elements
                // directly into the .data initializer. For
                // any other shape (e.g. a Var referencing a
                // local array), use a memcpy through a stack
                // temp to materialize the struct value.
                if let TypedExprKind::ArrayLit { elements } = &expr.kind {
                    let parts: Vec<String> = elements.iter().map(emit_expr).collect();
                    out.push_str(&format!(
                        "  return ({}){{ .data = {{ {} }} }};\n",
                        wrapper,
                        parts.join(", ")
                    ));
                } else {
                    let elem_storage = c_element_storage(element);
                    out.push_str(&format!(
                        "  {{ {} __intent_ret_data[{}]; \
memcpy(__intent_ret_data, ({}), sizeof(__intent_ret_data)); \
{} __intent_ret = {{0}}; \
memcpy(__intent_ret.data, __intent_ret_data, sizeof(__intent_ret_data)); \
return __intent_ret; }}\n",
                        elem_storage,
                        length,
                        emit_expr(expr),
                        wrapper,
                    ));
                }
                return;
            }
            out.push_str("  return ");
            out.push_str(&emit_expr(expr));
            out.push_str(";\n");
        }
        TypedStmt::Assert { expr, message } => {
            // C `assert` macro stringifies its sole argument. To emit a
            // custom message, fall back to `if (!cond) { fprintf(stderr,...);
            // abort(); }` which keeps the same abort-on-failure shape.
            if let Some(msg) = message {
                out.push_str("  if (!(");
                out.push_str(&emit_expr(expr));
                out.push_str(")) { fprintf(stderr, \"assertion failed: ");
                out.push_str(&escape_c_string(msg));
                out.push_str("\\n\"); abort(); }\n");
            } else {
                out.push_str("  assert(");
                out.push_str(&emit_expr(expr));
                out.push_str(");\n");
            }
        }
        TypedStmt::Prove { expr } => {
            out.push_str("  /* proven by compiler: ");
            out.push_str(&escape_comment(&emit_expr(expr)));
            out.push_str(" */\n");
        }
        TypedStmt::Print { items } => emit_print_items(items, out),
        TypedStmt::If {
            cond,
            then_body,
            else_body,
        } => {
            out.push_str("  if (");
            out.push_str(&emit_expr(cond));
            out.push_str(") {\n");
            for s in then_body {
                emit_stmt(s, out);
            }
            out.push_str("  }");
            if !else_body.is_empty() {
                out.push_str(" else {\n");
                for s in else_body {
                    emit_stmt(s, out);
                }
                out.push_str("  }");
            }
            out.push('\n');
        }
        TypedStmt::While { cond, body } => {
            out.push_str("  while (");
            out.push_str(&emit_expr(cond));
            out.push_str(") {\n");
            for s in body {
                emit_stmt(s, out);
            }
            out.push_str("  }\n");
        }
        TypedStmt::Break => {
            out.push_str("  break;\n");
        }
        TypedStmt::Continue => {
            out.push_str("  continue;\n");
        }
        TypedStmt::IndexAssign {
            name,
            base_ty,
            index,
            field_path,
            value,
            checked,
        } => emit_index_assign(name, base_ty, index, field_path, value, *checked, out),
        TypedStmt::FieldAssign {
            object,
            field,
            through_mut_ref,
            value,
            ..
        } => {
            // C emit: `obj.field = value;` for owned struct
            // values, `obj->field = value;` for `mut ref`
            // borrows (typed-AST `RefMut` collapses to a
            // pointer in C codegen — see field-access
            // emission). T1.2 phase 2a follow-up.
            //
            // Heap-shaped field overwrite: when the field
            // type is OwnedStr or Vec<T>, the previous slot's
            // resources must be freed before the new value
            // is stored, otherwise the old allocation leaks.
            // Mirrors the leaf-Drop logic in `emit_index_assign`
            // (closure #126 / F2). Closure #132.
            let obj = emit_expr(object);
            let v = emit_expr(value);
            let op = if *through_mut_ref { "->" } else { "." };
            let lvalue = format!("{}{}{}", obj, op, field);
            match &value.ty {
                Type::OwnedStr => {
                    out.push_str(&format!("  free((void*){});\n", lvalue));
                }
                Type::Vec(element) => {
                    out.push_str(&format!(
                        "  {}({});\n",
                        vec_helper(element, "free"),
                        lvalue
                    ));
                }
                Type::Struct(struct_name) => {
                    // Closure #148: assigning a struct-typed
                    // field (`t.inner = newInner`) must free
                    // the previous struct's owning fields,
                    // otherwise the nested heap leaks.
                    let fields = STRUCT_FIELDS_REGISTRY
                        .with(|r| r.borrow().get(struct_name).cloned())
                        .unwrap_or_default();
                    let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
                    if has_owning {
                        let empty: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        emit_struct_field_drops(
                            &lvalue,
                            struct_name,
                            &fields,
                            &empty,
                            out,
                        );
                    }
                }
                Type::Enum(enum_name) => {
                    // Closure #148: assigning a payloaded-enum-
                    // typed field must free the previous
                    // payload heap. Same shape as the Reassign
                    // enum case.
                    let payload_ty = ENUM_PAYLOAD_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned());
                    let free_expr: Option<String> = match &payload_ty {
                        Some(Type::OwnedStr) => Some(format!(
                            "free((void*){}.payload)",
                            lvalue
                        )),
                        Some(Type::Vec(element)) => Some(format!(
                            "{}({}.payload)",
                            vec_helper(element, "free"),
                            lvalue
                        )),
                        _ => None,
                    };
                    if let Some(free_call) = free_expr {
                        let payload_tags: Vec<u32> =
                            ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
                                r.borrow()
                                    .get(enum_name)
                                    .cloned()
                                    .unwrap_or_default()
                            });
                        if !payload_tags.is_empty() {
                            let cases: Vec<String> = payload_tags
                                .iter()
                                .map(|t| format!("case {}", t))
                                .collect();
                            out.push_str(&format!(
                                "  switch ({}.tag) {{ {}: {}; break; default: break; }}\n",
                                lvalue,
                                cases.join(": "),
                                free_call
                            ));
                        }
                    }
                }
                _ => {}
            }
            out.push_str(&format!("  {} = {};\n", lvalue, v));
        }
        TypedStmt::For {
            var,
            ty,
            start,
            end,
            body,
            parallel,
            reductions,
        } => {
            let local = local_name(var);
            let c_ty = c_leaf_type(ty);
            if *parallel {
                // Effects verifier has proven the body is pure
                // (no shared mutable state, no I/O, no consuming
                // mutator calls); reductions are carved out via
                // the `reduction(op:var)` clause so OpenMP gives
                // each thread a private partial and combines.
                // Compilers without `-fopenmp` issue an "unknown
                // pragma" warning and fall back to sequential —
                // also correct.
                let mut pragma = String::from("omp parallel for");
                for r in reductions {
                    pragma.push_str(&format!(
                        " reduction({}:{})",
                        r.op.display_symbol(),
                        local_name(&r.var)
                    ));
                }
                out.push_str(&format!("  _Pragma(\"{}\")\n", pragma));
            }
            out.push_str(&format!(
                "  for ({0} {1} = {2}; {1} < {3}; {1}++) {{\n",
                c_ty,
                local,
                emit_expr(start),
                emit_expr(end)
            ));
            for s in body {
                emit_stmt(s, out);
            }
            out.push_str("  }\n");
        }
        TypedStmt::ForIter {
            var,
            element_ty,
            collection,
            collection_ty,
            consumes,
            body,
        } => emit_for_iter(
            var,
            element_ty,
            collection,
            collection_ty,
            *consumes,
            body,
            out,
        ),
        TypedStmt::TaskSpawn { name, body, captures } => {
            // Spawn the task on a real pthread. Allocate a
            // per-spawn outline ID, emit the outline + ctx
            // struct into the module-scope TASK_OUTLINES
            // buffer, and at the spawn site malloc +
            // populate the ctx, then call pthread_create.
            let id = TASK_OUTLINE_COUNTER.with(|c| {
                let n = c.get();
                c.set(n + 1);
                n
            });
            let struct_name = format!("intent_task_{}_ctx", id);
            let outline_fn = format!("intent_task_{}", id);
            // Build the outline + struct typedef in a side
            // buffer.
            let mut outline = String::new();
            outline.push_str(&format!("typedef struct {} {{\n", struct_name));
            for (cap_name, cap_ty) in captures {
                outline.push_str(&format!(
                    "  {};\n",
                    format_declarator(cap_ty, &format!("cap_{}", cap_name))
                ));
            }
            outline.push_str(&format!("}} {};\n\n", struct_name));
            outline.push_str(&format!(
                "static void* {}(void* _ctx_raw) {{\n",
                outline_fn
            ));
            outline.push_str(&format!(
                "  {}* ctx = ({}*)_ctx_raw;\n",
                struct_name, struct_name
            ));
            // Locals re-aliasing the ctx fields so the body's
            // emit (which uses local_name(...) for variables)
            // sees the captures as ordinary locals.
            for (cap_name, cap_ty) in captures {
                outline.push_str(&format!(
                    "  {} = ctx->cap_{};\n",
                    format_declarator(cap_ty, &local_name(cap_name)),
                    cap_name
                ));
            }
            for s in body {
                emit_stmt(s, &mut outline);
            }
            outline.push_str("  return (void*)0;\n");
            outline.push_str("}\n\n");
            TASK_OUTLINES.with(|b| b.borrow_mut().push_str(&outline));

            // Spawn-site code: allocate the ctx, populate
            // each capture, build the handle, fire
            // pthread_create.
            out.push_str(&format!(
                "  intent_task_handle {};\n",
                local_name(name)
            ));
            out.push_str(&format!(
                "  {}* _intent_ctx_{} = ({}*)malloc(sizeof({}));\n",
                struct_name, id, struct_name, struct_name
            ));
            for (cap_name, _) in captures {
                out.push_str(&format!(
                    "  _intent_ctx_{}->cap_{} = {};\n",
                    id,
                    cap_name,
                    local_name(cap_name)
                ));
            }
            out.push_str(&format!(
                "  intent_thread_create(&{}.thread, {}, _intent_ctx_{});\n",
                local_name(name),
                outline_fn,
                id
            ));
            out.push_str(&format!(
                "  {}.ctx = _intent_ctx_{};\n",
                local_name(name),
                id
            ));
        }
        TypedStmt::TaskJoin { name } => {
            // Real-thread join: block until the worker
            // exits and free the heap-allocated ctx struct.
            out.push_str(&format!(
                "  intent_thread_join({}.thread);\n",
                local_name(name)
            ));
            out.push_str(&format!("  free({}.ctx);\n", local_name(name)));
        }
    }
}

fn emit_for_iter(
    var: &str,
    element_ty: &Type,
    collection: &str,
    collection_ty: &Type,
    consumes: bool,
    body: &[TypedStmt],
    out: &mut String,
) {
    let idx = format!("_intent_idx_{}", var);
    let elem_local = local_name(var);
    let coll_local = local_name(collection);
    let underlying = collection_ty.deref();
    let is_ref = collection_ty.is_any_ref();

    // (length_expr, element_access)
    let (length_expr, elem_access) = match underlying {
        Type::Array { length, .. } => {
            (format!("{}", length), format!("{}[{}]", coll_local, idx))
        }
        Type::Vec(_) => {
            let prefix = if is_ref {
                format!("(*{})", coll_local)
            } else {
                coll_local.clone()
            };
            (
                format!("{}.len", prefix),
                format!("{}.data[{}]", prefix, idx),
            )
        }
        _ => return, // checker rejects other cases
    };

    out.push_str(&format!(
        "  for (uint64_t {0} = 0; {0} < {1}; {0}++) {{\n",
        idx, length_expr
    ));
    // Use the element's full storage spelling (handles
    // `Vec<U>` aggregates via the per-type typedef alias).
    // Was emitting `"/* vec */"` for nested Vec elements.
    // Refines #7 phase 2.
    out.push_str(&format!(
        "    {} {} = {};\n",
        c_element_storage(element_ty),
        elem_local,
        elem_access
    ));
    for s in body {
        emit_stmt(s, out);
    }
    out.push_str("  }\n");

    // Consuming iteration owns the source for the duration of the loop.
    // For owned `Vec<T>`, the buffer must be freed when the loop exits.
    // Arrays have stack lifetime so no free is needed.
    //
    // For non-Copy elements, each slot was loaded into `x` and freed
    // by x's scope-exit drop in the body. Routing through
    // `intent_vec_<T>__free` here would re-walk every slot
    // (closure #127's per-element drop) and double-free. Skip the
    // helper and emit only the outer buffer free.
    if consumes && !is_ref {
        if let Type::Vec(element) = underlying {
            if element.is_copy() {
                out.push_str(&format!(
                    "  {}({});\n",
                    vec_helper(element, "free"),
                    coll_local
                ));
            } else {
                out.push_str(&format!(
                    "  free({}.data);\n",
                    coll_local
                ));
            }
        }
    }
}

/// Emit per-field free calls for a struct binding at the
/// given C path (e.g. `v_o` or `v_o.inner`). Recursively
/// descends into nested struct fields. Heap fields
/// (OwnedStr, Vec) emit a free; nested struct fields recurse;
/// other field types are no-ops. Fields are walked in
/// reverse declaration order (Rust RAII convention).
/// T1.2 phase 2b + D2.
fn emit_struct_field_drops(
    path: &str,
    struct_name: &str,
    fields: &[(String, Type)],
    moved: &std::collections::HashSet<&String>,
    out: &mut String,
) {
    for (field_name, field_ty) in fields.iter().rev() {
        if moved.contains(field_name) {
            continue;
        }
        match field_ty {
            Type::OwnedStr => {
                out.push_str("  free((void*)");
                out.push_str(path);
                out.push('.');
                out.push_str(field_name);
                out.push_str(");\n");
            }
            Type::Vec(element) => {
                out.push_str("  ");
                out.push_str(&vec_helper(element, "free"));
                out.push('(');
                out.push_str(path);
                out.push('.');
                out.push_str(field_name);
                out.push_str(");\n");
            }
            Type::Struct(inner_name) => {
                // Recurse into the nested struct's fields.
                let inner_fields = STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(inner_name).cloned())
                    .unwrap_or_default();
                if !inner_fields.is_empty() {
                    let inner_path = format!("{}.{}", path, field_name);
                    let empty: std::collections::HashSet<&String> =
                        std::collections::HashSet::new();
                    emit_struct_field_drops(
                        &inner_path,
                        inner_name,
                        &inner_fields,
                        &empty,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
    let _ = struct_name; // reserved for future per-struct diagnostics
}

fn emit_index_assign(
    name: &str,
    base_ty: &Type,
    index: &TypedExpr,
    field_path: &[(String, u32)],
    value: &TypedExpr,
    checked: bool,
    out: &mut String,
) {
    let local = local_name(name);
    let index_str = emit_expr(index);
    let value_str = emit_expr(value);

    // Build the per-field suffix once: `.field1.field2…`.
    // Empty for plain `xs[i] = v;`. T1.2 phase 2b follow-up.
    let field_suffix: String = field_path
        .iter()
        .map(|(name, _)| format!(".{}", name))
        .collect();

    let underlying = base_ty.deref();
    let through_ref = base_ty.is_ref_mut();

    let element_ty: Option<Type> = match underlying {
        Type::Array { element, .. } => Some((**element).clone()),
        Type::Vec(element) => Some((**element).clone()),
        _ => None,
    };

    // Resolve the leaf field type for the field_path (if any).
    // If the leaf is a heap-shaped field (OwnedStr / Vec<T>),
    // we must Drop the old slot value before overwriting it,
    // otherwise the previous heap allocation leaks. The Copy
    // gate in the checker permits this only at the leaf
    // position; intermediate segments stay Copy. F2 / #126.
    let leaf_ty: Option<Type> = element_ty.as_ref().and_then(|el| {
        let mut cur = el.clone();
        for (seg, _) in field_path {
            let Type::Struct(struct_name) = &cur else {
                return None;
            };
            let fields = STRUCT_FIELDS_REGISTRY
                .with(|r| r.borrow().get(struct_name).cloned())
                .unwrap_or_default();
            let next = fields.iter().find(|(n, _)| n == seg).map(|(_, t)| t.clone());
            cur = next?;
        }
        Some(cur)
    });

    // Build the lvalue prefix and slot index expression for
    // the chosen container shape. The lvalue used for the
    // pre-Drop free MUST match the lvalue used for the store,
    // so we compute it once.
    let (slot_lvalue, store_line): (Option<String>, String) = match underlying {
        Type::Array { length, .. } => {
            let idx_expr = if checked {
                format!("intent_check_bounds((uint64_t)({}), {})", index_str, length)
            } else {
                index_str.clone()
            };
            let lv = format!("{}[{}]{}", local, idx_expr, field_suffix);
            let store = format!("  {} = {};\n", lv, value_str);
            (Some(lv), store)
        }
        Type::Vec(_) => {
            let prefix = if through_ref {
                format!("(*{})", local)
            } else {
                local.clone()
            };
            let idx_expr = if checked {
                format!(
                    "intent_check_bounds((uint64_t)({}), {}.len)",
                    index_str, prefix
                )
            } else {
                format!("(uint64_t)({})", index_str)
            };
            let lv = format!("{}.data[{}]{}", prefix, idx_expr, field_suffix);
            let store = format!("  {} = {};\n", lv, value_str);
            (Some(lv), store)
        }
        _ => (
            None,
            format!("  /* unsupported index-assign target for {} */\n", base_ty),
        ),
    };

    if let (Some(lv), Some(lty)) = (slot_lvalue.as_ref(), leaf_ty.as_ref()) {
        // Mixed-place leaf drop (closure #126 / F2): when the
        // assignment writes through a field path, free the OLD
        // leaf field's heap before storing the new value.
        if !field_path.is_empty() {
            match lty {
                Type::OwnedStr => {
                    out.push_str(&format!("  free((void*){});\n", lv));
                }
                Type::Vec(elem) => {
                    out.push_str(&format!("  {}({});\n", vec_helper(elem, "free"), lv));
                }
                _ => {}
            }
        } else {
            // Whole-element overwrite (closure #149 / #150):
            // `xs[i] = newval` for ANY heap-shaped element
            // must free the OLD slot's heap before the store.
            // Previously only the field_path != [] case was
            // handled at the leaf level, so several
            // whole-element shapes leaked.
            match lty {
                Type::OwnedStr => {
                    // Closure #150: `Vec<OwnedStr>[i] = "x" + "y"`
                    // — free the old i8* before storing the new.
                    out.push_str(&format!("  free((void*){});\n", lv));
                }
                Type::Vec(elem) => {
                    // Closure #150: `Vec<Vec<i64>>[i] = vec(…)`
                    // — call the inner __free over the old
                    // slot before storing the new struct.
                    out.push_str(&format!(
                        "  {}({});\n",
                        vec_helper(elem, "free"),
                        lv
                    ));
                }
                Type::Struct(struct_name) => {
                    let fields = STRUCT_FIELDS_REGISTRY
                        .with(|r| r.borrow().get(struct_name).cloned())
                        .unwrap_or_default();
                    let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
                    if has_owning {
                        let empty: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        emit_struct_field_drops(
                            lv,
                            struct_name,
                            &fields,
                            &empty,
                            out,
                        );
                    }
                }
                Type::Enum(enum_name) => {
                    let payload_ty = ENUM_PAYLOAD_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned());
                    let free_expr: Option<String> = match &payload_ty {
                        Some(Type::OwnedStr) => Some(format!(
                            "free((void*){}.payload)",
                            lv
                        )),
                        Some(Type::Vec(element)) => Some(format!(
                            "{}({}.payload)",
                            vec_helper(element, "free"),
                            lv
                        )),
                        _ => None,
                    };
                    if let Some(free_call) = free_expr {
                        let payload_tags: Vec<u32> =
                            ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
                                r.borrow()
                                    .get(enum_name)
                                    .cloned()
                                    .unwrap_or_default()
                            });
                        if !payload_tags.is_empty() {
                            let cases: Vec<String> = payload_tags
                                .iter()
                                .map(|t| format!("case {}", t))
                                .collect();
                            out.push_str(&format!(
                                "  switch ({}.tag) {{ {}: {}; break; default: break; }}\n",
                                lv,
                                cases.join(": "),
                                free_call
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    out.push_str(&store_line);
}

/// Emit a `print item1, item2, …;` statement. Each item is printed
/// without a newline; a single space separates adjacent items; a
/// final newline terminates the line.
fn emit_print_items(items: &[crate::ir::TypedPrintItem], out: &mut String) {
    use crate::ir::TypedPrintItem;
    for (i, item) in items.iter().enumerate() {
        match item {
            TypedPrintItem::Str(s) => {
                // fputs doesn't append a newline; perfect for the
                // mid-line case.
                out.push_str("  fputs(\"");
                out.push_str(&escape_c_string(s));
                out.push_str("\", stdout);\n");
            }
            TypedPrintItem::Expr(expr) => emit_print_expr_no_newline(expr, out),
        }
        if i + 1 < items.len() {
            out.push_str("  fputs(\" \", stdout);\n");
        }
    }
    out.push_str("  putchar('\\n');\n");
}

fn emit_print_expr_no_newline(expr: &TypedExpr, out: &mut String) {
    match &expr.ty {
        Type::Bool => {
            out.push_str("  fputs(");
            out.push_str(&emit_expr(expr));
            out.push_str(" ? \"true\" : \"false\", stdout);\n");
        }
        Type::U8 | Type::U16 | Type::U32 | Type::U64 => {
            out.push_str("  printf(\"%llu\", (unsigned long long)(");
            out.push_str(&emit_expr(expr));
            out.push_str("));\n");
        }
        Type::F32 | Type::F64 => {
            out.push_str("  printf(\"%g\", (double)(");
            out.push_str(&emit_expr(expr));
            out.push_str("));\n");
        }
        Type::Str => {
            out.push_str("  fputs(");
            out.push_str(&emit_expr(expr));
            out.push_str(", stdout);\n");
        }
        Type::OwnedStr => {
            // Conservative whitelist: only Call returning
            // OwnedStr (intent_str_concat / user fn) and
            // Binary `+` (string concat) are guaranteed-fresh
            // heap-producers in v1. Var / FieldAccess /
            // TupleAccess reference a value owned by some
            // binding (whose scope-exit Drop frees the heap)
            // — freeing after print would double-free. Bind
            // to a brace-scoped tmp so the free has a stable
            // handle and consecutive prints don't collide.
            // Closure #135.
            let is_fresh = crate::ir::is_fresh_owned_str(expr);
            if is_fresh {
                out.push_str("  {\n    char* _intent_print_tmp = ");
                out.push_str(&emit_expr(expr));
                out.push_str(";\n");
                out.push_str("    fputs(_intent_print_tmp, stdout);\n");
                out.push_str("    free((void*)_intent_print_tmp);\n");
                out.push_str("  }\n");
            } else {
                out.push_str("  fputs(");
                out.push_str(&emit_expr(expr));
                out.push_str(", stdout);\n");
            }
        }
        Type::Array { .. } | Type::Vec(_) => {
            out.push_str("  /* aggregate print not supported */\n");
        }
        _ => {
            out.push_str("  printf(\"%lld\", (long long)(");
            out.push_str(&emit_expr(expr));
            out.push_str("));\n");
        }
    }
}

fn emit_expr(expr: &TypedExpr) -> String {
    match &expr.kind {
        TypedExprKind::Int(value) => value.to_string(),
        TypedExprKind::Float(value) => emit_float_literal(*value, &expr.ty),
        TypedExprKind::Bool(value) => {
            if *value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        TypedExprKind::Str(text) => format!("\"{}\"", escape_c_string(text)),
        TypedExprKind::Var(name) => local_name(name),
        TypedExprKind::Unary { op, expr } => {
            let symbol = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "!",
            };
            format!("({}{})", symbol, emit_expr(expr))
        }
        TypedExprKind::Binary { op, left, right, checked } => {
            emit_binary(*op, left, right, *checked, &expr.ty)
        }
        TypedExprKind::Call { name, args, .. } => emit_call(name, args, &expr.ty),
        TypedExprKind::Cast { expr, ty } => {
            format!("(({})({}))", c_leaf_type(ty), emit_expr(expr))
        }
        TypedExprKind::ArrayLit { elements } => {
            let array_ty = match &expr.ty {
                Type::Array { element, length } => format!("{}[{}]", c_leaf_type(element), length),
                _ => "/* array */".to_string(),
            };
            let parts: Vec<String> = elements.iter().map(emit_expr).collect();
            format!("(({}){{ {} }})", array_ty, parts.join(", "))
        }
        TypedExprKind::Index {
            array,
            index,
            checked,
        } => emit_index(array, index, *checked),
        TypedExprKind::Len { array, length } => emit_len(array, *length),
        TypedExprKind::Ref { name } | TypedExprKind::RefMut { name } => {
            // For arrays, C array-decay means just passing the name works.
            // For Vecs and primitives, take the address.
            let inner_ty = match &expr.ty {
                Type::Ref(inner) | Type::RefMut(inner) => inner,
                _ => unreachable!("Ref/RefMut TypedExpr must have ref type"),
            };
            match &**inner_ty {
                Type::Array { .. } => local_name(name),
                _ => format!("&{}", local_name(name)),
            }
        }
        TypedExprKind::RefField { object, field, object_ty, .. }
        | TypedExprKind::RefMutField { object, field, object_ty, .. } => {
            // `ref t.x` / `mut ref t.x` — take the address of
            // the struct field. C array-decay applies the same
            // way as for plain `Ref { name }`: passing
            // `v_t.field` works without `&` for array fields.
            // When `object` is bound to a ref-typed value
            // (e.g. `self: ref T` in a method body) we use
            // `v_t->field` since v_t is a pointer. Closure
            // #165.
            let inner_ty = match &expr.ty {
                Type::Ref(inner) | Type::RefMut(inner) => inner,
                _ => unreachable!("RefField/RefMutField must have ref type"),
            };
            let sep = if object_ty.is_any_ref() { "->" } else { "." };
            // Closure #210: when the object is borrowed via
            // `ref T` (read-only borrow), the C parameter is
            // `const T*`, so `&v_t->field` would be
            // `const FieldType*`. For Mutex/Atomic/Channel
            // fields, the helper functions
            // (`intent_mutex_i64_lock` etc.) take non-const
            // pointers — atomic-style ops conceptually
            // mutate even via a read-only borrow. Without a
            // const-strip cast, cc warns
            // `-Wdiscarded-qualifiers`. Closure #176
            // already handled the analogous shape for direct
            // `ref Mutex<T>` / `ref Channel<T,N>` / `ref
            // Atomic<T>` params; #210 covers field-borrow
            // through `ref Struct`.
            let needs_const_strip = object_ty.is_ref()
                && matches!(
                    &**inner_ty,
                    Type::Mutex(_) | Type::Atomic(_) | Type::Channel(_, _)
                );
            match &**inner_ty {
                Type::Array { .. } => format!("{}{}{}", local_name(object), sep, field),
                _ if needs_const_strip => {
                    let storage = match &**inner_ty {
                        Type::Mutex(_) => "intent_mutex_i64".to_string(),
                        Type::Atomic(element) => c_atomic_storage(element),
                        Type::Channel(element, capacity) => {
                            c_channel_storage(element, *capacity)
                        }
                        _ => unreachable!(),
                    };
                    format!(
                        "({}*)&{}{}{}",
                        storage,
                        local_name(object),
                        sep,
                        field
                    )
                }
                _ => format!("&{}{}{}", local_name(object), sep, field),
            }
        }
        TypedExprKind::FnRef { name, .. } => {
            // C function names decay to function pointers
            // when used in non-call positions, so emitting the
            // bare prefixed identifier just works.
            function_name(name)
        }
        TypedExprKind::CallIndirect { callee, args } => {
            // `callee(args)` — C-style indirect call. Function
            // pointers are dereferenced implicitly when
            // invoked, so the simple form is enough.
            let callee_c = emit_expr(callee);
            let arg_parts: Vec<String> = args.iter().map(emit_expr).collect();
            format!("{}({})", callee_c, arg_parts.join(", "))
        }
        TypedExprKind::Tuple { elements } => {
            // `(intent_tuple_<shape>){ ._0 = …, ._1 = …, … }`
            // designated-initializer form. The struct typedef is
            // emitted in the preamble's `emit_tuple_bundle` pass.
            // Refines T1.1 phase 2.
            let elem_tys: Vec<Type> = elements.iter().map(|e| e.ty.clone()).collect();
            let struct_name = tuple_c_struct(&elem_tys);
            let parts: Vec<String> = elements
                .iter()
                .enumerate()
                .map(|(i, e)| format!("._{} = {}", i, emit_expr(e)))
                .collect();
            format!("({}){{ {} }}", struct_name, parts.join(", "))
        }
        TypedExprKind::TupleAccess { tuple, index } => {
            let inner = emit_expr(tuple);
            format!("({})._{}", inner, index)
        }
        TypedExprKind::StructLit { type_name, fields } => {
            // `(Struct_<Name>){ .field1 = …, .field2 = … }`
            // designated-initializer compound literal. T1.2.
            // Array-typed fields with an inline `[…]` ArrayLit
            // initializer use a bare-brace `{e1, e2, …}` form
            // since C forbids assigning a compound-literal-array
            // to a struct member of array type. T1.2 phase 2b.
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, e)| {
                    let rhs = match (&e.ty, &e.kind) {
                        (Type::Array { .. }, TypedExprKind::ArrayLit { elements }) => {
                            let parts: Vec<String> = elements.iter().map(emit_expr).collect();
                            format!("{{ {} }}", parts.join(", "))
                        }
                        _ => emit_expr(e),
                    };
                    format!(".{} = {}", n, rhs)
                })
                .collect();
            format!("({}){{ {} }}", struct_c_name(type_name), parts.join(", "))
        }
        TypedExprKind::FieldAccess { object, field, .. } => {
            // Through-a-borrow access uses `->`; by-value
            // uses `.`. Distinguish via the operand's type.
            let inner = emit_expr(object);
            if object.ty.is_any_ref() {
                format!("({})->{}", inner, field)
            } else {
                format!("({}).{}", inner, field)
            }
        }
        TypedExprKind::EnumVariant { enum_name, tag, .. } => {
            // Plain (payload-less) variant: just the tag.
            // Payloaded enum's payload-less variant: build a
            // tagged-union struct with `.tag` set and the
            // payload field zero-initialized. For mixed-payload
            // enums (closure #283) the payload sits inside a
            // `.u` union; the payload-less variant just sets
            // the tag and leaves the union zeroed. Aggregate
            // payload types need brace-init.
            let payloaded = ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(enum_name));
            if payloaded {
                let is_mixed = ENUM_VARIANT_PAYLOADS_REGISTRY.with(|r| {
                    r.borrow().get(enum_name).map(|v| {
                        let payloads: Vec<&Type> =
                            v.iter().filter_map(|(_, p)| p.as_ref()).collect();
                        payloads.len() >= 2
                            && payloads[1..].iter().any(|t| *t != payloads[0])
                    }).unwrap_or(false)
                });
                if is_mixed {
                    // Mixed-payload: just set the tag; the
                    // union's storage is zero-initialized
                    // implicitly by the absent designator.
                    return format!(
                        "(({}){{ .tag = (int32_t){} }})",
                        enum_c_name(enum_name),
                        tag,
                    );
                }
                let payload_ty = ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(enum_name).cloned())
                    .expect("just checked payloaded");
                let payload_zero = match &payload_ty {
                    Type::Vec(_) | Type::Tuple(_) | Type::Struct(_) | Type::Array { .. } => "{0}",
                    _ => "0",
                };
                format!(
                    "(({}){{ .tag = (int32_t){}, .payload = {} }})",
                    enum_c_name(enum_name),
                    tag,
                    payload_zero
                )
            } else {
                format!("((int32_t){})", tag)
            }
        }
        TypedExprKind::EnumVariantWithPayload { enum_name, tag, payload, .. } => {
            // T1.3 phase 2b: build the tagged-union struct
            // literal with both `.tag` and `.payload` set.
            // Array payloads need a bare-brace `{e1, e2, …}`
            // initializer since C forbids assigning a
            // compound-literal array into a struct field of
            // array type. Same fix as struct fields in
            // closure #100. Closure #119.
            let payload_str = match (&payload.ty, &payload.kind) {
                (Type::Array { .. }, TypedExprKind::ArrayLit { elements }) => {
                    let parts: Vec<String> = elements.iter().map(emit_expr).collect();
                    format!("{{ {} }}", parts.join(", "))
                }
                _ => emit_expr(payload),
            };
            // Closure #283: mixed-payload-type enums store
            // the payload through a per-variant union member
            // (`.u.v_<variant>`). Single-payload-type enums
            // keep the legacy `.payload` field for back-
            // compat.
            let is_mixed = ENUM_VARIANT_PAYLOADS_REGISTRY.with(|r| {
                r.borrow().get(enum_name).map(|v| {
                    let payloads: Vec<&Type> =
                        v.iter().filter_map(|(_, p)| p.as_ref()).collect();
                    payloads.len() >= 2
                        && payloads[1..].iter().any(|t| *t != payloads[0])
                }).unwrap_or(false)
            });
            if is_mixed {
                // Look up the variant name from the
                // per-variant registry by tag.
                let variant_name = ENUM_VARIANT_PAYLOADS_REGISTRY.with(|r| {
                    r.borrow().get(enum_name).and_then(|v| {
                        v.get(*tag as usize).map(|(n, _)| n.clone())
                    })
                }).unwrap_or_else(|| format!("tag{}", tag));
                let member = enum_variant_member(&variant_name);
                return format!(
                    "(({}){{ .tag = (int32_t){}, .u = {{ .{} = {} }} }})",
                    enum_c_name(enum_name),
                    tag,
                    member,
                    payload_str
                );
            }
            format!(
                "(({}){{ .tag = (int32_t){}, .payload = {} }})",
                enum_c_name(enum_name),
                tag,
                payload_str
            )
        }
        TypedExprKind::Match { scrutinee, arms } => {
            // GCC statement-expression: switch on the tag,
            // materialize each arm's value into a fresh
            // temp, yield the temp. Exhaustiveness is
            // checker-enforced; if there's no wildcard arm
            // the default aborts so out-of-spec values trip
            // loudly. With a wildcard arm, the default
            // branch *is* its body. T1.3 (wildcard).
            // Use `c_type_name` so payloaded-enum result
            // types render as `Enum_<Name>` rather than the
            // bare `int32_t` tag (the latter would mismatch
            // the arm bodies' struct literals when the match
            // returns a payloaded enum). Closure #130
            // (`try` follow-up + Match-on-Enum-result C
            // codegen fix).
            let result_ty = c_type_name(&expr.ty);
            // T1.3 phase 2b: detect whether scrutinee is a
            // payloaded enum so dispatch can use `.tag` and
            // payload bindings can be extracted via `.payload`.
            let scrutinee_payloaded = match &scrutinee.ty {
                Type::Enum(name) => {
                    ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name))
                }
                _ => false,
            };
            let scr_full = emit_expr(scrutinee);
            let mut body = String::new();
            // For payloaded enums, materialize the scrutinee
            // into a fresh local so we can read both .tag (for
            // dispatch) and .payload (for binding) without
            // re-evaluating the source expression.
            let dispatch = if scrutinee_payloaded {
                let enum_name = match &scrutinee.ty {
                    Type::Enum(n) => n,
                    _ => unreachable!(),
                };
                body.push_str(&format!(
                    "{} __scr = ({}); ",
                    enum_c_name(enum_name),
                    scr_full
                ));
                "__scr.tag".to_string()
            } else if matches!(&scrutinee.ty, Type::Bool) {
                // Closure #205: gcc warns
                // `switch condition has boolean value` (-Wswitch-bool)
                // when the dispatch expression is bool-typed. Cast
                // to int so the switch's `case 0` / `case 1` arms
                // dispatch on the canonical 0/1 representation
                // without the warning.
                format!("((int){})", scr_full)
            } else {
                scr_full
            };
            body.push_str(&format!("{} __r; ", result_ty));
            body.push_str(&format!("switch ({}) {{ ", dispatch));
            let mut wildcard_body: Option<String> = None;
            for arm in arms {
                if arm.is_wildcard {
                    let arm_v = emit_expr(&arm.body);
                    wildcard_body = Some(arm_v);
                    continue;
                }
                // For VariantWithBinding patterns, emit a fresh
                // scoped block that declares the local binding
                // initialized from the payload (legacy
                // `.payload` for single-type, or `.u.v_<variant>`
                // for mixed-payload — closure #283).
                let arm_block = if let Some((bname, bty)) = &arm.binding {
                    let body_v = emit_expr(&arm.body);
                    let scrutinee_enum_name = match &scrutinee.ty {
                        Type::Enum(n) => Some(n.clone()),
                        _ => None,
                    };
                    let payload_access = if let Some(enum_n) = &scrutinee_enum_name {
                        let is_mixed = ENUM_VARIANT_PAYLOADS_REGISTRY.with(|r| {
                            r.borrow().get(enum_n).map(|v| {
                                let payloads: Vec<&Type> =
                                    v.iter().filter_map(|(_, p)| p.as_ref()).collect();
                                payloads.len() >= 2
                                    && payloads[1..].iter().any(|t| *t != payloads[0])
                            }).unwrap_or(false)
                        });
                        if is_mixed {
                            let variant_name = ENUM_VARIANT_PAYLOADS_REGISTRY.with(|r| {
                                r.borrow().get(enum_n).and_then(|v| {
                                    v.get(arm.tag as usize).map(|(n, _)| n.clone())
                                })
                            }).unwrap_or_else(|| format!("tag{}", arm.tag));
                            format!("__scr.u.{}", enum_variant_member(&variant_name))
                        } else {
                            "__scr.payload".to_string()
                        }
                    } else {
                        "__scr.payload".to_string()
                    };
                    format!(
                        "{{ {} v_{} = {}; __r = ({}); }}",
                        c_type_name(bty),
                        bname,
                        payload_access,
                        body_v
                    )
                } else {
                    let body_v = emit_expr(&arm.body);
                    format!("__r = ({});", body_v)
                };
                if let Some(int_v) = arm.int_value {
                    body.push_str(&format!(
                        "case {}: {} break; ",
                        int_v, arm_block
                    ));
                } else {
                    body.push_str(&format!(
                        "case {}: {} break; ",
                        arm.tag, arm_block
                    ));
                }
            }
            match wildcard_body {
                Some(w) => body.push_str(&format!("default: __r = ({}); break; ", w)),
                None => body.push_str("default: abort(); "),
            }
            body.push_str("} __r; ");
            format!("({{ {}}})", body)
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            // Plain C ternary — branches are always single
            // expressions so this is unambiguous. T4
            // (if-as-expression).
            let c = emit_expr(cond);
            let t = emit_expr(then_value);
            let e = emit_expr(else_value);
            format!("(({}) ? ({}) : ({}))", c, t, e)
        }
        TypedExprKind::Block { stmts, tail } => {
            // GCC statement-expression form: `({ T a = e1;
            // T b = e2; print …; tail; })`. The tail's value
            // is the last evaluated sub-expression. T-block.
            // V1 admits Let + Print stmts; the checker rejects
            // anything else from user-written blocks. Synthetic
            // blocks (e.g. the `match-str` desugar) can also
            // include `Drop` stmts so the temp scrutinee gets
            // released after the if-chain evaluates. Closure
            // #137.
            let mut body = String::from("({ ");
            for s in stmts {
                match s {
                    TypedStmt::Let { name, ty, expr: rhs } => {
                        body.push_str(&format!(
                            "{} v_{} = ({}); ",
                            c_type_name(ty),
                            name,
                            emit_expr(rhs)
                        ));
                    }
                    TypedStmt::Print { items } => {
                        emit_print_items(items, &mut body);
                    }
                    TypedStmt::Reassign { name, expr: rhs, .. } => {
                        // Block-expr Reassign: simple stored-
                        // value update. Mirrors the stmt-level
                        // Reassign emit for the trivial case.
                        body.push_str(&format!(
                            "v_{} = ({}); ",
                            name,
                            emit_expr(rhs),
                        ));
                    }
                    TypedStmt::While { cond, body: while_body } => {
                        // Block-expr While loop (closure #238).
                        // Emit `while (cond) { body }` inside the
                        // GCC statement-expression. Inner body
                        // currently restricted to Assign / Print
                        // by the Block-expr checker, both of
                        // which round-trip through the top-level
                        // stmt emitter cleanly.
                        body.push_str(&format!("while (({})) {{ ", emit_expr(cond)));
                        for inner in while_body {
                            match inner {
                                TypedStmt::Reassign { name, expr: rhs, .. } => {
                                    body.push_str(&format!(
                                        "v_{} = ({}); ",
                                        name,
                                        emit_expr(rhs),
                                    ));
                                }
                                TypedStmt::Print { items } => {
                                    emit_print_items(items, &mut body);
                                }
                                _ => {
                                    body.push_str("/* unsupported while-body stmt */ ");
                                }
                            }
                        }
                        body.push_str("} ");
                    }
                    TypedStmt::Discard { expr: discard_expr } => {
                        // Closure #200: `let _ = expr;` inside
                        // a Block-expr. Evaluate the RHS for
                        // side effects and free any heap result.
                        // Brace-scope each so consecutive
                        // discards don't collide on the tmp
                        // name. Mirrors the regular stmt-level
                        // discard handling (closures #134, #145,
                        // #146).
                        let rhs_c = emit_expr(discard_expr);
                        match &discard_expr.ty {
                            Type::OwnedStr => {
                                body.push_str(&format!(
                                    "{{ char* _intent_discard = ({}); free((void*)_intent_discard); }} ",
                                    rhs_c
                                ));
                            }
                            Type::Vec(element) => {
                                let s_name = vec_c_struct(element);
                                body.push_str(&format!(
                                    "{{ {} _intent_discard = ({}); {}(_intent_discard); }} ",
                                    s_name,
                                    rhs_c,
                                    vec_helper(element, "free"),
                                ));
                            }
                            Type::Struct(struct_name) => {
                                let fields = STRUCT_FIELDS_REGISTRY
                                    .with(|r| r.borrow().get(struct_name).cloned())
                                    .unwrap_or_default();
                                let has_owning =
                                    fields.iter().any(|(_, ty)| !ty.is_copy());
                                if has_owning {
                                    let mut field_drops = String::new();
                                    let empty: std::collections::HashSet<&String> =
                                        std::collections::HashSet::new();
                                    emit_struct_field_drops(
                                        "_intent_discard",
                                        struct_name,
                                        &fields,
                                        &empty,
                                        &mut field_drops,
                                    );
                                    body.push_str(&format!(
                                        "{{ {} _intent_discard = ({}); {}}} ",
                                        struct_c_name(struct_name),
                                        rhs_c,
                                        field_drops,
                                    ));
                                } else {
                                    body.push_str(&format!("(void)({}); ", rhs_c));
                                }
                            }
                            Type::Enum(enum_name) => {
                                let payload_ty = ENUM_PAYLOAD_REGISTRY
                                    .with(|r| r.borrow().get(enum_name).cloned());
                                let free_expr: Option<String> = match &payload_ty {
                                    Some(Type::OwnedStr) => Some(
                                        "free((void*)_intent_discard.payload)".to_string(),
                                    ),
                                    Some(Type::Vec(element)) => Some(format!(
                                        "{}(_intent_discard.payload)",
                                        vec_helper(element, "free")
                                    )),
                                    _ => None,
                                };
                                if let Some(free_call) = free_expr {
                                    let payload_tags: Vec<u32> = ENUM_PAYLOAD_TAGS_REGISTRY
                                        .with(|r| {
                                            r.borrow()
                                                .get(enum_name)
                                                .cloned()
                                                .unwrap_or_default()
                                        });
                                    if !payload_tags.is_empty() {
                                        let cases: Vec<String> = payload_tags
                                            .iter()
                                            .map(|t| format!("case {}", t))
                                            .collect();
                                        body.push_str(&format!(
                                            "{{ {} _intent_discard = ({}); switch (_intent_discard.tag) {{ {}: {}; break; default: break; }} }} ",
                                            format!("Enum_{}", enum_name),
                                            rhs_c,
                                            cases.join(": "),
                                            free_call,
                                        ));
                                    } else {
                                        body.push_str(&format!("(void)({}); ", rhs_c));
                                    }
                                } else {
                                    body.push_str(&format!("(void)({}); ", rhs_c));
                                }
                            }
                            _ => {
                                // Copy / scalar / non-heap discards:
                                // just evaluate for side effects.
                                body.push_str(&format!("(void)({}); ", rhs_c));
                            }
                        }
                    }
                    TypedStmt::Drop { name, ty, .. } => match ty {
                        Type::OwnedStr => {
                            body.push_str(&format!(
                                "free((void*){}); ",
                                local_name(name)
                            ));
                        }
                        Type::Vec(element) => {
                            body.push_str(&format!(
                                "{}({}); ",
                                vec_helper(element, "free"),
                                local_name(name)
                            ));
                        }
                        Type::Struct(struct_name) => {
                            // Closure #192: Block-expr Drop
                            // for a struct binding with
                            // heap-owning fields. The
                            // inject_branch_drops machinery
                            // (closure #179) wraps if-expr /
                            // match arms with Drops for the
                            // OTHER branches' Vars. For
                            // Struct-typed Vars the per-field
                            // free chain has to run; previously
                            // this arm fell through to `_ =>
                            // {}` and the unchosen branch's
                            // heap leaked.
                            let fields = STRUCT_FIELDS_REGISTRY
                                .with(|r| r.borrow().get(struct_name).cloned())
                                .unwrap_or_default();
                            // Closure #207: if the struct has a
                            // user-declared `implement Drop for T`
                            // AND no owning fields, the auto-
                            // call invokes the user's drop method.
                            // Mirrors the regular stmt-level
                            // Struct Drop arm (lines 1965-1987).
                            // Without this, a Block-expr inner
                            // Let of a Copy-but-user-Drop struct
                            // (e.g. `Resource` with only an
                            // i64 field plus `implement Drop`)
                            // silently skipped the user drop at
                            // scope exit.
                            let has_user_drop = USER_DROP_REGISTRY
                                .with(|r| r.borrow().contains(struct_name));
                            let has_owning_field = fields.iter().any(|(_, ty)| {
                                matches!(ty, Type::OwnedStr | Type::Vec(_))
                            });
                            if has_user_drop && !has_owning_field {
                                body.push_str(&format!(
                                    "(void){}({}); ",
                                    function_name(&format!("{}_drop", struct_name)),
                                    local_name(name),
                                ));
                                continue;
                            }
                            let empty: std::collections::HashSet<&String> =
                                std::collections::HashSet::new();
                            // emit_struct_field_drops appends
                            // to `out` with `  ` indent + `\n`
                            // suffix per line. Inside the
                            // statement-expression we strip
                            // newlines so it stays one inline
                            // sequence.
                            let mut tmp = String::new();
                            emit_struct_field_drops(
                                &local_name(name),
                                struct_name,
                                &fields,
                                &empty,
                                &mut tmp,
                            );
                            for line in tmp.lines() {
                                let trimmed = line.trim_start();
                                if !trimmed.is_empty() {
                                    body.push_str(trimmed);
                                    body.push(' ');
                                }
                            }
                        }
                        Type::Enum(enum_name) => {
                            // Closure #193: parallel to the
                            // Struct arm. Block-expr Drop for
                            // a payloaded enum needs to switch
                            // on the active tag and free the
                            // heap payload — otherwise the
                            // unchosen branch's payload leaks.
                            let payload_ty = ENUM_PAYLOAD_REGISTRY
                                .with(|r| r.borrow().get(enum_name).cloned());
                            let free_expr: Option<String> = match &payload_ty {
                                Some(Type::OwnedStr) => Some(format!(
                                    "free((void*){}.payload)",
                                    local_name(name)
                                )),
                                Some(Type::Vec(element)) => Some(format!(
                                    "{}({}.payload)",
                                    vec_helper(element, "free"),
                                    local_name(name)
                                )),
                                _ => None,
                            };
                            if let Some(free_call) = free_expr {
                                let payload_tags: Vec<u32> =
                                    ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
                                        r.borrow()
                                            .get(enum_name)
                                            .cloned()
                                            .unwrap_or_default()
                                    });
                                if !payload_tags.is_empty() {
                                    let cases: Vec<String> = payload_tags
                                        .iter()
                                        .map(|t| format!("case {}", t))
                                        .collect();
                                    body.push_str(&format!(
                                        "switch ({}.tag) {{ {}: {}; break; default: break; }} ",
                                        local_name(name),
                                        cases.join(": "),
                                        free_call,
                                    ));
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            body.push_str(&format!("({}); }})", emit_expr(tail)));
            body
        }
        TypedExprKind::DynDispatch {
            receiver, iface_name: _, method: _, slot_index, args, ..
        } => {
            // Vtables Phase 3 (tree-C) + 4c: dispatch through
            // the fat pointer's vtable slot. For an owned
            // receiver `(recv).vtable->m<slot>((recv).data)`.
            // For a borrowed receiver (`ref dyn Iface` /
            // `mut ref dyn Iface`) the C value is a pointer
            // to the fat pointer, so deref via `(*recv)` first.
            let recv_c = emit_expr(receiver);
            let recv_lval = match receiver.ty {
                Type::Ref(_) | Type::RefMut(_) => format!("(*({}))", recv_c),
                _ => format!("({})", recv_c),
            };
            let mut arg_parts: Vec<String> = Vec::with_capacity(args.len() + 1);
            arg_parts.push(format!("{}.data", recv_lval));
            for a in args {
                arg_parts.push(emit_expr(a));
            }
            format!(
                "({}.vtable->m{}({}))",
                recv_lval, slot_index, arg_parts.join(", ")
            )
        }
        TypedExprKind::DynCoerce { value, iface_name, from_type_name, from_ty: _ } => {
            // Vtables Phase 3 (tree-C): materialize the fat
            // pointer literal. The data slot must hold the
            // address of a stable lvalue. Var sources point at
            // the binding's stack address (lives for the
            // enclosing block). Non-Var sources are pre-hoisted
            // into a synthetic let by the checker pass — by
            // the time codegen runs, every DynCoerce should
            // have a Var source. Closure #276.
            match &value.kind {
                TypedExprKind::Var(name) => {
                    format!(
                        "((intent_dyn_{iface}){{ .vtable = &intent_vtbl_{iface}_{ty}, .data = (void*)&{lvalue} }})",
                        iface = iface_name,
                        ty = from_type_name,
                        lvalue = local_name(name),
                    )
                }
                _ => unreachable!(
                    "DynCoerce non-Var source reached codegen; the checker's \
                     synthetic-let hoist should have rewritten it. iface={}",
                    iface_name
                ),
            }
        }
    }
}

/// Per-shape C struct name for a tuple type. Mirrors
/// `vec_c_struct` — the elements' tags get concatenated
/// with `_` so distinct shapes never collide. T1.1.
pub(crate) fn tuple_c_struct(elements: &[Type]) -> String {
    let tags: Vec<String> = elements.iter().map(element_tag).collect();
    format!("intent_tuple_{}", tags.join("_"))
}

/// Emit the typedef for a tuple shape (`typedef struct { … }
/// intent_tuple_<shape>;`). Each element becomes a numbered
/// field `_0`, `_1`, … so `.0` / `.1` access in source
/// lowers to `._0` / `._1`. Called from the preamble after
/// `emit_array_typedefs_for` so any nested array element
/// typedefs are already in scope.
pub(crate) fn emit_tuple_bundle(elements: &[Type], out: &mut String) {
    let struct_name = tuple_c_struct(elements);
    out.push_str(&format!("typedef struct {{\n"));
    for (i, ty) in elements.iter().enumerate() {
        let storage = c_element_storage(ty);
        out.push_str(&format!("    {} _{};\n", storage, i));
    }
    out.push_str(&format!("}} {};\n", struct_name));
}

fn emit_call(name: &str, args: &[TypedExpr], result_ty: &Type) -> String {
    match name {
        "min" => {
            // Inline ternary. Operands are evaluated once each
            // (no fresh stmt-emit machinery available here), so a
            // side-effecting subexpression would run twice. The
            // effects checker rejects impure operands in pure-fn
            // / parallel-for bodies, which is where reduction
            // bodies live — so this restriction is invisible to
            // users today.
            let a = emit_expr(&args[0]);
            let b = emit_expr(&args[1]);
            return format!("(({}) < ({}) ? ({}) : ({}))", a, b, a, b);
        }
        "max" => {
            let a = emit_expr(&args[0]);
            let b = emit_expr(&args[1]);
            return format!("(({}) > ({}) ? ({}) : ({}))", a, b, a, b);
        }
        // Atomic builtins. Each call lowers to a single
        // C11 `<stdatomic.h>` operation with seq_cst memory
        // order. Element type T is recovered from the call's
        // typed arguments: `atomic_new` uses the result_ty
        // (`Atomic<T>`); the others read T off the value
        // argument's type (args[1]) since the checker has
        // already coerced it to T. The cell argument lowers
        // to `_Atomic <c_ty>*` per `format_declarator`.
        "atomic_new" => {
            return format!("({})", emit_expr(&args[0]));
        }
        "atomic_load" => {
            return format!(
                "atomic_load_explicit({}, memory_order_seq_cst)",
                emit_expr(&args[0])
            );
        }
        "atomic_store" => {
            let cell = emit_expr(&args[0]);
            let v = emit_expr(&args[1]);
            let elt_c = c_leaf_type(&args[1].ty);
            // C11 atomic_store_explicit returns void. Wrap in
            // a GNU/C statement-expression so the call site can
            // still consume a value of element type T (we
            // return the value that was stored).
            return format!(
                "({{ {elt} __v = ({v}); atomic_store_explicit({cell}, __v, memory_order_seq_cst); __v; }})",
                elt = elt_c,
                v = v,
                cell = cell
            );
        }
        "atomic_fetch_add" => {
            return format!(
                "atomic_fetch_add_explicit({}, {}, memory_order_seq_cst)",
                emit_expr(&args[0]),
                emit_expr(&args[1])
            );
        }
        "atomic_compare_exchange" => {
            // C11's `atomic_compare_exchange_*_explicit` takes a
            // pointer to the expected value (which it writes the
            // observed value into on failure). Wrap in a GNU
            // statement-expression so the call site sees a
            // single bool result without exposing the
            // intermediate.
            let cell = emit_expr(&args[0]);
            let exp = emit_expr(&args[1]);
            let new = emit_expr(&args[2]);
            let elt_c = c_leaf_type(&args[1].ty);
            return format!(
                "({{ {elt} __cas_exp = ({exp}); atomic_compare_exchange_strong_explicit({cell}, &__cas_exp, ({new}), memory_order_seq_cst, memory_order_seq_cst); }})",
                elt = elt_c,
                exp = exp,
                cell = cell,
                new = new
            );
        }
        "channel_new" => {
            // The result type carries (T, N); dispatch to the
            // matching per-(T, N) helper.
            let (element, capacity) = match result_ty {
                Type::Channel(elt, cap) => (elt.as_ref().clone(), *cap),
                _ => unreachable!("channel_new must return Channel<T, N>"),
            };
            return format!("{}()", c_channel_helper(&element, capacity, "new"));
        }
        "channel_send" => {
            // args[0] is `&Channel<T, N>` / `&mut Channel<T, N>`.
            // Recover (T, N) from its type, dispatch.
            let (element, capacity) = channel_inner_from_ref(&args[0].ty);
            return format!(
                "{}({}, {})",
                c_channel_helper(&element, capacity, "send"),
                emit_expr(&args[0]),
                emit_expr(&args[1])
            );
        }
        "channel_recv" => {
            let (element, capacity) = channel_inner_from_ref(&args[0].ty);
            return format!(
                "{}({})",
                c_channel_helper(&element, capacity, "recv"),
                emit_expr(&args[0])
            );
        }
        "mutex_new" => {
            return format!("intent_mutex_i64_new({})", emit_expr(&args[0]));
        }
        "mutex_lock" => {
            return format!("intent_mutex_i64_lock({})", emit_expr(&args[0]));
        }
        "guard_get" => {
            return format!("intent_guard_i64_get({})", emit_expr(&args[0]));
        }
        "guard_set" => {
            return format!(
                "intent_guard_i64_set({}, {})",
                emit_expr(&args[0]),
                emit_expr(&args[1])
            );
        }
        "try_vec" => {
            // Closure #284: try_vec(n) -> Result<Vec<i64>,
            // AllocError>. Emits a GCC statement-expression
            // that mallocs an i64-buffer with null-check and
            // builds the Result tagged union. result_ty is
            // the mangled `Type::Enum("Result__<Vec...>__AllocError")`.
            let result_enum = match result_ty {
                Type::Enum(n) => n.clone(),
                _ => unreachable!("try_vec must return Type::Enum(Result__...)"),
            };
            let n_expr = emit_expr(&args[0]);
            let vec_struct = vec_c_struct(&Type::I64);
            let result_c = enum_c_name(&result_enum);
            // Variant tags: Ok=0, Err=1 (declaration order in
            // `enum Result<T, E> { Ok(T), Err(E) }`).
            // Note: AllocError is a payload-less enum →
            // lowers to `int32_t` (no Enum_AllocError
            // typedef). The Err variant just gets the tag
            // (0 = OutOfMemory in declaration order).
            return format!(
                "({{ \
                  uint64_t __try_vec_n = ({n}); \
                  int64_t* __try_vec_data = (int64_t*)malloc((__try_vec_n == 0 ? 1 : __try_vec_n) * sizeof(int64_t)); \
                  {result} __try_vec_r; \
                  if (__try_vec_data == NULL) {{ \
                    __try_vec_r.tag = 1; \
                    __try_vec_r.u.v_Err = (int32_t)0; \
                  }} else {{ \
                    {vs} __try_vec_v; \
                    __try_vec_v.data = __try_vec_data; \
                    __try_vec_v.len = 0; \
                    __try_vec_v.capacity = __try_vec_n == 0 ? 1 : __try_vec_n; \
                    __try_vec_r.tag = 0; \
                    __try_vec_r.u.v_Ok = __try_vec_v; \
                  }} \
                  __try_vec_r; \
                }})",
                n = n_expr,
                result = result_c,
                vs = vec_struct,
            );
        }
        "vec" => {
            let element = match result_ty {
                Type::Vec(element) => element,
                _ => unreachable!("vec() must return Vec<_>"),
            };
            // Use the element's storage spelling (handles
            // `Vec<U>` aggregates as `intent_vec_<U>`).
            // `c_leaf_type` was right for scalars but emits
            // `"/* vec */"` placeholders for nested Vecs.
            let c_element = c_element_storage(element);
            // For Array elements: C forbids initializing one
            // array from a compound-literal-as-rvalue (gcc:
            // "array initialized from non-constant array
            // expression"). The vec-emit normally turns
            // ArrayLit args into `((int64_t[4]){...})`
            // compound literals via `emit_expr`; for the
            // outer brace-list of a `(intent_arr4_int64_t[N]){...}`
            // initializer we need plain `{...}` so the outer
            // array directly initializes from braced
            // element-lists. Strip the cast for ArrayLit
            // args when this is the case. Refines #7 phase 2c.
            let element_is_array = matches!(element.as_ref(), Type::Array { .. });
            let parts: Vec<String> = args
                .iter()
                .map(|a| {
                    if element_is_array {
                        if let TypedExprKind::ArrayLit { elements } = &a.kind {
                            let inner: Vec<String> =
                                elements.iter().map(emit_expr).collect();
                            return format!("{{ {} }}", inner.join(", "));
                        }
                    }
                    emit_expr(a)
                })
                .collect();
            // C99 forbids zero-length array literals, so the
            // empty-vec case (e.g. `let xs: Vec<i64> = vec();`
            // — #8 from STATUS.md) passes NULL through the
            // `__from(0, NULL)` shape. The runtime helper
            // already special-cases `n == 0` and skips the
            // memcpy.
            if parts.is_empty() {
                format!(
                    "{}(0, (const {}*)0)",
                    vec_helper(element, "from"),
                    c_element
                )
            } else {
                let array_literal = format!(
                    "({}[{}]){{ {} }}",
                    c_element,
                    parts.len(),
                    parts.join(", ")
                );
                format!(
                    "{}({}, (const {}*){})",
                    vec_helper(element, "from"),
                    parts.len(),
                    c_element,
                    array_literal
                )
            }
        }
        "push" => {
            let element = match result_ty {
                Type::Vec(element) => element,
                _ => unreachable!("push() must return Vec<_>"),
            };
            format!(
                "{}({}, {})",
                vec_helper(element, "push"),
                emit_expr(&args[0]),
                emit_expr(&args[1])
            )
        }
        "push_mut" => {
            // In-place push: first arg is `mut ref Vec<T>`,
            // which lowers to a pointer to the Vec struct.
            // Element type comes from peeking through the ref.
            let element = match args[0].ty.deref() {
                Type::Vec(element) => element.clone(),
                _ => unreachable!("push_mut() arg 0 must be (mut ref) Vec<_>"),
            };
            format!(
                "{}({}, {})",
                vec_helper(&element, "push_mut"),
                emit_expr(&args[0]),
                emit_expr(&args[1])
            )
        }
        "pop" => {
            // In-place pop: first arg is `mut ref Vec<T>`.
            // The helper aborts on empty, otherwise decrements
            // `len` and returns the element (by-move for
            // non-Copy elements). Closure #219.
            let element = match args[0].ty.deref() {
                Type::Vec(element) => element.clone(),
                _ => unreachable!("pop() arg 0 must be (mut ref) Vec<_>"),
            };
            format!(
                "{}({})",
                vec_helper(&element, "pop_mut"),
                emit_expr(&args[0]),
            )
        }
        "set" => {
            let element = match result_ty {
                Type::Vec(element) => element,
                _ => unreachable!("set() must return Vec<_>"),
            };
            format!(
                "{}({}, (uint64_t)({}), {})",
                vec_helper(element, "set"),
                emit_expr(&args[0]),
                emit_expr(&args[1]),
                emit_expr(&args[2])
            )
        }
        "clone" => {
            let element = match result_ty {
                Type::Vec(element) => element,
                _ => unreachable!("clone() must return Vec<_>"),
            };
            format!(
                "{}({})",
                vec_helper(element, "clone"),
                emit_expr(&args[0])
            )
        }
        "clone_at" => {
            // `clone_at(xs, i)`: return a deep copy of slot i.
            // For Copy elements the raw slot value is itself
            // a fresh independent copy (memcpy semantics).
            // For Vec<U> elements we call the inner's __clone
            // so the result owns its own buffer — refines #7
            // phase 2d. Source operand may be `Vec<T>` or
            // `&Vec<T>` / `&mut Vec<T>`; collection_expr
            // figures out the actual storage spelling so the
            // emitted access (`v.data[i]` vs `v->data[i]`)
            // is well-formed.
            let xs_arg = &args[0];
            let underlying = xs_arg.ty.deref();
            // Closure #291: `clone_at(ref [T; N], i)` accepts
            // arrays alongside Vec. Arrays index directly as
            // `xs[i]` (C array decay); Vec uses `.data[i]`.
            let element_ty = match underlying {
                Type::Vec(element) => &**element,
                Type::Array { element, .. } => &**element,
                other => {
                    unreachable!("clone_at requires Vec or Array, got {:?}", other)
                }
            };
            let is_array = matches!(underlying, Type::Array { .. });
            let xs_str = emit_expr(xs_arg);
            let access_via_ref = matches!(
                &xs_arg.ty,
                Type::Ref(_) | Type::RefMut(_)
            );
            // Wrap xs_str in parens so `&xs->data[i]`
            // parses as `(&xs)->data[i]` — `->` binds
            // tighter than unary `&` so naked
            // concatenation breaks.
            let slot = if is_array {
                // C array indexing: arrays decay to T* so
                // both `xs[i]` (value) and `xs[i]` (ref —
                // ref-of-array passes the decayed pointer)
                // index the same way. No `*` indirection
                // needed.
                format!("({})[{}]", xs_str, emit_expr(&args[1]))
            } else if access_via_ref {
                format!("({})->data[{}]", xs_str, emit_expr(&args[1]))
            } else {
                format!("({}).data[{}]", xs_str, emit_expr(&args[1]))
            };
            // Element-aware deep-clone: recurse through
            // `c_element_deep_clone` so a `Vec<Vec<U>>` slot
            // routes through the inner Vec's __clone helper.
            // For Copy elements the helper returns the slot
            // unchanged (memcpy semantics).
            c_element_deep_clone(&slot, element_ty)
        }
        _ => {
            let rendered_args = args.iter().map(emit_expr).collect::<Vec<_>>().join(", ");
            // Closure #269: extern "C" fns emit a bare C-ABI
            // call (no `fn_` prefix). The C_EXTERN_FN_REGISTRY
            // gets populated at backend entry from the
            // program's extern fn list.
            let is_extern = C_EXTERN_FN_REGISTRY
                .with(|r| r.borrow().contains(name));
            let symbol = if is_extern {
                name.to_string()
            } else {
                function_name(name)
            };
            format!("{}({})", symbol, rendered_args)
        }
    }
}

fn emit_index(array: &TypedExpr, index: &TypedExpr, checked: bool) -> String {
    let index_str = emit_expr(index);
    let array_str = emit_expr(array);
    // For Ref/RefMut types, C array decay handles arrays automatically; Vec needs explicit (*ptr).
    let (underlying, is_ref) = match &array.ty {
        Type::Ref(inner) | Type::RefMut(inner) => (&**inner, true),
        other => (other, false),
    };
    match underlying {
        Type::Array { length, .. } => {
            if checked {
                format!(
                    "({}[intent_check_bounds((uint64_t)({}), {})])",
                    array_str, index_str, length
                )
            } else {
                format!("({}[{}])", array_str, index_str)
            }
        }
        Type::Vec(element) => {
            // Fresh-Vec operand: bind to a brace-scoped tmp,
            // read .data[i], then free the buffer via
            // `intent_vec_<T>__free`. Without this the heap
            // leaks. Var / FieldAccess Vec operands keep the
            // simple form — binding owns the buffer. Closure
            // #142.
            if !is_ref && crate::ir::is_fresh_non_copy(array) {
                let struct_name = vec_c_struct(element);
                let free_helper = vec_helper(element, "free");
                let elem_storage = c_element_storage(element);
                if checked {
                    return format!(
                        "(({{ {sn} _intent_idx_tmp = ({arr}); {es} _intent_idx_r = _intent_idx_tmp.data[intent_check_bounds((uint64_t)({idx}), _intent_idx_tmp.len)]; {fh}(_intent_idx_tmp); _intent_idx_r; }}))",
                        sn = struct_name,
                        arr = array_str,
                        es = elem_storage,
                        idx = index_str,
                        fh = free_helper
                    );
                } else {
                    return format!(
                        "(({{ {sn} _intent_idx_tmp = ({arr}); {es} _intent_idx_r = _intent_idx_tmp.data[(uint64_t)({idx})]; {fh}(_intent_idx_tmp); _intent_idx_r; }}))",
                        sn = struct_name,
                        arr = array_str,
                        es = elem_storage,
                        idx = index_str,
                        fh = free_helper
                    );
                }
            }
            let prefix = if is_ref {
                format!("(*{})", array_str)
            } else {
                array_str.clone()
            };
            if checked {
                format!(
                    "({}.data[intent_check_bounds((uint64_t)({}), {}.len)])",
                    prefix, index_str, prefix
                )
            } else {
                format!("({}.data[(uint64_t)({})])", prefix, index_str)
            }
        }
        _ => format!("({}[{}])", array_str, index_str),
    }
}

fn emit_len(array: &TypedExpr, static_length: u64) -> String {
    let (underlying, is_ref) = match &array.ty {
        Type::Ref(inner) | Type::RefMut(inner) => (&**inner, true),
        other => (other, false),
    };
    match underlying {
        Type::Array { .. } => format!("((uint64_t){})", static_length),
        Type::Vec(element) => {
            // Fresh-Vec operand: bind to a brace-scoped tmp,
            // read .len, then free the buffer via the
            // matching `intent_vec_<T>__free` helper. Var /
            // FieldAccess Vec operands keep the simple
            // `(v.len)` form — binding owns the buffer.
            // Closure #141.
            let array_str = emit_expr(array);
            if !is_ref && crate::ir::is_fresh_non_copy(array) {
                let struct_name = vec_c_struct(element);
                let free_helper = vec_helper(element, "free");
                return format!(
                    "((uint64_t)({{ {sn} _intent_len_tmp = ({arr}); uint64_t _intent_len_r = _intent_len_tmp.len; {fh}(_intent_len_tmp); _intent_len_r; }}))",
                    sn = struct_name,
                    arr = array_str,
                    fh = free_helper
                );
            }
            if is_ref {
                format!("((*{}).len)", array_str)
            } else {
                format!("({}.len)", array_str)
            }
        }
        Type::Str | Type::OwnedStr => {
            // Fresh OwnedStr operand: free the heap after
            // strlen via a GCC statement-expression. Var /
            // FieldAccess / Str operands stay non-consuming.
            // Closure #140.
            //
            // Closure #262: when the operand is a borrow (Ref
            // / RefMut), the C expression has type `char**` /
            // `const char**` — strlen wants the inner pointer.
            // Dereference once with `(*expr)`. Without this,
            // `len(ref s)` for `s: OwnedStr` returned
            // `strlen(&s)` which read junk from the pointer's
            // own bytes (≈ 6 on x86-64 little-endian).
            let arg_str = emit_expr(array);
            let arg_expr = if is_ref {
                format!("(*{})", arg_str)
            } else {
                arg_str
            };
            if crate::ir::is_fresh_owned_str(array) {
                format!(
                    "((uint64_t)({{ char* _intent_len_tmp = ({}); uint64_t _intent_len_r = (uint64_t)strlen(_intent_len_tmp); free((void*)_intent_len_tmp); _intent_len_r; }}))",
                    arg_expr
                )
            } else {
                format!("((uint64_t)strlen({}))", arg_expr)
            }
        }
        _ => format!("((uint64_t){})", static_length),
    }
}

fn emit_binary(
    op: BinaryOp,
    left: &TypedExpr,
    right: &TypedExpr,
    checked: bool,
    _result_type: &Type,
) -> String {
    // Str/OwnedStr concat: `a + b` → an inline call to a runtime
    // helper that mallocs a fresh buffer of size strlen(a) +
    // strlen(b) + 1, copies both, and returns the new pointer.
    // OwnedStr operands are consumed (their backing buffer is
    // freed by the helper before it returns the new buffer);
    // the checker has already marked the underlying bindings
    // as moved so they can't be used afterward. The owned
    // flag uses the same conservative whitelist as the rest
    // of the fresh-OwnedStr handlers (Call / Binary / Block
    // / IfExpr / Match) — Var / FieldAccess operands share
    // their buffer with a live binding and would double-free
    // at the binding's scope-exit Drop. Closure #144 widened
    // the previous `matches!(ty, OwnedStr)` check that
    // double-freed `t.name + "x"` (FieldAccess + Str).
    if matches!(op, BinaryOp::Add)
        && matches!(left.ty, Type::Str | Type::OwnedStr)
        && matches!(right.ty, Type::Str | Type::OwnedStr)
    {
        let lhs_owned = crate::ir::owned_str_consumed_at_concat(left);
        let rhs_owned = crate::ir::owned_str_consumed_at_concat(right);
        return format!(
            "intent_str_concat({}, {}, {}, {})",
            emit_expr(left),
            if lhs_owned { 1 } else { 0 },
            emit_expr(right),
            if rhs_owned { 1 } else { 0 },
        );
    }
    // Str/OwnedStr comparisons lower to strcmp(a, b) <op> 0 instead
    // of pointer comparison. Either type is accepted in either
    // position — strcmp only reads, so OwnedStr is auto-borrowed.
    if matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    ) && matches!(left.ty, Type::Str | Type::OwnedStr)
      && matches!(right.ty, Type::Str | Type::OwnedStr)
    {
        let cmp = match op {
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            _ => unreachable!(),
        };
        // Fresh-OwnedStr operands need a free after strcmp;
        // bind to brace-scoped tmps so the values stay alive
        // for the compare and get released after. Closure #140.
        let l_fresh = crate::ir::is_fresh_owned_str(left);
        let r_fresh = crate::ir::is_fresh_owned_str(right);
        if l_fresh || r_fresh {
            let mut body = String::from("({ ");
            body.push_str(&format!("const char* _intent_cmp_l = ({}); ", emit_expr(left)));
            body.push_str(&format!("const char* _intent_cmp_r = ({}); ", emit_expr(right)));
            body.push_str(&format!(
                "bool _intent_cmp_r_b = (strcmp(_intent_cmp_l, _intent_cmp_r) {} 0); ",
                cmp
            ));
            if l_fresh {
                body.push_str("free((void*)_intent_cmp_l); ");
            }
            if r_fresh {
                body.push_str("free((void*)_intent_cmp_r); ");
            }
            body.push_str("_intent_cmp_r_b; })");
            return body;
        }
        return format!("(strcmp({}, {}) {} 0)", emit_expr(left), emit_expr(right), cmp);
    }

    let right_expr = match op {
        BinaryOp::Div | BinaryOp::Rem if checked => {
            format!("{}({})", divisor_helper(&right.ty), emit_expr(right))
        }
        BinaryOp::Shl | BinaryOp::Shr if checked => {
            let bits = left.ty.bits().unwrap_or(64);
            format!("{}({}, {})", shift_helper(&right.ty), emit_expr(right), bits)
        }
        _ => emit_expr(right),
    };

    format!("({} {} {})", emit_expr(left), op.display_symbol(), right_expr)
}

fn emit_float_literal(value: f64, ty: &Type) -> String {
    if *ty == Type::F32 {
        format!("{:?}f", value as f32)
    } else {
        format!("{:?}", value)
    }
}

/// C-specific spelling for a leaf type. Used wherever the backend
/// emits a type name into the generated C source. Lives in this
/// module (not in `ast::Type`) so the AST stays backend-agnostic
/// for the upcoming LLVM backend migration.
pub(crate) fn c_leaf_type(ty: &Type) -> &'static str {
    match ty {
        Type::I8 => "int8_t",
        Type::I16 => "int16_t",
        Type::I32 => "int32_t",
        Type::I64 => "int64_t",
        Type::U8 => "uint8_t",
        Type::U16 => "uint16_t",
        Type::U32 => "uint32_t",
        Type::U64 => "uint64_t",
        Type::F32 => "float",
        Type::F64 => "double",
        Type::Bool => "bool",
        Type::Str => "const char*",
        Type::OwnedStr => "char*",
        Type::Array { .. } => "/* array */",
        Type::Vec(_) => "/* vec */",
        Type::Ref(_) => "/* ref */",
        Type::RefMut(_) => "/* ref mut */",
        // `Task` lowers to a small handle struct: the
        // pthread_t plus the heap-allocated ctx pointer so
        // join can free the ctx after pthread_join returns.
        // The typedef sits in the runtime preamble alongside
        // the channel / mutex helpers.
        Type::Task => "intent_task_handle",
        // `Atomic<T>` is parametric over T (integer widths +
        // bool). c_leaf_type cannot synthesize a `String`, so
        // callers that need the storage spelling for a specific
        // atomic call into `c_atomic_storage` instead. The
        // arm below is reachable only from places that look at
        // `Type::Atomic` generically without spelling it
        // (e.g. divisor-helper / shift-helper unreachable
        // arms); returning the i64 form keeps any escapee
        // valid C while a stricter audit would replace it
        // with `unreachable!`.
        Type::Atomic(_) => "_Atomic int64_t",
        // `Channel<T, N>` is parametric over both element
        // width and capacity. c_leaf_type can't synthesize a
        // String for each (T, N) pair; callers that need the
        // storage spelling use `c_channel_storage(element, N)`
        // directly. Hitting this arm means a caller forgot to
        // special-case Channel — fall back to the i64/16 form
        // so output stays valid C rather than panicking, but a
        // stricter audit would `unreachable!`.
        Type::Channel(_, _) => "intent_channel_int64_t_16",
        // `Mutex<T>` storage is a 2-field struct: payload + a
        // CAS-based spin lock. v1: i64 payload only.
        Type::Mutex(_) => "intent_mutex_i64",
        // `Guard<T>` is a thin handle holding a pointer back to
        // its mutex. The scope-exit drop unlocks. v1: i64
        // payload.
        Type::Guard(_) => "intent_guard_i64",
        // `fn(T1, T2) -> R` has no fixed leaf spelling in C —
        // function-pointer types are declarator-shaped
        // (`R (*name)(T1, T2)`). Callers that need to emit a
        // declaration use `format_declarator` which special-
        // cases FnPtr. Hitting this arm means a caller asked
        // for a bare type name where only a declarator would
        // be syntactically valid; return an opaque pointer
        // typedef so the build doesn't break, but a stricter
        // audit would `unreachable!`.
        Type::FnPtr(_, _) => "void*",
        // Tuple `(T1, T2, …)` lowers to a per-shape C struct
        // (`intent_tuple_<tags>`) emitted in the preamble.
        // `c_leaf_type` can't synthesize a `String` so it
        // returns an opaque placeholder; callers that need
        // the storage spelling go through `c_type_name` or
        // `c_element_storage`, both of which know to emit
        // `tuple_c_struct(elements)`. Hitting this arm means
        // a caller treated a Tuple as a leaf — fall back to
        // `void*` so output stays valid C. Refines T1.1.
        Type::Tuple(_) => "/* tuple */",
        // `Struct(name)` lowers to a per-name C struct
        // (`Struct_<name>`) emitted in the preamble. Same
        // routing principle as Tuple: leaf callers get an
        // opaque placeholder; the call sites that need the
        // real spelling go through `c_type_name` /
        // `c_element_storage`. T1.2.
        Type::Struct(_) => "/* struct */",
        Type::Enum(_) => "int32_t",
        // Type params should be substituted before reaching
        // codegen — hitting this arm means a generic call
        // wasn't monomorphized. Fall back to opaque pointer
        // so the build doesn't die; phase 2 will remove. T1.4.
        Type::Param(_) => "void*",
        // Same story for Type::Apply — the monomorphization
        // pass should have replaced every generic
        // instantiation with a concrete mangled
        // Struct/Enum. Closure #281.
        Type::Apply { .. } => "void*",
        // `dyn Iface` is a fat pointer `{ &vtable, &data }`.
        // c_leaf_type can't synthesize the per-Iface typedef
        // name (returns &'static str); callers that need
        // the storage spelling will use a c_object_storage
        // helper added in Phase 3. Hitting this arm means
        // a caller treated `dyn Iface` as a leaf — fall
        // back to a generic two-pointer struct typedef so
        // the build doesn't break. Phase 1.
        Type::Object(_) => "intent_dyn",
    }
}

fn c_type_name(ty: &Type) -> String {
    match ty {
        Type::Vec(element) => vec_c_struct(element),
        // Closure #239: arrays in return-type position are
        // spelled as the struct wrapper `intent_arr_ret_<N>_<T>`.
        // c_type_name is called by emit_prototype +
        // emit_function for the return type and (mostly) by
        // Let stmts for binding storage. The Let path passes
        // through `format_declarator` instead so the array
        // declarator form keeps working for locals.
        Type::Array { element, length } => array_return_struct_name(element, *length),
        Type::Ref(_) | Type::RefMut(_) => {
            unreachable!("reference types do not appear in return positions")
        }
        Type::Atomic(element) => c_atomic_storage(element),
        Type::Channel(element, capacity) => c_channel_storage(element, *capacity),
        Type::Tuple(elements) => tuple_c_struct(elements),
        Type::Object(iface) => format!("intent_dyn_{}", iface),
        Type::Struct(name) => struct_c_name(name),
        // T1.3 phase 2b: payloaded enums lower to the
        // tagged-union struct (`Enum_<Name>`); plain enums
        // stay as bare `int32_t` tags (via the c_leaf_type
        // fallthrough below). The registry is populated at
        // the start of `emit_c`.
        Type::Enum(name) => {
            let payloaded = ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name));
            if payloaded {
                enum_c_name(name)
            } else {
                "int32_t".to_string()
            }
        }
        other => c_leaf_type(other).to_string(),
    }
}

/// Per-name C struct typedef for a user-declared struct.
/// Prefixes with `Struct_` so the emitted C identifier is
/// distinct from any builtin. T1.2.
pub(crate) fn struct_c_name(name: &str) -> String {
    format!("Struct_{}", name)
}

/// Walk a Vec-element type and emit a `typedef` for every
/// struct shape that appears. Per-name emit. T1.2.
pub(crate) fn emit_struct_bundle(
    decl: &crate::ir::TypedStructDecl,
    out: &mut String,
) {
    let cname = struct_c_name(&decl.name);
    out.push_str("typedef struct {\n");
    for (fname, fty) in &decl.fields {
        // `format_declarator` handles arrays natively — `[T;N]`
        // becomes `T fname[N]` so the field is a real C array,
        // not a missing typedef ref. Other field types fall
        // through to their normal storage spelling.
        match fty {
            Type::Array { .. } => {
                out.push_str("    ");
                out.push_str(&format_declarator(fty, fname));
                out.push_str(";\n");
            }
            _ => {
                let storage = c_element_storage(fty);
                out.push_str(&format!("    {} {};\n", storage, fname));
            }
        }
    }
    out.push_str(&format!("}} {};\n", cname));
}

/// Storage type spelling for `Atomic<T>` in declarations:
/// `_Atomic <c_leaf_type(T)>`. The `_Atomic` qualifier guides
/// the C11 atomic ops to use the natural width of T. The
/// element T is restricted by the checker
/// (`is_supported_atomic_element`) to the integer widths plus
/// bool, so `c_leaf_type(element)` always returns a primitive
/// spelling.
fn c_atomic_storage(element: &Type) -> String {
    format!("_Atomic {}", c_leaf_type(element))
}

/// Helper: given `&Channel<T, N>` or `&mut Channel<T, N>`,
/// return `(T, N)`. Panics on shapes the type-checker
/// shouldn't ever produce.
fn channel_inner_from_ref(ty: &Type) -> (Type, u64) {
    match ty {
        Type::Ref(inner) | Type::RefMut(inner) => match inner.as_ref() {
            Type::Channel(elt, cap) => ((**elt).clone(), *cap),
            other => unreachable!("channel ref inner must be Channel<T, N>, got {:?}", other),
        },
        other => unreachable!("channel arg must be &Channel<T, N>, got {:?}", other),
    }
}

fn format_declarator(ty: &Type, name: &str) -> String {
    match ty {
        Type::Array { element, length } => {
            format!("{} {}[{}]", c_leaf_type(element), name, length)
        }
        Type::Vec(element) => format!("{} {}", vec_c_struct(element), name),
        Type::Tuple(elements) => format!("{} {}", tuple_c_struct(elements), name),
        Type::Object(iface) => format!("intent_dyn_{} {}", iface, name),
        Type::Struct(sname) => format!("{} {}", struct_c_name(sname), name),
        // T1.3 phase 2b: payloaded enums lower to the
        // tagged-union struct (Enum_<Name>); plain enums
        // stay as bare int32_t tags (falls through to
        // `c_leaf_type` via `other`).
        Type::Enum(ename) if ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(ename)) => {
            format!("{} {}", enum_c_name(ename), name)
        }
        Type::Ref(inner) => match &**inner {
            Type::Array { element, .. } => format!("const {}* {}", c_leaf_type(element), name),
            Type::Vec(element) => format!("const {}* {}", vec_c_struct(element), name),
            // `&Atomic<T>` drops the `const` qualifier: atomic
            // operations always conceptually mutate the cell;
            // C11 atomics don't model a "read-only borrow" any
            // differently, and a `const _Atomic *` would
            // reject `atomic_store_explicit`.
            Type::Atomic(element) => format!("{}* {}", c_atomic_storage(element), name),
            Type::Channel(element, capacity) => {
                format!("{}* {}", c_channel_storage(element, *capacity), name)
            }
            Type::Tuple(elements) => format!("const {}* {}", tuple_c_struct(elements), name),
            Type::Struct(sname) => format!("const {}* {}", struct_c_name(sname), name),
            // Vtables Phase 4c: `ref dyn Iface` lowers to a
            // pointer to the per-Iface fat pointer typedef.
            Type::Object(iface) => format!("const intent_dyn_{}* {}", iface, name),
            other => format!("const {}* {}", c_leaf_type(other), name),
        },
        Type::RefMut(inner) => match &**inner {
            Type::Array { element, .. } => format!("{}* {}", c_leaf_type(element), name),
            Type::Vec(element) => format!("{}* {}", vec_c_struct(element), name),
            Type::Atomic(element) => format!("{}* {}", c_atomic_storage(element), name),
            Type::Channel(element, capacity) => {
                format!("{}* {}", c_channel_storage(element, *capacity), name)
            }
            Type::Tuple(elements) => format!("{}* {}", tuple_c_struct(elements), name),
            Type::Struct(sname) => format!("{}* {}", struct_c_name(sname), name),
            // Vtables Phase 4c: `mut ref dyn Iface` lowers
            // to a (mutable) pointer to the fat pointer.
            Type::Object(iface) => format!("intent_dyn_{}* {}", iface, name),
            other => format!("{}* {}", c_leaf_type(other), name),
        },
        Type::Atomic(element) => format!("{} {}", c_atomic_storage(element), name),
        Type::Channel(element, capacity) => {
            format!("{} {}", c_channel_storage(element, *capacity), name)
        }
        Type::FnPtr(params, ret) => {
            // C function pointer declarator:
            //   R (*name)(T1, T2, ...)
            // We format each parameter via format_declarator
            // with a synthetic empty name, then collapse the
            // trailing space — keeps array/vec/ref decay
            // happening through the existing machinery.
            let params_c: Vec<String> = params
                .iter()
                .map(|t| {
                    // No parameter name in fn-pointer
                    // declarators; format_declarator expects
                    // one so pass "" and trim. For pure-scalar
                    // params the result is "<ty> " which
                    // trims clean.
                    let s = format_declarator(t, "");
                    s.trim_end().to_string()
                })
                .collect();
            // Closure #216: when the return type is itself a
            // FnPtr, naive declarator nesting (`R (*)(...) (*name)()`)
            // is ill-formed C — the inner fn-ptr declarator
            // can't appear as a prefix-only type. Proper C
            // declarator nesting (`R (*(*name)())(...)`) is
            // syntactically complex to synthesize correctly.
            // Since all fn-ptrs are interchangeable at the C
            // storage level (`void*` in struct fields / Vec
            // slots — closures #214/#215), drop the inner
            // signature and emit `void*` for the return when
            // it's a FnPtr. Call sites handle the implicit
            // conversion (caller's `let f: fn(...) -> R = p();`
            // assigns a `void*` to a fn-pointer-typed local
            // which gcc accepts with an implicit conversion).
            let ret_decl = if matches!(ret.as_ref(), Type::FnPtr(_, _)) {
                "void*".to_string()
            } else {
                let r = format_declarator(ret, "");
                r.trim_end().to_string()
            };
            format!("{} (*{})({})", ret_decl, name, params_c.join(", "))
        }
        other => format!("{} {}", c_leaf_type(other), name),
    }
}

fn emit_runtime_helpers(out: &mut String, body: &str) {
    // Only emit helpers actually called from the body. We previously
    // emitted all of them with INTENT_UNUSED to suppress warnings,
    // but the dead helpers cluttered the generated C. Filtering by a
    // simple substring check on the rendered body keeps the output
    // proportional to what the program actually uses.
    let needs_bounds = body.contains("intent_check_bounds(");
    let divisor_kinds: &[(&str, &str, &str)] = &[
        ("i8", "int8_t", "0"),
        ("i16", "int16_t", "0"),
        ("i32", "int32_t", "0"),
        ("i64", "int64_t", "0"),
        ("u8", "uint8_t", "0"),
        ("u16", "uint16_t", "0"),
        ("u32", "uint32_t", "0"),
        ("u64", "uint64_t", "0"),
        ("f32", "float", "0.0f"),
        ("f64", "double", "0.0"),
    ];
    let shift_kinds: &[(&str, &str, bool)] = &[
        ("i8", "int8_t", true),
        ("i16", "int16_t", true),
        ("i32", "int32_t", true),
        ("i64", "int64_t", true),
        ("u8", "uint8_t", false),
        ("u16", "uint16_t", false),
        ("u32", "uint32_t", false),
        ("u64", "uint64_t", false),
    ];
    let used_divisors: Vec<&(&str, &str, &str)> = divisor_kinds
        .iter()
        .filter(|(ty, _, _)| body.contains(&format!("intent_check_{}_divisor(", ty)))
        .collect();
    let used_shifts: Vec<&(&str, &str, bool)> = shift_kinds
        .iter()
        .filter(|(ty, _, _)| body.contains(&format!("intent_check_{}_shift(", ty)))
        .collect();

    if !needs_bounds && used_divisors.is_empty() && used_shifts.is_empty() {
        return;
    }

    if needs_bounds {
        out.push_str("static INTENT_UNUSED inline uint64_t intent_check_bounds(uint64_t index, uint64_t length) { assert(index < length); return index; }\n");
    }

    for (ty, c_ty, zero) in &used_divisors {
        out.push_str("static INTENT_UNUSED inline ");
        out.push_str(c_ty);
        out.push_str(" intent_check_");
        out.push_str(ty);
        out.push_str("_divisor(");
        out.push_str(c_ty);
        out.push_str(" x) { assert(x != ");
        out.push_str(zero);
        out.push_str("); return x; }\n");
    }

    for (ty, c_ty, signed) in &used_shifts {
        out.push_str("static INTENT_UNUSED inline ");
        out.push_str(c_ty);
        out.push_str(" intent_check_");
        out.push_str(ty);
        out.push_str("_shift(");
        out.push_str(c_ty);
        out.push_str(" x, unsigned bits) { ");
        if *signed {
            out.push_str("assert(x >= 0 && ");
        } else {
            out.push_str("assert(");
        }
        out.push_str("(uint64_t)x < bits); return x; }\n");
    }
    out.push('\n');
}

fn divisor_helper(ty: &Type) -> &'static str {
    match ty {
        Type::I8 => "intent_check_i8_divisor",
        Type::I16 => "intent_check_i16_divisor",
        Type::I32 => "intent_check_i32_divisor",
        Type::I64 => "intent_check_i64_divisor",
        Type::U8 => "intent_check_u8_divisor",
        Type::U16 => "intent_check_u16_divisor",
        Type::U32 => "intent_check_u32_divisor",
        Type::U64 => "intent_check_u64_divisor",
        Type::F32 => "intent_check_f32_divisor",
        Type::F64 => "intent_check_f64_divisor",
        Type::Bool | Type::Str | Type::OwnedStr | Type::Array { .. } | Type::Vec(_) | Type::Ref(_) | Type::RefMut(_) | Type::Task | Type::Atomic(_) | Type::Channel(_, _) | Type::Mutex(_) | Type::Guard(_) | Type::FnPtr(_, _) | Type::Tuple(_) | Type::Struct(_) | Type::Enum(_) | Type::Apply { .. } | Type::Param(_) | Type::Object(_) => {
            unreachable!("non-numeric type cannot be a divisor")
        }
    }
}

fn shift_helper(ty: &Type) -> &'static str {
    match ty {
        Type::I8 => "intent_check_i8_shift",
        Type::I16 => "intent_check_i16_shift",
        Type::I32 => "intent_check_i32_shift",
        Type::I64 => "intent_check_i64_shift",
        Type::U8 => "intent_check_u8_shift",
        Type::U16 => "intent_check_u16_shift",
        Type::U32 => "intent_check_u32_shift",
        Type::U64 => "intent_check_u64_shift",
        Type::F32
        | Type::F64
        | Type::Bool
        | Type::Str
        | Type::OwnedStr
        | Type::Array { .. }
        | Type::Vec(_)
        | Type::Ref(_)
        | Type::RefMut(_)
        | Type::Task
        | Type::Atomic(_)
        | Type::Channel(_, _)
        | Type::Mutex(_)
        | Type::Guard(_)
        | Type::FnPtr(_, _) | Type::Tuple(_) | Type::Struct(_) | Type::Enum(_) | Type::Apply { .. } | Type::Param(_) | Type::Object(_) => unreachable!("shift count must be an integer"),
    }
}

pub(crate) fn function_name(name: &str) -> String {
    format!("fn_{}", sanitize_ident(name))
}

fn local_name(name: &str) -> String {
    format!("v_{}", sanitize_ident(name))
}

fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Escape a string for safe inclusion as a C string literal (already
/// surrounded by `"`s in the emitted code).
fn escape_c_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn escape_comment(text: &str) -> String {
    text.replace("*/", "* /")
}

/// Walk a TypedProgram and collect the set of interface names
/// that are actually used as `dyn Iface` somewhere — either
/// in a function param/return, struct/enum field, local
/// binding, or expression position. Interfaces declared with
/// `interface` + `implement` but never coerced to dyn don't
/// need vtable scaffolding (and would surface trampoline
/// compile errors against unused signatures).
pub(crate) fn collect_used_dyn_ifaces(program: &TypedProgram) -> std::collections::HashSet<String> {
    fn walk_type(ty: &Type, set: &mut std::collections::HashSet<String>) {
        match ty {
            Type::Object(name) => {
                set.insert(name.clone());
            }
            Type::Vec(inner) | Type::Atomic(inner) | Type::Mutex(inner)
            | Type::Guard(inner) | Type::Ref(inner) | Type::RefMut(inner) => walk_type(inner, set),
            Type::Channel(inner, _) => walk_type(inner, set),
            Type::Tuple(elements) => elements.iter().for_each(|t| walk_type(t, set)),
            Type::FnPtr(params, ret) => {
                params.iter().for_each(|t| walk_type(t, set));
                walk_type(ret, set);
            }
            Type::Array { element, .. } => walk_type(element, set),
            _ => {}
        }
    }
    fn walk_expr(expr: &TypedExpr, set: &mut std::collections::HashSet<String>) {
        walk_type(&expr.ty, set);
        match &expr.kind {
            TypedExprKind::Unary { expr, .. } => walk_expr(expr, set),
            TypedExprKind::Binary { left, right, .. } => {
                walk_expr(left, set);
                walk_expr(right, set);
            }
            TypedExprKind::Call { args, .. } | TypedExprKind::ArrayLit { elements: args } => {
                args.iter().for_each(|a| walk_expr(a, set));
            }
            TypedExprKind::Cast { expr, ty } => {
                walk_expr(expr, set);
                walk_type(ty, set);
            }
            TypedExprKind::Index { array, index, .. } => {
                walk_expr(array, set);
                walk_expr(index, set);
            }
            TypedExprKind::Len { array, .. } => walk_expr(array, set),
            TypedExprKind::CallIndirect { callee, args } => {
                walk_expr(callee, set);
                args.iter().for_each(|a| walk_expr(a, set));
            }
            TypedExprKind::Tuple { elements } => elements.iter().for_each(|e| walk_expr(e, set)),
            TypedExprKind::TupleAccess { tuple, .. } => walk_expr(tuple, set),
            TypedExprKind::StructLit { fields, .. } => {
                fields.iter().for_each(|(_, e)| walk_expr(e, set));
            }
            TypedExprKind::FieldAccess { object, .. } => walk_expr(object, set),
            TypedExprKind::EnumVariantWithPayload { payload, payload_ty, .. } => {
                walk_expr(payload, set);
                walk_type(payload_ty, set);
            }
            TypedExprKind::Match { scrutinee, arms } => {
                walk_expr(scrutinee, set);
                arms.iter().for_each(|a| walk_expr(&a.body, set));
            }
            TypedExprKind::IfExpr { cond, then_value, else_value } => {
                walk_expr(cond, set);
                walk_expr(then_value, set);
                walk_expr(else_value, set);
            }
            TypedExprKind::Block { stmts, tail } => {
                stmts.iter().for_each(|s| walk_stmt(s, set));
                walk_expr(tail, set);
            }
            TypedExprKind::DynDispatch { receiver, args, iface_name, .. } => {
                set.insert(iface_name.clone());
                walk_expr(receiver, set);
                args.iter().for_each(|a| walk_expr(a, set));
            }
            TypedExprKind::DynCoerce { value, iface_name, from_ty, .. } => {
                set.insert(iface_name.clone());
                walk_expr(value, set);
                walk_type(from_ty, set);
            }
            _ => {}
        }
    }
    fn walk_stmt(stmt: &TypedStmt, set: &mut std::collections::HashSet<String>) {
        match stmt {
            TypedStmt::Let { ty, expr, .. } => {
                walk_type(ty, set);
                walk_expr(expr, set);
            }
            TypedStmt::Reassign { ty, expr, .. } => {
                walk_type(ty, set);
                walk_expr(expr, set);
            }
            TypedStmt::Drop { ty, .. } => walk_type(ty, set),
            TypedStmt::Discard { expr } => walk_expr(expr, set),
            TypedStmt::Return { expr } => walk_expr(expr, set),
            TypedStmt::Assert { expr, .. } | TypedStmt::Prove { expr } => walk_expr(expr, set),
            TypedStmt::Print { items } => {
                for item in items {
                    if let crate::ir::TypedPrintItem::Expr(e) = item {
                        walk_expr(e, set);
                    }
                }
            }
            TypedStmt::If { cond, then_body, else_body } => {
                walk_expr(cond, set);
                then_body.iter().for_each(|s| walk_stmt(s, set));
                else_body.iter().for_each(|s| walk_stmt(s, set));
            }
            TypedStmt::While { cond, body } => {
                walk_expr(cond, set);
                body.iter().for_each(|s| walk_stmt(s, set));
            }
            TypedStmt::For { ty, start, end, body, .. } => {
                walk_type(ty, set);
                walk_expr(start, set);
                walk_expr(end, set);
                body.iter().for_each(|s| walk_stmt(s, set));
            }
            TypedStmt::ForIter { element_ty, collection_ty, body, .. } => {
                walk_type(element_ty, set);
                walk_type(collection_ty, set);
                body.iter().for_each(|s| walk_stmt(s, set));
            }
            TypedStmt::IndexAssign { index, value, base_ty, .. } => {
                walk_type(base_ty, set);
                walk_expr(index, set);
                walk_expr(value, set);
            }
            TypedStmt::FieldAssign { object, value, .. } => {
                walk_expr(object, set);
                walk_expr(value, set);
            }
            TypedStmt::TaskSpawn { body, captures, .. } => {
                captures.iter().for_each(|(_, t)| walk_type(t, set));
                body.iter().for_each(|s| walk_stmt(s, set));
            }
            TypedStmt::TaskJoin { .. } | TypedStmt::Break | TypedStmt::Continue => {}
        }
    }
    let mut set = std::collections::HashSet::new();
    for f in &program.functions {
        walk_type(&f.return_type, &mut set);
        for p in &f.params { walk_type(&p.ty, &mut set); }
        for s in &f.body { walk_stmt(s, &mut set); }
    }
    for sd in &program.structs {
        for (_, fty) in &sd.fields { walk_type(fty, &mut set); }
    }
    for ed in &program.enums {
        for pt in &ed.payload_types {
            if let Some(t) = pt { walk_type(t, &mut set); }
        }
    }
    set
}

/// Vtables Phase 3 + Phase 4: emit per-Iface vtable forward
/// decl + `intent_dyn_<Iface>` fat-pointer typedef so structs
/// declared LATER can carry `dyn Iface` fields. The full
/// `struct intent_vtbl_<Iface>` body is emitted by
/// `emit_dyn_iface_vtable_bodies` AFTER struct typedefs so
/// it can reference `Struct_<T>` arg types if any iface
/// method takes a struct by value. Only emits for ifaces
/// actually used as `dyn Iface` somewhere.
fn emit_dyn_iface_typedefs(out: &mut String, used: &std::collections::HashSet<String>) {
    for iface in crate::ast::all_iface_names() {
        if !used.contains(&iface) { continue; }
        out.push_str(&format!(
            "typedef struct intent_vtbl_{iface} intent_vtbl_{iface};\n",
            iface = iface,
        ));
        out.push_str(&format!(
            "typedef struct intent_dyn_{iface} {{ \
const intent_vtbl_{iface}* vtable; void* data; \
}} intent_dyn_{iface};\n",
            iface = iface
        ));
    }
}

/// Vtables Phase 4: emit the full body of each
/// `struct intent_vtbl_<Iface>` after struct typedefs are
/// declared, so the fn-ptr slots can reference `Struct_<T>`
/// for methods that take structs by value.
fn emit_dyn_iface_vtable_bodies(
    out: &mut String,
    used: &std::collections::HashSet<String>,
) {
    for iface in crate::ast::all_iface_names() {
        if !used.contains(&iface) { continue; }
        let Some(methods) = crate::ast::iface_methods_for(&iface) else {
            continue;
        };
        out.push_str(&format!("struct intent_vtbl_{} {{\n", iface));
        for (idx, (_name, params, ret)) in methods.iter().enumerate() {
            let ret_ty = c_type_name(ret);
            let arg_decls: Vec<String> = std::iter::once("void*".to_string())
                .chain(params.iter().skip(1).map(|t| format_declarator(t, "").trim().to_string()))
                .collect();
            out.push_str(&format!(
                "    {} (*m{})({});\n",
                ret_ty, idx, arg_decls.join(", ")
            ));
        }
        out.push_str("};\n");
    }
}

/// Vtables Phase 3: emit per-(T, Iface) trampolines and the
/// static vtable instances they populate. A trampoline
/// converts `void* self` to the concrete self shape declared
/// by the impl method (by-value, ref, or mut-ref) and forwards
/// to the hoisted `<Type>_<method>` function.
fn emit_dyn_iface_vtables(out: &mut String, used: &std::collections::HashSet<String>) {
    for iface in crate::ast::all_iface_names() {
        if !used.contains(&iface) { continue; }
        let Some(methods) = crate::ast::iface_methods_for(&iface) else {
            continue;
        };
        for type_name in crate::ast::impls_for_iface(&iface) {
            for (idx, (method_name, params, ret)) in methods.iter().enumerate() {
                let ret_ty = c_type_name(ret);
                let self_ty = &params[0];
                let mut sig_args: Vec<String> = vec!["void* __intent_self".to_string()];
                let mut forwarded: Vec<String> = Vec::new();
                // The trampoline's self-shape follows the
                // iface declaration (value, ref, or mut-ref)
                // but the underlying nominal type comes from
                // THIS impl's concrete `for_type` — not the
                // example self the iface declaration spelled.
                // Otherwise heterogeneous impls (Circle.area,
                // Square.area) would all cast to the iface's
                // first declared self, which is wrong for any
                // non-first impl.
                let impl_struct_name = format!("Struct_{}", type_name);
                let self_forward = match self_ty {
                    Type::Struct(_) | Type::Enum(_) => {
                        format!("*(({}*)__intent_self)", impl_struct_name)
                    }
                    Type::Ref(_) => {
                        format!("(const {}*)__intent_self", impl_struct_name)
                    }
                    Type::RefMut(_) => {
                        format!("({}*)__intent_self", impl_struct_name)
                    }
                    other => {
                        panic!(
                            "vtables Phase 3: unsupported self shape `{}` for \
                             interface '{}' method '{}' — v1 supports value, \
                             ref, and mut-ref receivers only",
                            other, iface, method_name
                        );
                    }
                };
                forwarded.push(self_forward);
                for (i, pt) in params.iter().enumerate().skip(1) {
                    let pname = format!("__intent_arg{}", i);
                    sig_args.push(format_declarator(pt, &pname));
                    forwarded.push(pname);
                }
                out.push_str(&format!(
                    "static {ret} intent_trampoline_{type_name}_{iface}_{slot}_{method}({sig}) {{\n",
                    ret = ret_ty,
                    type_name = type_name,
                    iface = iface,
                    slot = idx,
                    method = method_name,
                    sig = sig_args.join(", "),
                ));
                out.push_str(&format!(
                    "    return fn_{type_name}_{method}({fwd});\n",
                    type_name = type_name,
                    method = method_name,
                    fwd = forwarded.join(", "),
                ));
                out.push_str("}\n");
            }
            out.push_str(&format!(
                "static const intent_vtbl_{iface} intent_vtbl_{iface}_{type_name} = {{\n",
                iface = iface,
                type_name = type_name,
            ));
            for (idx, (method_name, _, _)) in methods.iter().enumerate() {
                out.push_str(&format!(
                    "    .m{slot} = intent_trampoline_{type_name}_{iface}_{slot}_{method},\n",
                    slot = idx,
                    type_name = type_name,
                    iface = iface,
                    method = method_name,
                ));
            }
            out.push_str("};\n");
        }
    }
}
