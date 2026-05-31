use vani::ast::Type;
use vani::backend::Backend;
use vani::backend_c::CBackend;
use vani::backend_llvm::LlvmBackend;
use vani::ir::{TypedExpr, TypedExprKind, TypedPrintItem, TypedProgram, TypedStmt};
use vani::ssa::lower_program;
use vani::ssa_backend_c;
use vani::ssa_backend_llvm;

/// Module-wide gate: returns false if any function uses a
/// feature the SSA backends don't yet cover safely. Avoids
/// emitting broken IR that would silently produce wrong
/// output (the runtime error would only surface in tests).
/// Per-backend `extra_reject` lets callers add backend-
/// specific exclusions on top of the common set (e.g.,
/// SSA-LLVM still rejects parallel-for and tasks; SSA-C
/// now handles parallel-for so it sets `false` here).
fn ssa_path_supports(
    ir: &TypedProgram,
    extra_reject: impl Fn(&TypedStmt) -> bool,
) -> bool {
    for f in &ir.functions {
        for param in &f.params {
            if !ssa_type_supported(&param.ty) {
                return false;
            }
        }
        if !ssa_type_supported(&f.return_type) {
            return false;
        }
        if !stmts_ssa_supported(&f.body, &extra_reject) {
            return false;
        }
    }
    true
}

/// SSA-LLVM handles `parallel for` (full reduction op
/// table) and `task`/`join` (single-block bodies via
/// pthread_create/CreateThread outlining). Multi-block
/// task bodies and other unsupported shapes surface
/// `EmitError` from inside the SSA-LLVM emit → tree-LLVM
/// fallback.
fn ssa_llvm_extra_reject(stmt: &TypedStmt) -> bool {
    // Closure #212: `Vec<Atomic<T>>` / `Vec<Channel<T,N>>`
    // route through SSA-LLVM's vec literal emit which
    // expects the element to be a value-shaped LLVM type
    // (i32, i64, struct, …). SSA-LLVM represents Atomic
    // as the alloca *pointer* (so subsequent `&counter`
    // references reuse the same address), and Channel
    // similarly indirects through the struct. Storing a
    // pointer-shaped SSA value into an `i32` Vec slot
    // emits `store i32 %ptr, …` which fails the LLVM IR
    // verifier with a type mismatch. Tree-LLVM doesn't
    // have this issue (it goes through a different vec
    // emit path) — gate Vec<Atomic|Channel> out of SSA-
    // LLVM so it falls back to tree-LLVM. Also gates any
    // outer Vec containing Atomic/Channel at any nesting
    // depth.
    stmt_uses_vec_of_atomic_or_channel(stmt)
}

fn ty_contains_vec_of_atomic_or_channel(ty: &Type) -> bool {
    match ty {
        Type::Vec(inner) => matches!(
            &**inner,
            Type::Atomic(_) | Type::Channel(_, _)
        ) || ty_contains_vec_of_atomic_or_channel(inner),
        Type::Array { element, .. } => ty_contains_vec_of_atomic_or_channel(element),
        Type::Ref(inner) | Type::RefMut(inner) => {
            ty_contains_vec_of_atomic_or_channel(inner)
        }
        Type::Tuple(elements) => elements
            .iter()
            .any(ty_contains_vec_of_atomic_or_channel),
        Type::FnPtr(params, ret) => {
            params.iter().any(ty_contains_vec_of_atomic_or_channel)
                || ty_contains_vec_of_atomic_or_channel(ret)
        }
        _ => false,
    }
}

fn expr_uses_vec_of_atomic_or_channel(expr: &vani::ir::TypedExpr) -> bool {
    if ty_contains_vec_of_atomic_or_channel(&expr.ty) {
        return true;
    }
    use vani::ir::TypedExprKind as E;
    match &expr.kind {
        E::Unary { expr, .. } | E::Cast { expr, .. } | E::Len { array: expr, .. } => {
            expr_uses_vec_of_atomic_or_channel(expr)
        }
        E::Binary { left, right, .. } => {
            expr_uses_vec_of_atomic_or_channel(left)
                || expr_uses_vec_of_atomic_or_channel(right)
        }
        E::Call { args, .. }
        | E::ArrayLit { elements: args } => {
            args.iter().any(expr_uses_vec_of_atomic_or_channel)
        }
        E::CallIndirect { callee, args } => {
            expr_uses_vec_of_atomic_or_channel(callee)
                || args.iter().any(expr_uses_vec_of_atomic_or_channel)
        }
        E::Index { array, index, .. } => {
            expr_uses_vec_of_atomic_or_channel(array)
                || expr_uses_vec_of_atomic_or_channel(index)
        }
        E::Tuple { elements } => {
            elements.iter().any(expr_uses_vec_of_atomic_or_channel)
        }
        E::TupleAccess { tuple, .. } => expr_uses_vec_of_atomic_or_channel(tuple),
        E::StructLit { fields, .. } => fields
            .iter()
            .any(|(_, e)| expr_uses_vec_of_atomic_or_channel(e)),
        E::FieldAccess { object, .. } => expr_uses_vec_of_atomic_or_channel(object),
        E::EnumVariantWithPayload { payload, .. } => {
            expr_uses_vec_of_atomic_or_channel(payload)
        }
        E::IfExpr { cond, then_value, else_value } => {
            expr_uses_vec_of_atomic_or_channel(cond)
                || expr_uses_vec_of_atomic_or_channel(then_value)
                || expr_uses_vec_of_atomic_or_channel(else_value)
        }
        E::Match { scrutinee, arms } => {
            expr_uses_vec_of_atomic_or_channel(scrutinee)
                || arms.iter().any(|arm| expr_uses_vec_of_atomic_or_channel(&arm.body))
        }
        E::Block { stmts, tail } => {
            stmts.iter().any(stmt_uses_vec_of_atomic_or_channel)
                || expr_uses_vec_of_atomic_or_channel(tail)
        }
        _ => false,
    }
}

fn stmt_uses_vec_of_atomic_or_channel(stmt: &TypedStmt) -> bool {
    match stmt {
        TypedStmt::Let { ty, expr, .. } | TypedStmt::Reassign { ty, expr, .. } => {
            ty_contains_vec_of_atomic_or_channel(ty)
                || expr_uses_vec_of_atomic_or_channel(expr)
        }
        TypedStmt::Drop { ty, .. } => ty_contains_vec_of_atomic_or_channel(ty),
        TypedStmt::Discard { expr }
        | TypedStmt::Return { expr }
        | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => expr_uses_vec_of_atomic_or_channel(expr),
        TypedStmt::Print { items } => items.iter().any(|i| match i {
            TypedPrintItem::Expr(e) => expr_uses_vec_of_atomic_or_channel(e),
            TypedPrintItem::Str(_) => false,
        }),
        TypedStmt::If { cond, then_body, else_body } => {
            expr_uses_vec_of_atomic_or_channel(cond)
                || then_body.iter().any(stmt_uses_vec_of_atomic_or_channel)
                || else_body.iter().any(stmt_uses_vec_of_atomic_or_channel)
        }
        TypedStmt::While { cond, body } => {
            expr_uses_vec_of_atomic_or_channel(cond)
                || body.iter().any(stmt_uses_vec_of_atomic_or_channel)
        }
        TypedStmt::For { start, end, body, .. } => {
            expr_uses_vec_of_atomic_or_channel(start)
                || expr_uses_vec_of_atomic_or_channel(end)
                || body.iter().any(stmt_uses_vec_of_atomic_or_channel)
        }
        TypedStmt::ForIter { body, element_ty, collection_ty, .. } => {
            ty_contains_vec_of_atomic_or_channel(element_ty)
                || ty_contains_vec_of_atomic_or_channel(collection_ty)
                || body.iter().any(stmt_uses_vec_of_atomic_or_channel)
        }
        TypedStmt::IndexAssign { index, value, .. } => {
            expr_uses_vec_of_atomic_or_channel(index)
                || expr_uses_vec_of_atomic_or_channel(value)
        }
        TypedStmt::FieldAssign { object, value, .. } => {
            expr_uses_vec_of_atomic_or_channel(object)
                || expr_uses_vec_of_atomic_or_channel(value)
        }
        TypedStmt::TaskSpawn { body, .. } => {
            body.iter().any(stmt_uses_vec_of_atomic_or_channel)
        }
        _ => false,
    }
}

/// SSA-C now handles both `parallel for` (via OpenMP
/// pragmas + reduction clauses) and `task`/`join` (via the
/// `intent_thread_create`/`intent_thread_join` runtime
/// wrappers + outlined `static void* intent_task_<N>(void*)`
/// helpers). Multi-block task bodies and non-canonical
/// parallel-for carry shapes still surface `EmitError` →
/// tree-C fallback.
fn ssa_c_extra_reject(_stmt: &TypedStmt) -> bool {
    false
}

fn ssa_type_supported(ty: &Type) -> bool {
    // Every concurrency primitive now flows through SSA
    // (Atomic + Mutex/Guard + Channel) on both SSA-C and
    // SSA-LLVM. Anything an SSA backend can't yet handle
    // surfaces `EmitError` from inside its emit and falls
    // back per-backend in `emit_c_via_ssa` /
    // `emit_llvm_via_ssa`.
    //
    // Exception (closure #239): `[T; N]` in return position
    // routes through tree-LLVM. SSA-LLVM's array-return emit
    // returns a pointer to a stack-alloca'd array (the
    // pointer dangles after the fn returns); tree-C's
    // struct-wrap also lives in tree-side emit. Fix by
    // gating away from SSA when an array return appears
    // anywhere in the program.
    if matches!(ty, Type::Array { .. }) {
        return false;
    }
    true
}

fn stmts_ssa_supported(stmts: &[TypedStmt], extra_reject: &impl Fn(&TypedStmt) -> bool) -> bool {
    stmts.iter().all(|s| stmt_ssa_supported(s, extra_reject))
}

fn stmt_ssa_supported(stmt: &TypedStmt, extra_reject: &impl Fn(&TypedStmt) -> bool) -> bool {
    if extra_reject(stmt) {
        return false;
    }
    match stmt {
        TypedStmt::Print { items } => items.iter().all(|i| match i {
            TypedPrintItem::Expr(e) => expr_ssa_supported(e),
            TypedPrintItem::Str(_) => true,
        }),
        TypedStmt::Let { ty, expr, .. } | TypedStmt::Reassign { ty, expr, .. } => {
            ssa_type_supported(ty) && expr_ssa_supported(expr)
        }
        TypedStmt::Drop { ty, .. } => ssa_type_supported(ty),
        TypedStmt::Discard { expr } => expr_ssa_supported(expr),
        TypedStmt::Return { expr } => expr_ssa_supported(expr),
        TypedStmt::Assert { expr, .. } | TypedStmt::Prove { expr } => {
            expr_ssa_supported(expr)
        }
        TypedStmt::If { cond, then_body, else_body } => {
            expr_ssa_supported(cond)
                && stmts_ssa_supported(then_body, extra_reject)
                && stmts_ssa_supported(else_body, extra_reject)
        }
        TypedStmt::While { cond, body } => {
            expr_ssa_supported(cond) && stmts_ssa_supported(body, extra_reject)
        }
        TypedStmt::For { start, end, body, .. } => {
            expr_ssa_supported(start)
                && expr_ssa_supported(end)
                && stmts_ssa_supported(body, extra_reject)
        }
        TypedStmt::ForIter { collection_ty, consumes, element_ty, body, .. } => {
            // Consuming `for x in xs` over a Vec of non-Copy
            // elements: the SSA lowerer never emits a Drop for
            // the consumed collection, leaving the outer buffer
            // leaked (and there is no IR shape for "free the
            // outer buffer only, skip the per-element walk").
            // Route through tree-LLVM/tree-C which now handles
            // it directly via `emit_for_iter`. Closure #159.
            let consume_owned_vec = *consumes
                && matches!(collection_ty, Type::Vec(_))
                && !element_ty.is_copy();
            if consume_owned_vec {
                return false;
            }
            ssa_type_supported(collection_ty)
                && stmts_ssa_supported(body, extra_reject)
        }
        TypedStmt::TaskSpawn { body, .. } => stmts_ssa_supported(body, extra_reject),
        TypedStmt::TaskJoin { .. } => true,
        TypedStmt::IndexAssign { base_ty, index, value, .. } => {
            ssa_type_supported(base_ty)
                && expr_ssa_supported(index)
                && expr_ssa_supported(value)
        }
        // FieldAssign currently has no SSA lowering — route
        // through the tree backend. T1.2 phase 2a follow-up.
        TypedStmt::FieldAssign { .. } => false,
        TypedStmt::Break | TypedStmt::Continue => true,
    }
}

fn expr_ssa_supported(expr: &TypedExpr) -> bool {
    if !ssa_type_supported(&expr.ty) {
        return false;
    }
    match &expr.kind {
        TypedExprKind::Binary { left, right, .. } => {
            expr_ssa_supported(left) && expr_ssa_supported(right)
        }
        TypedExprKind::Unary { expr: e, .. } => expr_ssa_supported(e),
        TypedExprKind::Cast { expr: e, .. } => expr_ssa_supported(e),
        TypedExprKind::Index { array, index, .. } => {
            expr_ssa_supported(array) && expr_ssa_supported(index)
        }
        TypedExprKind::Len { array, .. } => expr_ssa_supported(array),
        TypedExprKind::Call { name, args, .. } => {
            // `push_mut` (the in-place `push(mut ref xs, v)`
            // form), `pop` (in-place `pop(mut ref xs)`), and
            // `sort` / `sort_by` (in-place on `Vec<i64>`) all
            // operate through a Vec pointer and have no
            // SSA-backend lowering yet — route through the
            // tree backend.
            if name == "push_mut" || name == "pop"
                || name == "sort" || name == "sort_by" || name == "sort_desc"
                || name == "vec_swap" || name == "vec_remove_at"
                || name == "vec_replace_all"
                || name == "reverse" || name == "dedup"
                || name == "find" || name == "contains"
                || name == "binary_search"
                || name == "swap_remove" || name == "insert"
                || name == "clear"
                || name == "str_contains" || name == "str_starts_with"
                || name == "str_ends_with" || name == "str_trim"
                || name == "str_replace" || name == "str_split"
                || name == "str_index_of"
                || name == "substring"
                || name == "str_repeat"
                || name == "str_to_upper" || name == "str_to_lower"
                || name == "parse_bool"
                || name == "str_join"
                || name == "str_pad_left" || name == "str_pad_right"
                || name == "str_lines"
                || name == "str_chars" || name == "str_reverse"
                || name == "str_strip_prefix" || name == "str_strip_suffix"
                || name == "str_count_char"
                || name == "i64_to_str"
                || name == "f64_to_str"
                || name == "bool_to_str"
                || name == "parse_int"
                || name == "parse_float"
                || name == "pow" || name == "sqrt"
                || name == "sin" || name == "cos" || name == "tan"
                || name == "floor" || name == "ceil" || name == "abs"
                || name == "log" || name == "log2" || name == "log10"
                || name == "exp" || name == "atan2"
                || name == "f64_is_nan" || name == "f64_is_inf"
                || name == "f64_is_finite"
                || name == "f64_pi" || name == "f64_e"
                || name == "f64_inf" || name == "f64_nan"
                || name == "f64_round" || name == "f64_trunc_to_i64"
                || name == "i64_gcd" || name == "i64_lcm" || name == "i64_pow"
                || name == "i64_abs_diff" || name == "i64_signum"
                || name == "f64_signum"
                || name == "is_ascii_digit" || name == "is_ascii_alpha"
                || name == "is_ascii_alphanumeric" || name == "is_ascii_whitespace"
                || name == "i64_count_set_bits"
                || name == "i64_leading_zeros"
                || name == "i64_trailing_zeros"
                || name == "i64_bswap"
                || name == "i64_rotate_left"
                || name == "i64_rotate_right"
                || name == "f64_to_bits" || name == "f64_from_bits"
                || name == "i64_min_value" || name == "i64_max_value"
                || name == "f64_max_finite"
                || name == "i64_div_floor" || name == "i64_mod_floor"
                || name == "f64_lerp" || name == "f64_clamp01"
                || name == "i64_log2_floor" || name == "i64_log2_ceil"
                || name == "i64_is_power_of_2" || name == "i64_next_power_of_2"
                || name == "i64_saturating_add"
                || name == "i64_saturating_sub"
                || name == "i64_saturating_mul"
                || name == "i64_min" || name == "i64_max" || name == "i64_clamp"
                || name == "f64_min" || name == "f64_max" || name == "f64_clamp"
                || name == "i64_isqrt"
                || name == "f64_hypot"
                || name == "f64_to_radians" || name == "f64_to_degrees"
                || name == "asin" || name == "acos" || name == "atan"
                || name == "sinh" || name == "cosh" || name == "tanh"
                || name == "f64_epsilon"
                || name == "f64_min_positive" || name == "f64_min_subnormal"
                || name == "f64_copysign" || name == "f64_fma"
                || name == "f64_remainder"
                || name == "f64_is_normal" || name == "f64_is_subnormal"
                || name == "f64_sign_bit"
                || name == "seed_rng" || name == "rand_i64"
                || name == "rand_in_range"
                || name == "hash_i64" || name == "hash_f64"
                || name == "hash_str" || name == "hash_combine"
                || name == "siphash_i64" || name == "siphash_str"
                || name == "heap_push" || name == "heap_pop"
                || name == "heap_peek" || name == "heapify"
                || name == "deque_new"
                || name == "deque_push_back" || name == "deque_push_front"
                || name == "deque_pop_back" || name == "deque_pop_front"
                || name == "deque_peek_back" || name == "deque_peek_front"
                || name == "deque_len" || name == "deque_clear"
                || name == "hashset_new" || name == "hashset_insert"
                || name == "hashset_contains" || name == "hashset_remove"
                || name == "hashset_len" || name == "hashset_clear"
                || name == "hashmap_new" || name == "hashmap_insert"
                || name == "hashmap_get" || name == "hashmap_contains_key"
                || name == "hashmap_remove"
                || name == "hashmap_len" || name == "hashmap_clear"
                || name == "btreeset_new" || name == "btreeset_insert"
                || name == "btreeset_contains" || name == "btreeset_remove"
                || name == "btreeset_len" || name == "btreeset_range"
                || name == "btreeset_min" || name == "btreeset_max"
                || name == "btreeset_clear"
                || name == "btreemap_new" || name == "btreemap_insert"
                || name == "btreemap_get" || name == "btreemap_contains_key"
                || name == "btreemap_remove" || name == "btreemap_len"
                || name == "btreemap_range_keys" || name == "btreemap_range_values"
                || name == "btreemap_min_key" || name == "btreemap_max_key"
                || name == "btreemap_clear"
                || name == "vec_map" || name == "vec_fold" || name == "vec_filter"
                || name == "vec_position"
                || name == "vec_count_if"
                || name == "vec_max_by" || name == "vec_min_by"
                || name == "vec_zip_with"
                || name == "vec_range" || name == "vec_repeat"
                || name == "vec_extend" || name == "vec_concat"
                || name == "vec_reverse_copy" || name == "vec_unique"
                || name == "vec_iota"
                || name == "vec_first" || name == "vec_last"
                || name == "vec_running_sum"
                || name == "vec_dot"
                || name == "vec_intersect" || name == "vec_difference" || name == "vec_union"
                || name == "option_unwrap_or"
                || name == "option_is_some" || name == "option_is_none"
                || name == "option_map"
                || name == "option_filter" || name == "option_or"
                || name == "option_and_then"
                || name == "option_unwrap_or_f64"
                || name == "option_is_some_f64" || name == "option_is_none_f64"
                || name == "vec_take" || name == "vec_drop" || name == "vec_map_fold"
                || name == "vec_take_while" || name == "vec_drop_while"
                || name == "vec_filter_fold" || name == "vec_map_filter"
                || name == "vec_map_filter_fold"
                || name == "vec_sum" || name == "vec_product"
                || name == "vec_min" || name == "vec_max"
                || name == "vec_count" || name == "vec_any" || name == "vec_all"
                || name == "vec_chain"
                || name == "union_find_new" || name == "union_find_union"
                || name == "union_find_find" || name == "union_find_connected"
                || name == "union_find_count" || name == "union_find_clear"
                || name == "binary_heap_new" || name == "binary_heap_push"
                || name == "binary_heap_pop" || name == "binary_heap_peek"
                || name == "binary_heap_len" || name == "binary_heap_clear"
                || name == "bloom_filter_new" || name == "bloom_filter_insert"
                || name == "bloom_filter_contains" || name == "bloom_filter_len"
                || name == "bloom_filter_count" || name == "bloom_filter_clear"
                || name == "bst_new" || name == "bst_insert"
                || name == "bst_contains" || name == "bst_remove"
                || name == "bst_len" || name == "bst_min" || name == "bst_max"
                || name == "bst_clear"
                || name == "graph_new" || name == "graph_add_edge"
                || name == "graph_num_nodes" || name == "graph_num_edges"
                || name == "graph_bfs_reach" || name == "graph_dfs_reach"
                || name == "graph_dijkstra"
                || name == "graph_has_cycle" || name == "graph_mst_kruskal"
                || name == "graph_mst_prim"
                || name == "graph_astar" || name == "graph_topo_sort"
                || name == "graph_clear"
                || name == "trie_new" || name == "trie_insert"
                || name == "trie_contains" || name == "trie_starts_with"
                || name == "trie_delete"
                || name == "trie_len" || name == "trie_node_count"
                || name == "trie_clear"
                || name == "skiplist_new" || name == "skiplist_insert"
                || name == "skiplist_contains" || name == "skiplist_remove"
                || name == "skiplist_len"
                || name == "skiplist_min" || name == "skiplist_max"
                || name == "skiplist_clear"
            {
                return false;
            }
            args.iter().all(expr_ssa_supported)
        }
        TypedExprKind::CallIndirect { callee, args } => {
            expr_ssa_supported(callee) && args.iter().all(expr_ssa_supported)
        }
        TypedExprKind::ArrayLit { elements } => {
            elements.iter().all(expr_ssa_supported)
        }
        TypedExprKind::Int(_)
        | TypedExprKind::Float(_)
        | TypedExprKind::Bool(_)
        | TypedExprKind::Str(_)
        | TypedExprKind::Var(_)
        | TypedExprKind::Ref { .. }
        | TypedExprKind::RefMut { .. }
        | TypedExprKind::FnRef { .. } => true,
        // Tuples flow through the tree backends; SSA lowering
        // surfaces LowerError which routes here. Mark
        // unsupported so the SSA gate falls back early. T1.1.
        TypedExprKind::Tuple { .. } | TypedExprKind::TupleAccess { .. } => false,
        // Structs likewise fall back to tree backends until
        // SSA support lands (T1.2 follow-up).
        TypedExprKind::StructLit { .. } | TypedExprKind::FieldAccess { .. } => false,
        // Enums + match also fall through to tree backends
        // for now. T1.3 follow-up.
        TypedExprKind::EnumVariant { .. }
        | TypedExprKind::EnumVariantWithPayload { .. }
        | TypedExprKind::Match { .. } => false,
        // If-expressions route through tree backends. T4.
        TypedExprKind::IfExpr { .. } => false,
        // Block expressions route through tree backends in
        // v1 (SSA lowering can be added in a follow-up).
        TypedExprKind::Block { .. } => false,
        // Struct field-borrow routes through tree backends —
        // SSA doesn't model field-paths yet. T1.2 phase 2b
        // follow-up.
        TypedExprKind::RefField { .. } | TypedExprKind::RefMutField { .. } => false,
        // `dyn Iface` method dispatch / coercion route
        // through tree backends; SSA vtable lowering lands
        // with Phase 3.
        TypedExprKind::DynDispatch { .. } | TypedExprKind::DynCoerce { .. } => false,
    }
}

/// Try the SSA-driven C backend first; fall back to the
/// tree-based path if the SSA pipeline doesn't yet cover a
/// feature the program uses (Vec/Channel/FnPtr/Atomic/etc.).
/// Once SSA-C reaches feature parity, the fallback can go.
fn emit_c_via_ssa(ir: &TypedProgram) -> String {
    if ssa_path_supports(ir, ssa_c_extra_reject) {
        let (module, lower_errs) = lower_program(ir);
        if lower_errs.is_empty() {
            if let Ok(c) = ssa_backend_c::emit(&module) {
                return c;
            }
        }
    }
    CBackend.emit(ir)
}

/// Same dual-path strategy for the LLVM backend.
fn emit_llvm_via_ssa(ir: &TypedProgram) -> String {
    // T1.3 phase 2b: payloaded enums need tagged-union codegen.
    // Tree-LLVM now supports them (closure #90); SSA-LLVM
    // doesn't. Force the tree-LLVM path for payloaded
    // programs.
    let has_payloaded_enum = ir
        .enums
        .iter()
        .any(|e| e.payload_types.iter().any(|p| p.is_some()));
    if !has_payloaded_enum && ssa_path_supports(ir, ssa_llvm_extra_reject) {
        let (module, lower_errs) = lower_program(ir);
        if lower_errs.is_empty() {
            if let Ok(ll) = ssa_backend_llvm::emit(&module) {
                return ll;
            }
        }
    }
    LlvmBackend.emit(ir)
}
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const HELP: &str = "\
intentc — vāṇī language compiler driver

USAGE:
    intentc <COMMAND> [ARGS]

COMMANDS:
    check <path>... [--json] [--no-verify] [--smt-debug]
                                          Type-check one or more sources.
                                          Paths may be files or directories
                                          (the latter expand recursively to
                                          *.vani descendants). With --json,
                                          a single combined diagnostics
                                          object on stdout collects all
                                          findings across every file. With
                                          --no-verify, the SMT verifier
                                          is skipped for fast iteration
                                          (runtime guards stay in place);
                                          same effect as INTENTC_NO_VERIFY=1.
                                          With --smt-debug, every SMT query
                                          and z3 response is dumped to
                                          stderr (also via INTENTC_SMT_DEBUG=1).
    emit <file.vani> [--backend=<c|llvm>] [-o out]
                                          Emit lowered source for a program.
                                          --backend defaults to 'llvm'. Pass
                                          --backend=c for the legacy C output.
    emit-c <file.vani> [-o out.c]       Legacy alias for 'emit --backend=c'.
                                          Kept for back-compat.
    run <file.vani> [--backend=<c|llvm>]
        [--link-with PATH ...]            Compile and run a program. Default
        [-l<name> ...]                    backend is 'llvm' (emits LLVM IR
                                          and runs it via $LLI or `lli`).
                                          With --backend=c, invokes $CC or
                                          `cc` on the C output.
                                          --link-with / -l<name> require
                                          --backend=c (LLVM-JIT auto-resolves
                                          host symbols).
    build <file.vani> [-o out]          AOT-compile to a native binary.
          [--link-with PATH ...]          Lowers via the LLVM backend, calls
          [-l<name> ...]                  $LLC (or `llc`) for object code,
                                          then $CC (or `cc`) to link with
                                          libc. Output defaults to the
                                          source file's stem in the cwd.
                                          --link-with adds an extra object
                                          or source file to the link line
                                          (e.g. foo.o, foo.c) for `extern
                                          \"C\" fn` whose body lives in a
                                          separately-compiled translation
                                          unit. -l<name> forwards a system
                                          library flag (e.g. -lm) to cc.
    tokens <file.vani>                  Dump the token stream (debug).
    ast <file.vani>                     Dump the parsed AST (debug). Skips
                                          type checking.
    ir <file.vani>                      Dump the typed IR (debug). Runs the
                                          checker; what the backends see.
    fmt <path>... [--check|--in-place]
                                          Pretty-print canonical source.
                                          // comments are preserved. Paths
                                          may be files or directories (the
                                          latter expand recursively to
                                          *.vani descendants; dot-dirs
                                          skipped).
                                          Default writes to stdout (single-
                                          file only); --check exits 1 if
                                          any file is not canonical;
                                          --in-place rewrites each file
                                          (mtime stable when canonical).
    test <path>... [--json] [--smt-debug] Compile + run each path via the
                                          LLVM backend, treating exit 0 as
                                          pass. Paths may be files or
                                          directories (the latter expand
                                          recursively to *.vani
                                          descendants; dot-dirs skipped).
                                          Output per file plus a summary;
                                          exits 1 if any failed.
                                          With --json, a machine-readable
                                          results object is printed on
                                          stdout instead of human lines.
                                          With --smt-debug, every SMT query
                                          and z3 response is dumped to
                                          stderr (also via INTENTC_SMT_DEBUG=1).

MANIFEST (vani.toml):
    For run / build / check / emit / ir / ast / tokens, if
    no source file is given on the command line, the driver
    walks up from the current directory looking for a
    `vani.toml` manifest. When found, its `[package].entry`
    key supplies the source file. Minimal format:

        [package]
        name = \"my_project\"
        entry = \"src/main.vani\"

GLOBAL OPTIONS:
    -h, --help        Show this message
    -V, --version     Show version
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{}", message);
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        return Err(HELP.to_string());
    }

    match args[1].as_str() {
        "-h" | "--help" => {
            println!("{}", HELP);
            Ok(ExitCode::SUCCESS)
        }
        "-V" | "--version" => {
            println!("intentc {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        "check" => {
            // Type-check one or more files. Paths may be files or
            // directories (the latter expand recursively to
            // `*.vani` descendants via `expand_intent_paths`).
            // Exit 1 if any file's check fails. For `--json`, all
            // diagnostics across all files are flattened into a
            // single `{"diagnostics": [...]}` object so the schema
            // matches the single-file form.
            let mut json = false;
            let mut path_args: Vec<String> = Vec::new();
            for arg in args.iter().skip(2) {
                match arg.as_str() {
                    "--json" => json = true,
                    "--no-verify" => {
                        // Same effect as INTENTC_NO_VERIFY=1 — sets
                        // the env var for the remainder of the
                        // process so the checker's gates fire.
                        std::env::set_var("INTENTC_NO_VERIFY", "1");
                    }
                    "--smt-debug" => {
                        // Surface the existing INTENTC_SMT_DEBUG=1
                        // toggle as a CLI flag so users debugging
                        // a `prove` failure don't have to
                        // rediscover the env var. The verifier
                        // dumps each SMT query + z3 response to
                        // stderr.
                        std::env::set_var("INTENTC_SMT_DEBUG", "1");
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unexpected argument '{}'", other));
                    }
                    other => path_args.push(other.to_string()),
                }
            }
            if path_args.is_empty() {
                return Err(format!("'check' requires a path argument\n\n{}", HELP));
            }
            let files = expand_intent_paths(&path_args)?;
            if files.is_empty() {
                return Err("no .vani files to check".into());
            }

            let mut failed = 0usize;
            // For --json multi-file: accumulate every file's
            // FileMap entries (shifted into a global frame) and
            // every diagnostic (shifted by the same amount) into a
            // single combined map/diags pair, then emit one JSON
            // object at the end.
            let mut combined_map = vani::diagnostic::FileMap::new();
            let mut combined_diags: Vec<vani::diagnostic::Diagnostic> = Vec::new();

            for file in &files {
                match vani::compile_path(file) {
                    Ok((_checked, _map)) => {
                        if !json && files.len() > 1 {
                            println!("ok: {}", file.display());
                        }
                    }
                    Err((map, diagnostics)) => {
                        failed += 1;
                        if json {
                            let shift = combined_map.extend_with(&map);
                            for d in diagnostics {
                                let mut shifted = d.clone();
                                shifted.span = vani::span::Span::new(
                                    d.span.start + shift,
                                    d.span.end + shift,
                                );
                                shifted.related = d
                                    .related
                                    .iter()
                                    .map(|(s, note)| {
                                        (
                                            vani::span::Span::new(
                                                s.start + shift,
                                                s.end + shift,
                                            ),
                                            note.clone(),
                                        )
                                    })
                                    .collect();
                                combined_diags.push(shifted);
                            }
                        } else if files.len() == 1 {
                            return Err(
                                vani::diagnostic::format_diagnostics_with_files(
                                    &map,
                                    &diagnostics,
                                ),
                            );
                        } else {
                            eprintln!(
                                "{}",
                                vani::diagnostic::format_diagnostics_with_files(
                                    &map,
                                    &diagnostics,
                                )
                            );
                        }
                    }
                }
            }

            if json {
                print!(
                    "{}",
                    vani::diagnostic::format_diagnostics_json_with_files(
                        &combined_map,
                        &combined_diags,
                    )
                );
                // The single-file `{"diagnostics":[]}` success case
                // also flows through here — combined_map is empty
                // and the formatter emits the right empty object.
            } else if failed == 0 {
                if files.len() == 1 {
                    println!("ok: {}", files[0].display());
                } else {
                    println!("ok: {} file(s)", files.len());
                }
            }
            if failed > 0 {
                Ok(ExitCode::from(1))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        "emit" | "emit-c" => {
            // `emit-c` is the legacy spelling kept for back-compat; the
            // new `emit` form takes an explicit `--backend=<c|llvm>` flag
            // so we can grow into LLVM (and beyond) without churning the
            // CLI. The legacy alias pins backend=c regardless of flags.
            let cmd_name = args[1].clone();
            let file = required_file(&args, 2, &cmd_name)?;
            let (backend_kind, out) = parse_emit_args(&args, 3, &cmd_name)?;
            let checked = compile_path_or_report(&file)?;
            let text = match backend_kind {
                BackendKind::C => emit_c_via_ssa(&checked.ir),
                BackendKind::Llvm => emit_llvm_via_ssa(&checked.ir),
            };
            match out {
                Some(path) => fs::write(&path, text)
                    .map_err(|error| format!("failed to write '{}': {}", path.display(), error))?,
                None => print!("{}", text),
            }
            Ok(ExitCode::SUCCESS)
        }
        "run" => {
            let (file, flag_start) = required_file_at(&args, 2, "run")?;
            let (backend_kind, link_args) = parse_run_args(&args, flag_start)?;
            match backend_kind {
                BackendKind::C => run_program(&file, &link_args),
                BackendKind::Llvm => {
                    if !link_args.is_empty() {
                        return Err(
                            "--link-with / -l<name> require --backend=c \
                             (LLVM-JIT via lli auto-resolves libc/libm symbols \
                             from the host process; use `intentc build … \
                             --link-with …` for AOT linking with custom code)"
                                .to_string(),
                        );
                    }
                    run_program_llvm(&file)
                }
            }
        }
        "build" => {
            let (file, flag_start) = required_file_at(&args, 2, "build")?;
            let (out, link_args) = parse_build_args(&args, flag_start)?;
            build_program_llvm(&file, out.as_deref(), &link_args)
        }
        "tokens" => {
            // Debug subcommand: dump the token stream to stdout.
            // Useful for parser/lexer development — see a token's
            // source span and kind without running the full
            // pipeline.
            let file = required_file(&args, 2, "tokens")?;
            let source = fs::read_to_string(&file)
                .map_err(|error| format!("failed to read '{}': {}", file.display(), error))?;
            match vani::lexer::lex(&source) {
                Ok(tokens) => {
                    for tok in &tokens {
                        println!("{:>5}..{:<5} {:?}", tok.span.start, tok.span.end, tok.kind);
                    }
                    Ok(ExitCode::SUCCESS)
                }
                Err(diag) => Err(vani::diagnostic::format_diagnostics(
                    file.to_str().unwrap_or("<input>"),
                    &source,
                    &[diag],
                )),
            }
        }
        "ast" => {
            // Debug subcommand: dump the parsed AST. Skips the
            // type checker — useful when you want to see what the
            // parser produced even if the checker would reject.
            let file = required_file(&args, 2, "ast")?;
            let source = fs::read_to_string(&file)
                .map_err(|error| format!("failed to read '{}': {}", file.display(), error))?;
            let tokens = vani::lexer::lex(&source).map_err(|diag| {
                vani::diagnostic::format_diagnostics(
                    file.to_str().unwrap_or("<input>"),
                    &source,
                    &[diag],
                )
            })?;
            let (program, diags) = vani::parser::parse(tokens);
            // Print whatever the parser produced, then surface any
            // parse diagnostics on stderr so partial parses are
            // still useful.
            println!("{:#?}", program);
            if !diags.is_empty() {
                eprintln!(
                    "{}",
                    vani::diagnostic::format_diagnostics(
                        file.to_str().unwrap_or("<input>"),
                        &source,
                        &diags
                    )
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "ir" => {
            // Debug subcommand: run the full pipeline through the
            // type checker and dump the resulting TypedProgram.
            // Useful for checker / IR work — see what the backends
            // are actually about to lower.
            let file = required_file(&args, 2, "ir")?;
            let checked = compile_path_or_report(&file)?;
            println!("{:#?}", checked.ir);
            Ok(ExitCode::SUCCESS)
        }
        "test" => {
            // Treat each path as a self-contained test case: compile +
            // run it through the LLVM backend, capturing stdout/stderr.
            // A test "passes" iff the program exits 0 (i.e. no `assert`
            // fired, no runtime guard tripped, no proof obligation
            // remained unsatisfied at runtime). Output per file plus a
            // summary line; exit 1 if any failed. A directory arg
            // expands to its `*.vani` children (non-recursive).
            if args.len() < 3 {
                return Err("test requires at least one source file\n\n".to_string() + HELP);
            }
            // Split flags from path args. Supported: --smt-debug
            // and --json. The JSON form is machine-readable for CI;
            // a single object on stdout, no per-file lines.
            let mut path_args: Vec<String> = Vec::new();
            let mut json = false;
            for arg in args.iter().skip(2) {
                match arg.as_str() {
                    "--smt-debug" => {
                        std::env::set_var("INTENTC_SMT_DEBUG", "1");
                    }
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        return Err(format!(
                            "unknown flag for 'test': '{}' (expected --smt-debug, --json)",
                            other
                        ));
                    }
                    other => path_args.push(other.to_string()),
                }
            }
            let files = expand_intent_paths(&path_args)?;
            if files.is_empty() {
                return Err("no .vani files to test".into());
            }
            let mut passed = 0usize;
            let mut failed = 0usize;
            // For --json mode we collect per-file outcomes and emit
            // one object at the end. Each result records ok-ness,
            // elapsed ms, and (for failures) the exit code + a
            // brief reason. We deliberately do NOT include
            // stdout/stderr in the JSON to keep the payload small —
            // the human-readable form prints them on FAILED.
            let mut json_results: Vec<String> = Vec::new();
            for path in &files {
                let start = std::time::Instant::now();
                let result = run_program_llvm_capture(path);
                let elapsed = start.elapsed().as_millis();
                let path_str = json_escape(&path.display().to_string());
                match result {
                    Ok((0, _, _)) => {
                        if !json {
                            println!("{}: ok ({} ms)", path.display(), elapsed);
                        }
                        json_results.push(format!(
                            "{{\"path\":\"{}\",\"ok\":true,\"ms\":{}}}",
                            path_str, elapsed
                        ));
                        passed += 1;
                    }
                    Ok((code, stdout, stderr)) => {
                        if !json {
                            println!(
                                "{}: FAILED (exit {}, {} ms)",
                                path.display(),
                                code,
                                elapsed
                            );
                            if !stdout.is_empty() {
                                eprintln!("--- stdout ---\n{}", stdout);
                            }
                            let stderr = trim_lli_backtrace(&stderr);
                            if !stderr.is_empty() {
                                eprintln!("--- stderr ---\n{}", stderr);
                            }
                        }
                        json_results.push(format!(
                            "{{\"path\":\"{}\",\"ok\":false,\"ms\":{},\"exit\":{},\"reason\":\"runtime\"}}",
                            path_str, elapsed, code
                        ));
                        failed += 1;
                    }
                    Err(msg) => {
                        if !json {
                            println!("{}: FAILED (compile, {} ms)", path.display(), elapsed);
                            eprintln!("{}", msg);
                        }
                        json_results.push(format!(
                            "{{\"path\":\"{}\",\"ok\":false,\"ms\":{},\"reason\":\"compile\"}}",
                            path_str, elapsed
                        ));
                        failed += 1;
                    }
                }
            }
            if json {
                println!(
                    "{{\"results\":[{}],\"summary\":{{\"passed\":{},\"failed\":{}}}}}",
                    json_results.join(","),
                    passed,
                    failed
                );
            } else {
                println!();
                println!("{} passed; {} failed", passed, failed);
            }
            if failed > 0 {
                Ok(ExitCode::from(1))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        "fmt" => {
            // Pretty-print canonical source. `// …` comments are
            // preserved (best-effort: trailing same-line comments
            // are promoted to leading; blank lines between comment
            // groups are not preserved in v1).
            //
            // Modes (mutually exclusive):
            //   default:     print formatted source to stdout
            //                (single-file only)
            //   --check:     exit 1 (silent) if any file is not
            //                already canonical; useful for CI
            //   --in-place:  overwrite each file with canonical
            //                source
            //
            // Path args may be files or directories. Directories
            // expand to their `*.vani` children (non-recursive)
            // via the same helper used by `intentc test`.
            let mut check = false;
            let mut in_place = false;
            let mut path_args: Vec<String> = Vec::new();
            for arg in args.iter().skip(2) {
                match arg.as_str() {
                    "--check" => check = true,
                    "--in-place" | "-i" => in_place = true,
                    other if other.starts_with('-') => {
                        return Err(format!(
                            "unknown flag for 'fmt': '{}' (expected --check or --in-place)",
                            other
                        ));
                    }
                    other => path_args.push(other.to_string()),
                }
            }
            if check && in_place {
                return Err("--check and --in-place are mutually exclusive".into());
            }
            if path_args.is_empty() {
                return Err(format!("'fmt' requires a path argument\n\n{}", HELP));
            }
            let files = expand_intent_paths(&path_args)?;
            if files.is_empty() {
                return Err("no .vani files to format".into());
            }
            if files.len() > 1 && !check && !in_place {
                return Err(
                    "multiple files require --check or --in-place \
                     (stdout mode is single-file only)"
                        .into(),
                );
            }

            let mut not_canonical = 0usize;
            for file in &files {
                let source = fs::read_to_string(file).map_err(|error| {
                    format!("failed to read '{}': {}", file.display(), error)
                })?;
                let tokens = vani::lexer::lex(&source).map_err(|diag| {
                    vani::diagnostic::format_diagnostics(
                        file.to_str().unwrap_or("<input>"),
                        &source,
                        &[diag],
                    )
                })?;
                let comments = vani::lexer::extract_comments(&source);
                let (program, diags) = vani::parser::parse(tokens);
                if !diags.is_empty() {
                    return Err(vani::diagnostic::format_diagnostics(
                        file.to_str().unwrap_or("<input>"),
                        &source,
                        &diags,
                    ));
                }
                let formatted = vani::format::format_program_with_comments(
                    &program, &source, &comments,
                );

                if check {
                    if formatted != source {
                        eprintln!("{}: not canonically formatted", file.display());
                        not_canonical += 1;
                    }
                } else if in_place {
                    // Only write if content actually changes — keeps
                    // mtime stable for files already canonical,
                    // making `intentc fmt --in-place examples/`
                    // safe to run repeatedly.
                    if formatted != source {
                        fs::write(file, &formatted).map_err(|e| {
                            format!("failed to write '{}': {}", file.display(), e)
                        })?;
                    }
                } else {
                    print!("{}", formatted);
                }
            }
            if check && not_canonical > 0 {
                Ok(ExitCode::from(1))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        other => Err(format!("unknown command '{}'\n\n{}", other, HELP)),
    }
}

fn required_file(args: &[String], index: usize, command: &str) -> Result<PathBuf, String> {
    let (file, _next_idx) = required_file_at(args, index, command)?;
    Ok(file)
}

/// Like `required_file` but also returns the next arg index
/// to scan from. When a positional file is present at `index`
/// the next index is `index + 1`; when the file comes from
/// `vani.toml` (no positional consumed) the next index is
/// `index` itself so flag parsing sees every remaining arg.
/// Closure #280.
fn required_file_at(
    args: &[String],
    index: usize,
    command: &str,
) -> Result<(PathBuf, usize), String> {
    // Look for the first positional (non-flag) arg, skipping
    // flag pairs `-o PATH` / `--out PATH` / `--link-with PATH`
    // / `--backend=...` etc. Without this, `intentc build -o
    // out` with an implicit manifest entry would mis-read
    // `out` as the source path.
    let mut idx = index;
    while let Some(arg) = args.get(idx) {
        if arg == "-o" || arg == "--out" || arg == "--link-with" {
            idx += 2;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        // Found the positional file at `idx`. The caller
        // should start flag parsing from `idx + 1` (the arg
        // just after).
        return Ok((PathBuf::from(arg), idx + 1));
    }
    // No positional — try manifest discovery. The caller
    // should start flag parsing from `index` (no arg
    // consumed for the source).
    let cwd = std::env::current_dir()
        .map_err(|e| format!("failed to read cwd: {}", e))?;
    if let Some(manifest_path) = vani::manifest::find_manifest(&cwd) {
        let manifest = vani::manifest::load_manifest(&manifest_path)
            .map_err(|e| e.to_string())?;
        return Ok((manifest.entry_path, index));
    }
    Err(format!(
        "'{}' requires a source file argument (or a `vani.toml` \
         manifest with [package].entry in cwd / a parent directory)\n\n{}",
        command, HELP
    ))
}

#[derive(Clone, Copy, Debug)]
enum BackendKind {
    C,
    Llvm,
}

/// Parse `[--backend=<c|llvm>] [-o path | --out path]` for the
/// `emit` subcommand. The legacy `emit-c` alias forces backend=c
/// and rejects --backend to keep its semantics unambiguous.
// FFI follow-up: `intentc build` accepts extra inputs that flow
// straight to the system linker (`cc`). Two shapes:
//   --link-with PATH   add an object/source file (e.g. foo.o, foo.c).
//                      Repeatable. Useful for `extern "C" fn` whose
//                      implementation lives in a separately-compiled
//                      C/C++/Rust translation unit.
//   -l<name>           add a system library (e.g. -lm, -lcurl).
//                      Repeatable. Forwarded verbatim to cc.
// Both are appended after the vāṇी object file in the link line so
// usual link-order rules apply.
// Closure #274: `intentc run` accepts the same link flags as
// `intentc build` (only the C-backend path actually consumes
// them — LLVM-JIT runs through lli's host-symbol resolver and
// can't link extra translation units). Returning the same
// (backend, link_args) shape so the dispatch can validate the
// combination.
fn parse_run_args(
    args: &[String],
    from: usize,
) -> Result<(BackendKind, Vec<String>), String> {
    let mut backend = BackendKind::Llvm;
    let mut link_args: Vec<String> = Vec::new();
    let mut idx = from;
    while let Some(arg) = args.get(idx) {
        if let Some(value) = arg.strip_prefix("--backend=") {
            backend = match value {
                "c" => BackendKind::C,
                "llvm" => BackendKind::Llvm,
                other => return Err(format!("unknown backend '{}': expected c|llvm", other)),
            };
            idx += 1;
        } else if arg == "--link-with" {
            let path = args
                .get(idx + 1)
                .ok_or_else(|| "expected a path after '--link-with'".to_string())?;
            link_args.push(path.clone());
            idx += 2;
        } else if let Some(value) = arg.strip_prefix("--link-with=") {
            link_args.push(value.to_string());
            idx += 1;
        } else if arg.starts_with("-l") && arg.len() > 2 {
            link_args.push(arg.clone());
            idx += 1;
        } else if arg == "-o" || arg == "--out" {
            // `-o` is meaningless for run but the legacy parser
            // accepted it; preserve back-compat by consuming the
            // path arg without using it.
            let _ = args
                .get(idx + 1)
                .ok_or_else(|| format!("expected a path after '{}'", arg))?;
            idx += 2;
        } else {
            return Err(format!("unexpected argument '{}'", arg));
        }
    }
    Ok((backend, link_args))
}

fn parse_build_args(
    args: &[String],
    from: usize,
) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut out: Option<PathBuf> = None;
    let mut link_args: Vec<String> = Vec::new();
    let mut idx = from;
    while let Some(arg) = args.get(idx) {
        if arg == "-o" || arg == "--out" {
            let path = args
                .get(idx + 1)
                .ok_or_else(|| format!("expected a path after '{}'", arg))?;
            out = Some(PathBuf::from(path));
            idx += 2;
        } else if arg == "--link-with" {
            let path = args
                .get(idx + 1)
                .ok_or_else(|| "expected a path after '--link-with'".to_string())?;
            link_args.push(path.clone());
            idx += 2;
        } else if let Some(value) = arg.strip_prefix("--link-with=") {
            link_args.push(value.to_string());
            idx += 1;
        } else if arg.starts_with("-l") && arg.len() > 2 {
            link_args.push(arg.clone());
            idx += 1;
        } else {
            return Err(format!("unexpected argument '{}'", arg));
        }
    }
    Ok((out, link_args))
}

fn parse_emit_args(
    args: &[String],
    from: usize,
    cmd_name: &str,
) -> Result<(BackendKind, Option<PathBuf>), String> {
    // LLVM is now the default — the project's direction is to move
    // away from the C backend. The `emit-c` legacy alias forces C
    // regardless of this default.
    let mut backend = if cmd_name == "emit-c" {
        BackendKind::C
    } else {
        BackendKind::Llvm
    };
    let mut out: Option<PathBuf> = None;
    let mut idx = from;
    while let Some(arg) = args.get(idx) {
        if let Some(value) = arg.strip_prefix("--backend=") {
            if cmd_name == "emit-c" {
                return Err(
                    "'emit-c' forces backend=c; use 'emit --backend=…' to choose"
                        .to_string(),
                );
            }
            backend = match value {
                "c" => BackendKind::C,
                "llvm" => BackendKind::Llvm,
                other => return Err(format!("unknown backend '{}': expected c|llvm", other)),
            };
            idx += 1;
        } else if arg == "-o" || arg == "--out" {
            let path = args
                .get(idx + 1)
                .ok_or_else(|| format!("expected a path after '{}'", arg))?;
            out = Some(PathBuf::from(path));
            idx += 2;
        } else {
            return Err(format!("unexpected argument '{}'", arg));
        }
    }
    Ok((backend, out))
}

fn compile_path_or_report(
    _path: &Path,
) -> Result<vani::checker::CheckedProgram, String> {
    vani::compile_path(_path)
        .map(|(c, _)| c)
        .map_err(|(map, diagnostics)| {
            vani::diagnostic::format_diagnostics_with_files(&map, &diagnostics)
        })
}

fn run_program(path: &Path, link_args: &[String]) -> Result<ExitCode, String> {
    let checked = compile_path_or_report(path)?;
    let c = emit_c_via_ssa(&checked.ir);
    let (c_path, bin_path) = temp_paths(path);

    fs::write(&c_path, c)
        .map_err(|error| format!("failed to write '{}': {}", c_path.display(), error))?;

    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    // Probe once for `-fopenmp` support and add it when available
    // so `parallel for` loops in the source get actual parallelism.
    // Compilers without OpenMP issue an "unknown pragma" warning
    // and run sequentially — also correct (the verifier proved the
    // body is independent of iteration order).
    let openmp_ok = Command::new(&cc)
        .args(["-fopenmp", "-x", "c", "-E", "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let mut cmd = Command::new(&cc);
    cmd.arg(&c_path)
        .arg("-std=c11")
        .arg("-O2");
    // Link pthread on POSIX so the `task` lowering's
    // pthread_create / pthread_join references resolve.
    // glibc folds -lpthread into libc on modern systems;
    // -pthread is the portable spelling and is also a
    // no-op when libgomp already brings pthread in via
    // -fopenmp. On Windows the runtime uses CreateThread
    // (kernel32.lib is linked by default) and
    // WaitOnAddress / WakeByAddressSingle (kernel32 +
    // synchronization.lib).
    if !cfg!(target_os = "windows") {
        cmd.arg("-pthread");
        // Link libm so libm symbols emitted by the math
        // builtins (sqrt / sin / cos / pow / floor / ceil
        // / fabs) resolve at link time. glibc keeps the
        // math functions in libm; modern Apple SDKs / BSDs
        // ship the same set in libm. Windows has the math
        // functions in the C runtime (msvcrt) — no extra
        // flag needed. Closure #299.
        cmd.arg("-lm");
    } else {
        cmd.arg("-lsynchronization");
    }
    if openmp_ok {
        cmd.arg("-fopenmp");
    }
    // Closure #274: user-supplied link inputs (`--link-with PATH`
    // / `-l<name>`) trail the vāṇी source so symbol resolution
    // sees vāṇी's `call abs(...)` first and then the providing
    // object / library.
    for extra in link_args {
        cmd.arg(extra);
    }
    let compile_out = cmd
        .arg("-o")
        .arg(&bin_path)
        .output()
        .map_err(|error| format!("failed to invoke {}: {}", cc, error))?;

    if !compile_out.status.success() {
        return Err(format!(
            "{} failed while compiling '{}' (left at this path for debugging):\n{}",
            cc,
            c_path.display(),
            String::from_utf8_lossy(&compile_out.stderr).trim_end()
        ));
    }

    let run_result = Command::new(&bin_path).status();
    let _ = fs::remove_file(&c_path);
    let _ = fs::remove_file(&bin_path);
    let status = run_result
        .map_err(|error| format!("failed to run '{}': {}", bin_path.display(), error))?;

    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

/// LLVM equivalent of `run_program`. Emits `.ll`, runs it through
/// `lli`, returns the program's exit code. `LLI` env var overrides
/// the default `lli` binary lookup, mirroring `CC` for the C path.
fn run_program_llvm(path: &Path) -> Result<ExitCode, String> {
    let checked = compile_path_or_report(path)?;
    let ll = emit_llvm_via_ssa(&checked.ir);
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("program");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ll_path = env::temp_dir().join(format!("intentc-{}-{}-{}.ll", stem, pid, nanos));
    fs::write(&ll_path, ll)
        .map_err(|error| format!("failed to write '{}': {}", ll_path.display(), error))?;

    let lli = env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let mut cmd = Command::new(&lli);
    // The LLVM backend emits `parallel for` lowerings that call
    // GOMP_parallel / omp_get_thread_num / omp_get_num_threads
    // from libgomp.so. Probe the well-known soname; if present,
    // tell lli to load it so JIT calls resolve. When absent, the
    // OpenMP entries are unresolved but only get called by
    // `parallel for` sites — pure-sequential programs still run.
    add_libgomp_load_flags(&mut cmd);
    // lli's MCJIT isn't thread-safe for concurrent function
    // resolution; cap libgomp to a single thread when JITting so
    // `parallel for` runs serially under the JIT. AOT builds
    // (`intentc build`) leave the env alone and get real
    // parallelism. Users who want JIT'd parallelism can override.
    if env::var("OMP_NUM_THREADS").is_err() {
        cmd.env("OMP_NUM_THREADS", "1");
    }
    cmd.arg(&ll_path);
    let run_result = cmd.status();
    let _ = fs::remove_file(&ll_path);
    let status = run_result
        .map_err(|error| format!("failed to invoke {}: {}", lli, error))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

/// Probe known libgomp.so paths and add `-load=<path>` flags to
/// `cmd` for each one that exists. lli silently ignores duplicates
/// and unknown paths. Order matters only when symbols collide,
/// which they don't between libgomp versions.
fn add_libgomp_load_flags(cmd: &mut Command) {
    const CANDIDATES: &[&str] = &[
        "/usr/lib/x86_64-linux-gnu/libgomp.so.1",
        "/lib/x86_64-linux-gnu/libgomp.so.1",
        "/usr/lib64/libgomp.so.1",
        "/usr/lib/aarch64-linux-gnu/libgomp.so.1",
        // Mac (Homebrew clang's libomp.dylib also works because
        // lli on macOS can resolve both libgomp and libomp).
        "/opt/homebrew/opt/libomp/lib/libomp.dylib",
        "/usr/local/opt/libomp/lib/libomp.dylib",
    ];
    for path in CANDIDATES {
        if std::path::Path::new(path).exists() {
            cmd.arg(format!("-load={}", path));
            return;
        }
    }
    // `INTENT_LIBGOMP` env override for non-standard paths.
    if let Ok(p) = env::var("INTENT_LIBGOMP") {
        if std::path::Path::new(&p).exists() {
            cmd.arg(format!("-load={}", p));
        }
    }
}

/// Drop lli's signal-handler diagnostics from a captured stderr.
/// When an Intent program aborts (failed assert, divisor=0, etc.),
/// lli intercepts SIGABRT and dumps "PLEASE submit a bug report",
/// "Stack dump:", and a long native backtrace. None of that is
/// useful to an Intent user — the line that *is* useful (e.g.
/// `assertion failed: ...`) was printed earlier by the program
/// itself. Truncate at the first lli-internal marker.
/// Resolve a list of CLI args into a flat list of `.vani` files,
/// shared by `intentc test` and `intentc fmt`. Each arg is treated
/// as a file or a directory; a directory expands recursively to
/// every `*.vani` descendant, alphabetized. Dot-prefixed
/// directories (`.git`, `.cargo`, etc.) are skipped.
fn expand_intent_paths(args: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for raw in args {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            walk_intent_files(&path, &mut files).map_err(|e| {
                format!("failed to read directory '{}': {}", path.display(), e)
            })?;
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// Append every `*.vani` file under `dir` to `out`, recursing
/// into subdirectories in alphabetical order. Skips entries whose
/// name starts with `.` so `intentc fmt --check .` doesn't drill
/// into `.git/`, `.cargo/`, etc.
fn walk_intent_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk_intent_files(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("vani") {
            out.push(path);
        }
    }
    Ok(())
}

/// Minimal JSON-string escaping for paths and short reason strings
/// embedded in `intentc test --json` output. Just the basics: `\"`,
/// `\\`, control chars escaped as `\uXXXX`. We don't pull in a
/// JSON-emitter crate for this — the entire `--json` payload is
/// hand-shaped, mirroring `format_diagnostics_json_with_files`.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn trim_lli_backtrace(stderr: &str) -> String {
    const MARKERS: &[&str] = &["PLEASE submit a bug report", "Stack dump:"];
    let mut cut = stderr.len();
    for m in MARKERS {
        if let Some(idx) = stderr.find(m) {
            if idx < cut {
                cut = idx;
            }
        }
    }
    let trimmed = stderr[..cut].trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{}\n", trimmed)
    }
}

/// Like `run_program_llvm` but captures stdout+stderr instead of
/// inheriting the parent's. Returns `(exit_code, stdout, stderr)` so
/// callers (notably `intentc test`) can decide whether to show output.
fn run_program_llvm_capture(path: &Path) -> Result<(i32, String, String), String> {
    let checked = compile_path_or_report(path)?;
    let ll = emit_llvm_via_ssa(&checked.ir);
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("program");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ll_path = env::temp_dir().join(format!("intentc-{}-{}-{}.ll", stem, pid, nanos));
    fs::write(&ll_path, ll)
        .map_err(|error| format!("failed to write '{}': {}", ll_path.display(), error))?;

    let lli = env::var("LLI").unwrap_or_else(|_| "lli".to_string());
    let mut cmd = Command::new(&lli);
    add_libgomp_load_flags(&mut cmd);
    if env::var("OMP_NUM_THREADS").is_err() {
        cmd.env("OMP_NUM_THREADS", "1");
    }
    cmd.arg(&ll_path);
    let output_result = cmd.output();
    let _ = fs::remove_file(&ll_path);
    let out = output_result
        .map_err(|error| format!("failed to invoke {}: {}", lli, error))?;
    Ok((
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// AOT-compile to a native binary via the LLVM backend.
/// Pipeline: emit `.ll` → `llc -filetype=obj` → `.o` → `cc -o` → binary.
/// `out_path` overrides the default (source-stem in the cwd).
fn build_program_llvm(
    path: &Path,
    out_path: Option<&Path>,
    link_args: &[String],
) -> Result<ExitCode, String> {
    let checked = compile_path_or_report(path)?;
    let ll = emit_llvm_via_ssa(&checked.ir);
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("program");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ll_path = env::temp_dir().join(format!("intentc-{}-{}-{}.ll", stem, pid, nanos));
    let opt_path = env::temp_dir().join(format!("intentc-{}-{}-{}.opt.ll", stem, pid, nanos));
    let obj_path = env::temp_dir().join(format!("intentc-{}-{}-{}.o", stem, pid, nanos));
    fs::write(&ll_path, ll)
        .map_err(|error| format!("failed to write '{}': {}", ll_path.display(), error))?;

    // Optional opt(1) pass: promotes our alloca-heavy locals into
    // SSA values (mem2reg), inlines small functions, and folds
    // constants before llc sees the IR. Skipped silently if `opt`
    // is not installed — the build still completes with llc's own
    // optimizer (the -O=2 below).
    let opt = env::var("OPT").unwrap_or_else(|_| "opt".to_string());
    let llc_input = match Command::new(&opt)
        .arg("-O2")
        .arg("-S")
        .arg(&ll_path)
        .arg("-o")
        .arg(&opt_path)
        .output()
    {
        Ok(o) if o.status.success() => opt_path.clone(),
        // `opt` exists but choked: emit the stderr and keep going
        // with the unoptimized IR so the user still gets a binary.
        Ok(o) => {
            eprintln!(
                "warning: {} failed (continuing with unoptimized IR):\n{}",
                opt,
                String::from_utf8_lossy(&o.stderr).trim_end()
            );
            ll_path.clone()
        }
        // Tool missing entirely (cargo + no LLVM dev tools) — same
        // fallback. Don't make `intentc build` require `opt`.
        Err(_) => ll_path.clone(),
    };

    let llc = env::var("LLC").unwrap_or_else(|_| "llc".to_string());
    let llc_out = Command::new(&llc)
        .arg("-filetype=obj")
        .arg("-relocation-model=pic")
        // Default to -O=2. The verifier proves safety upstream so
        // the optimizer is free to assume no UB on the proved paths.
        // Users can override the optimization level by setting LLC
        // to a wrapper script if they need a different level.
        .arg("-O=2")
        .arg("-o")
        .arg(&obj_path)
        .arg(&llc_input)
        .output()
        .map_err(|error| format!("failed to invoke {}: {}", llc, error))?;
    if !llc_out.status.success() {
        let _ = fs::remove_file(&opt_path);
        let _ = fs::remove_file(&ll_path);
        return Err(format!(
            "{} failed while lowering '{}' (left at this path for debugging):\n{}",
            llc,
            llc_input.display(),
            String::from_utf8_lossy(&llc_out.stderr).trim_end()
        ));
    }

    let bin_path: PathBuf = match out_path {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(stem),
    };
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    // Linkage shape depends on host:
    //   - POSIX: the LLVM backend's `parallel for` lowering
    //     calls `@GOMP_parallel` / `@omp_get_*` from libgomp,
    //     so we link via `-fopenmp` (covers gcc + clang).
    //     Programs without parallel-for still link cleanly —
    //     the linker drops the unused dep.
    //   - Windows: libgomp isn't available; parallel-for
    //     open-codes `@CreateThread` (kernel32, auto-linked)
    //     and the mutex fast path calls `@WaitOnAddress` /
    //     `@WakeByAddressSingle` from synchronization.lib —
    //     so we add `-lsynchronization` and skip `-fopenmp`.
    let mut link_cmd = Command::new(&cc);
    link_cmd.arg(&obj_path);
    if cfg!(target_os = "windows") {
        link_cmd.arg("-lsynchronization");
    } else {
        link_cmd.arg("-fopenmp");
    }
    // FFI follow-up: user-supplied link inputs follow the vāṇī
    // object so symbol resolution sees vāṇī's `extern "C" fn` call
    // sites first and then the providing object/library.
    for extra in link_args {
        link_cmd.arg(extra);
    }
    let link_out = link_cmd
        .arg("-o")
        .arg(&bin_path)
        .output()
        .map_err(|error| format!("failed to invoke {}: {}", cc, error))?;
    let _ = fs::remove_file(&ll_path);
    let _ = fs::remove_file(&opt_path);
    let _ = fs::remove_file(&obj_path);
    if !link_out.status.success() {
        return Err(format!(
            "{} failed while linking:\n{}",
            cc,
            String::from_utf8_lossy(&link_out.stderr).trim_end()
        ));
    }
    Ok(ExitCode::SUCCESS)
}

fn temp_paths(source_path: &Path) -> (PathBuf, PathBuf) {
    let stem = source_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("program");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let unique = format!("{}-{}-{}", stem, pid, nanos);
    let c_path = env::temp_dir().join(format!("intentc-{}.c", unique));
    let bin_path = env::temp_dir().join(format!("intentc-{}", unique));
    (c_path, bin_path)
}
