//! SSA-consuming LLVM IR backend (milestone 6g step 5).
//!
//! Emits LLVM IR from an `ssa::Module`. Covers: scalar
//! arithmetic / control flow / direct calls + fn-pointer
//! indirect calls (`FnRef` + `CallIndirect`) + fixed-size
//! arrays (`ArrayLit` + `Index` + `IndexAssign` + `Len`,
//! with stack drop as a no-op) + `Vec<T>` (inline malloc +
//! per-element store + shared runtime helpers from
//! `backend_llvm` for `__push`/`__set`/`__clone`, inline
//! `@free` for drop) + string literals (`StrLit` →
//! `@.str.<n>` global + GEP for `i8*`) + `RefOf`
//! (already-pointer values via bitcast; scalar values via
//! snapshot alloca + store).
//!
//! Hint markers (parallel-for / task begin/end) pass through
//! as no-ops so loop bodies execute sequentially —
//! correctness preserved by the verifier's race-freedom
//! proof; real pthread/libgomp parallelism through SSA is
//! tracked as a separate follow-up.
//!
//! Once "parallel SSA lowering" lands, `intentc` can flip
//! its default backend to consume `ssa::Module` directly
//! (TODO #6).
//!
//! LLVM is true SSA, so block parameters lower directly to
//! `phi` instructions — no `goto` + forward-declare gymnastics
//! like the C backend needs. We compute a predecessor map per
//! block so each parameter's phi can gather its incoming
//! values from every edge.

use std::collections::BTreeMap;

use crate::ast::{BinaryOp, Type, UnaryOp};
use crate::backend_llvm::host_uses_win32_threading;
use crate::ssa::{
    BasicBlock, BlockId, Const, Function, HintKind, InstrKind, Module, Operand,
    Terminator, ValueId,
};
use crate::ssa_backend_c::{
    recognize_parallel_region as recognize_parallel_region_c, ParallelRegion,
};

thread_local! {
    /// Module-scope buffer for emitted string-literal globals.
    /// `StrLit` instructions append a `@.str.<n> = private
    /// constant [<len> x i8] c"…"` declaration here, and
    /// `emit` prepends the buffer to the module output.
    static STR_GLOBALS: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    /// Per-emit counter for unique `.str.N` global names.
    /// Reset at the top of every `emit` call so a fresh
    /// module starts from 0.
    static STR_COUNTER: std::cell::Cell<u32> = std::cell::Cell::new(0);
    /// Module-scope buffer for outlined function definitions
    /// (parallel-for bodies lifted into `@__intent_par_<N>`
    /// helpers). Spliced into the module output after the
    /// preamble + main functions.
    static DEFERRED_FUNCTIONS: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    /// Per-emit counter for outlined-fn names. Reset on
    /// every `emit` call so module-local naming is stable.
    static OUTLINE_COUNTER: std::cell::Cell<u32> = std::cell::Cell::new(0);
    /// Per-function counter for fresh local `%t<n>`
    /// temporaries inside an outlined fn (or any other
    /// "side" emit context that needs its own naming
    /// namespace). Reset before each outlined-fn emit.
    static OUTLINE_TMP_COUNTER: std::cell::Cell<u32> = std::cell::Cell::new(0);
}

/// Emit LLVM IR for the whole module. Returns the assembled
/// text including a small preamble of externs that match the
/// tree-based LLVM backend's runtime expectations (`@printf`,
/// `@malloc`, `@free`, etc.) — programs in the scalar subset
/// rarely use them, but emitting the declarations keeps the
/// output compatible with mixed-feature programs we'll add as
/// coverage grows.
pub fn emit(module: &Module) -> Result<String, EmitError> {
    STR_GLOBALS.with(|b| b.borrow_mut().clear());
    STR_COUNTER.with(|c| c.set(0));
    DEFERRED_FUNCTIONS.with(|b| b.borrow_mut().clear());
    OUTLINE_COUNTER.with(|c| c.set(0));

    // Walk the SSA module for `Type::Vec(T)` element types.
    // Each unique element gets one `%intent_vec_<elt>` struct
    // typedef + runtime helpers (`__from`, `__push`, `__set`,
    // `__clone`, `__free`). Shared with the tree-LLVM
    // backend.
    let mut vec_elements: Vec<Type> = Vec::new();
    let mut vec_seen: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for f in &module.functions {
        for (_, ty, _) in &f.params {
            collect_vec_elt(ty, &mut vec_seen, &mut vec_elements);
        }
        collect_vec_elt(&f.return_type, &mut vec_seen, &mut vec_elements);
        for block in &f.blocks {
            for (_, ty) in &block.params {
                collect_vec_elt(ty, &mut vec_seen, &mut vec_elements);
            }
            for instr in &block.instructions {
                collect_vec_elt(&instr.ty, &mut vec_seen, &mut vec_elements);
            }
        }
    }

    // Build a function-name → (param types, return type)
    // table so Call instructions can recover the per-param
    // LLVM type for Const operands (whose `operand_type`
    // lookup returns None).
    let mut fn_sigs: BTreeMap<String, (Vec<Type>, Type)> = BTreeMap::new();
    for f in &module.functions {
        let param_tys: Vec<Type> =
            f.params.iter().map(|(_, ty, _)| ty.clone()).collect();
        fn_sigs.insert(f.name.clone(), (param_tys, f.return_type.clone()));
    }

    // Emit functions to a side buffer first so module-scope
    // globals (string literals) the bodies trigger can be
    // spliced between the preamble and the function defs.
    let mut functions = String::new();
    for f in &module.functions {
        emit_function(f, &fn_sigs, &mut functions)?;
    }

    let mut out = String::new();
    out.push_str("; ModuleID = 'intent-ssa'\n");
    // Match the externs the tree-based LLVM backend declares.
    // Most scalar programs don't reference any of these; the
    // ones that do (printing, free, etc.) need them resolved
    // at link time via `lli` or `llc + cc`.
    out.push_str("declare i32 @printf(i8*, ...)\n");
    out.push_str("declare i32 @dprintf(i32, i8*, ...)\n");
    out.push_str("declare i32 @putchar(i32)\n");
    out.push_str("declare void @abort() noreturn\n");
    out.push_str("declare i8* @malloc(i64)\n");
    out.push_str("declare i8* @realloc(i8*, i64)\n");
    out.push_str("declare void @free(i8*)\n");
    out.push_str("declare i8* @memcpy(i8*, i8*, i64)\n");
    out.push_str("declare i32 @strcmp(i8*, i8*)\n");
    out.push_str("declare i64 @strlen(i8*)\n");
    // Empty string global used by the per-element Vec
    // clone helper (closure #152) and the payloaded-enum
    // payload clone path to round-trip an OwnedStr
    // through `intent_str_concat` with a zero-length right
    // operand — gives a strdup-like deep copy.
    out.push_str("@.empty_str_clone = private constant [1 x i8] c\"\\00\"\n");
    // Parallel-for runtime. Linux/macOS use libgomp;
    // Windows open-codes a `@CreateThread` fan-out (the
    // outlined fn reads tid/nt from a per-thread arg struct
    // instead of calling `omp_get_*`). Gated by the same
    // `host_uses_win32_threading()` switch as tree-LLVM so
    // both backends pick the same flavor.
    if !host_uses_win32_threading() {
        out.push_str("declare void @GOMP_parallel(void (i8*)*, i8*, i32, i32)\n");
        out.push_str("declare i32 @omp_get_thread_num()\n");
        out.push_str("declare i32 @omp_get_num_threads()\n");
    }
    // Threading runtime used by `task` outlining. POSIX
    // (`@pthread_create`, `@pthread_join`) on Linux/macOS;
    // Win32 (`@CreateThread`, `@WaitForSingleObject`,
    // `@CloseHandle`) on Windows hosts. Driven by the same
    // `host_uses_win32_threading()` gate as tree-LLVM.
    if host_uses_win32_threading() {
        out.push_str("declare i8* @CreateThread(i8*, i64, i8* (i8*)*, i8*, i32, i32*)\n");
        out.push_str("declare i32 @WaitForSingleObject(i8*, i32)\n");
        out.push_str("declare i32 @CloseHandle(i8*)\n");
    } else {
        out.push_str("declare i32 @pthread_create(i64*, i8*, i8* (i8*)*, i8*)\n");
        out.push_str("declare i32 @pthread_join(i64, i8**)\n");
    }
    // Mutex park/wake primitives. POSIX uses libc's
    // generic `syscall(2)` trampoline (variadic) to invoke
    // SYS_futex; Win32 uses `WaitOnAddress` /
    // `WakeByAddressSingle` from the synchronization API
    // (already linked in by `intentc build` via
    // `-lsynchronization`). The C side declares the same
    // shape in its `<linux/futex.h>` / `<synchapi.h>`
    // headers.
    if crate::backend_llvm::host_uses_win32_threading() {
        out.push_str("declare i32 @WaitOnAddress(i8*, i8*, i64, i32)\n");
        out.push_str("declare void @WakeByAddressSingle(i8*)\n");
    } else {
        out.push_str("declare i64 @syscall(i64, ...)\n");
    }
    // `intent_task_handle = { i64, i8* }` — pthread tid
    // alongside the heap-ctx pointer so the matching join
    // can free the ctx after pthread_join returns. Same
    // layout as tree-LLVM's so cross-backend parity holds.
    out.push_str("%intent_task_handle = type { i64, i8* }\n");
    // `Mutex<i64>` / `Guard<i64>` runtime types. The mutex's
    // `locked` field is i32 because that's the width
    // `futex(2)` / `WaitOnAddress` require on every host;
    // the i64 value comes first to keep the natural-aligned
    // layout. Guard holds a back-pointer to its mutex.
    out.push_str("%intent_mutex_i64 = type { i64, i32 }\n");
    out.push_str("%intent_guard_i64 = type { %intent_mutex_i64* }\n\n");

    // Shared `intent_str_concat` runtime helper used by Str
    // `+` lowering. Definition is identical to tree-LLVM's
    // — emit unconditionally; small and may be unused.
    crate::backend_llvm::emit_intent_str_concat_definition(&mut out);

    // Vec struct typedefs + runtime helpers, one per
    // element type referenced in the SSA module. Shared with
    // the tree-LLVM backend (the helpers are emitted IR that
    // calls `@malloc` / `@memcpy` / `@free` — externs already
    // declared above).
    for elt in &vec_elements {
        // The struct decl spells the data-pointer element
        // type. `llvm_type_string` returns the SSA-value
        // form which for arrays is `[N x T]*` (already a
        // pointer); for the in-buffer element slot we want
        // the bare value `[N x T]`. `vec_element_value_str`
        // collapses that. Refines #7 phase 2c.
        out.push_str(&format!(
            "{} = type {{ {}*, i64, i64 }}\n",
            crate::backend_llvm::vec_struct_name(elt),
            crate::backend_llvm::vec_element_value_str(elt),
        ));
    }
    if !vec_elements.is_empty() {
        out.push('\n');
        for elt in &vec_elements {
            crate::backend_llvm::emit_vec_helpers(elt, &mut out);
        }
    }

    // Channel typedefs: walk the module for `Channel<T, N>`
    // specs and emit one struct per unique (T, N).
    let mut chan_seen: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut chan_specs: Vec<(Type, u64)> = Vec::new();
    for f in &module.functions {
        for (_, ty, _) in &f.params {
            collect_channel_specs_in_ty_llvm(ty, &mut chan_seen, &mut chan_specs);
        }
        collect_channel_specs_in_ty_llvm(&f.return_type, &mut chan_seen, &mut chan_specs);
        for block in &f.blocks {
            for (_, ty) in &block.params {
                collect_channel_specs_in_ty_llvm(ty, &mut chan_seen, &mut chan_specs);
            }
            for instr in &block.instructions {
                collect_channel_specs_in_ty_llvm(&instr.ty, &mut chan_seen, &mut chan_specs);
            }
        }
    }
    for (element, capacity) in &chan_specs {
        let struct_ty = crate::backend_llvm::llvm_channel_struct(element, *capacity);
        let slot = crate::backend_llvm::channel_slot_llvm(element);
        // Layout matches tree-LLVM:
        //   { [N x slot] buf, [N x i64] seq, i64 head, i64 tail }
        out.push_str(&format!(
            "{} = type {{ [{} x {}], [{} x i64], i64, i64 }}\n",
            struct_ty, capacity, slot, capacity
        ));
    }
    if !chan_specs.is_empty() {
        out.push('\n');
    }

    // Splice string-literal globals collected during the
    // function pass.
    STR_GLOBALS.with(|b| {
        let s = std::mem::take(&mut *b.borrow_mut());
        if !s.is_empty() {
            out.push_str(&s);
            out.push('\n');
        }
    });

    out.push_str(&functions);

    // Splice outlined parallel-for functions after the
    // module's main functions so they're defined before any
    // `@__intent_par_<N>` reference at a `@GOMP_parallel`
    // call site needs resolution.
    DEFERRED_FUNCTIONS.with(|b| {
        let s = std::mem::take(&mut *b.borrow_mut());
        if !s.is_empty() {
            out.push_str(&s);
        }
    });

    // Re-bind for the trailing main shim below.
    let module = module;

    // `intentc run` invokes lli on this output. The C-shaped
    // entry point is `main()` returning the result of
    // `@fn_main()`; we provide it when the module defines a
    // function named `main`.
    if module.functions.iter().any(|f| f.name == "main") {
        out.push_str("define i32 @main() {\n");
        out.push_str("entry:\n");
        out.push_str("  %r = call i64 @fn_main()\n");
        out.push_str("  %r32 = trunc i64 %r to i32\n");
        out.push_str("  ret i32 %r32\n");
        out.push_str("}\n");
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct EmitError {
    pub message: String,
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ssa-llvm emit: {}", self.message)
    }
}

fn emit_function(
    f: &Function,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
    out: &mut String,
) -> Result<(), EmitError> {
    out.push_str(&format!(
        "define {} @fn_{}(",
        llvm_type_string(&f.return_type)?,
        f.name
    ));
    for (i, (name, ty, vid)) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{} %v_{}", llvm_type_string(ty)?, vid.0));
        let _ = name;
    }
    out.push_str(") {\n");

    // Map each ValueId to its source-language Type so
    // instruction emit can dispatch on shape (e.g., Vec vs
    // Array in a future iteration). Today we don't need it
    // for the scalar subset but threading it now keeps the
    // helper signatures stable as coverage grows.
    let mut value_types: BTreeMap<ValueId, Type> = BTreeMap::new();
    for (_, ty, vid) in &f.params {
        value_types.insert(*vid, ty.clone());
    }
    for block in &f.blocks {
        for (v, ty) in &block.params {
            value_types.insert(*v, ty.clone());
        }
        for instr in &block.instructions {
            value_types.insert(instr.result, instr.ty.clone());
        }
    }

    // Pre-scan for `ParallelForBegin` regions that we can
    // outline into an `@__intent_par_<N>` function called via
    // `@GOMP_parallel`. Reuses SSA-C's recognizer for the
    // canonical-shape check. Regions the recognizer accepts
    // get their `header_block` + `body_block` skipped during
    // the normal block walk; the begin-block's emit branches
    // into `emit_parallel_for_region_llvm` instead.
    let par_regions = collect_parallel_regions_llvm(f)?;
    // Pre-scan for `TaskBegin`/`TaskEnd` pairs (must share a
    // block — single-block-body restriction in v1). Body
    // instructions get lifted into an `@intent_task_<N>`
    // outlined fn; the spawn site emits the
    // pthread_create/CreateThread call, the join site emits
    // pthread_join + free.
    let task_regions = collect_task_regions_llvm(f)?;
    let skip_blocks: std::collections::BTreeSet<BlockId> = par_regions
        .iter()
        .flat_map(|r| {
            // Closure #187 added step_block — skip it too;
            // it's absorbed into the outlined fn's update
            // arithmetic, not emitted as a free-standing
            // LLVM basic block.
            [
                r.shape.header_block,
                r.shape.body_block,
                r.shape.step_block,
            ]
        })
        .collect();
    let par_by_begin: std::collections::BTreeMap<BlockId, &ParallelRegion> =
        par_regions.iter().map(|r| (r.begin_block, r)).collect();

    // Build the predecessor map and rewire any edges that
    // crossed a skipped block. The IR says the exit_block's
    // predecessor is the header_block (via Branch); after
    // outlining, the actual emitted edge is begin_block →
    // exit_block. Remove header's edge to exit and skip
    // body's back-edge to header. The pre-header's IR Jump
    // to header is similarly skipped; we replace the header
    // edge with a phi entry from begin (carrying the same
    // exit-args).
    let mut predecessors = compute_predecessors(f);
    if !par_regions.is_empty() {
        // Drop every predecessor entry sourced from a skipped
        // block. After this, the exit block of each parallel
        // region has no predecessors at the IR level (we just
        // removed its header edge), so no phi nodes get
        // emitted for its block params; those params are
        // instead materialized as `%v_<id> = …` instructions
        // in the begin block before the `br label %bb<exit>`
        // — exit dominates only through begin, so LLVM IR
        // SSA name resolution finds them.
        for preds in predecessors.values_mut() {
            preds.retain(|(src, _)| !skip_blocks.contains(src));
        }
    }
    // For multi-block task regions, the body's intermediate
    // blocks live in the outlined fn now; their phi
    // contributions to end_block need to be dropped from the
    // parent's predecessor map. begin_block's terminator
    // also no longer flows naturally (replaced by our
    // synthetic `br bb<end>` in the spawn-site emit); drop
    // those edges too. After filtering, end_block of every
    // multi-block region has zero parent-side predecessors
    // → no phi nodes get emitted for its body-internal
    // block params, and those params remain unused (the
    // post-TaskEnd code is independent of body-internal
    // values).
    let multi_block_skip_sources: std::collections::BTreeSet<BlockId> = task_regions
        .iter()
        .filter(|r| r.begin_block != r.end_block)
        .flat_map(|r| r.body_blocks.iter().copied())
        .collect();
    if !multi_block_skip_sources.is_empty() {
        for preds in predecessors.values_mut() {
            preds.retain(|(src, _)| !multi_block_skip_sources.contains(src));
        }
    }

    // Emit `entry:` if the function's entry block isn't at
    // index 0 — the LLVM convention is for the first labeled
    // block to be the entry, so a quick `br` from `entry`
    // bridges the gap.
    if f.entry.0 != 0 {
        out.push_str("entry:\n");
        out.push_str(&format!("  br label %bb{}\n", f.entry.0));
    }

    // Intermediate task-body blocks (everything between
    // begin and end, exclusive) get fully absorbed into the
    // outlined task fn; emit nothing for them in the parent.
    let task_fully_skipped: std::collections::BTreeSet<BlockId> = task_regions
        .iter()
        .flat_map(|r| {
            r.body_blocks
                .iter()
                .filter(move |b| **b != r.begin_block && **b != r.end_block)
                .copied()
        })
        .collect();
    for block in &f.blocks {
        if skip_blocks.contains(&block.id) {
            // Header / body of a recognized parallel-for
            // region — absorbed into the begin-block's
            // outlined emit. Emitting them here would
            // produce dead labels.
            continue;
        }
        if task_fully_skipped.contains(&block.id) {
            continue;
        }
        out.push_str(&format!("bb{}:\n", block.id.0));
        // Phi nodes: one per block parameter. Skip if the
        // block has no predecessors (entry block of the
        // function); its params would be the function's
        // formal parameters but those are SSA values defined
        // in the function's parameter list, so they have no
        // phi.
        let preds = predecessors.get(&block.id).cloned().unwrap_or_default();
        if !preds.is_empty() {
            for (param_idx, (param_vid, param_ty)) in block.params.iter().enumerate() {
                let ty_str = llvm_type_string(param_ty)?;
                let pairs: Vec<String> = preds
                    .iter()
                    .map(|(pred_id, args)| {
                        let arg = args
                            .get(param_idx)
                            .map(operand_str)
                            .unwrap_or_else(|| "undef".to_string());
                        format!("[{}, %bb{}]", arg, pred_id.0)
                    })
                    .collect();
                out.push_str(&format!(
                    "  %v_{} = phi {} {}\n",
                    param_vid.0,
                    ty_str,
                    pairs.join(", ")
                ));
            }
        }
        if let Some(region) = par_by_begin.get(&block.id) {
            emit_parallel_for_region_llvm(
                f,
                block,
                region,
                &value_types,
                fn_sigs,
                out,
            )?;
            // The region emit branches to the exit block at
            // the bottom — the IR-level terminator
            // (`Jump(header)`) is replaced. Skip the
            // standard terminator emit.
            continue;
        }
        let terminator_skipped = emit_block_instructions(
            block,
            f,
            &task_regions,
            &value_types,
            fn_sigs,
            out,
        )?;
        if !terminator_skipped {
            emit_terminator(&block.terminator, &f.return_type, &value_types, out)?;
        }
    }
    out.push_str("}\n\n");
    Ok(())
}

/// Walk a block's instructions, dispatching `TaskBegin` /
/// `TaskEnd` / `TaskJoin` hints to the task-outlining emit
/// path (and skipping the body instructions that belong to
/// the outlined fn). All other instructions go through the
/// standard `emit_instr`. Single-block task bodies are
/// required — multi-block bodies aren't recognized in v1
/// and surface via `EmitError` from `collect_task_regions`.
/// Returns `Ok(true)` when the caller should skip the
/// surrounding block's terminator emit — multi-block tasks
/// inject their own `br label %bb<end>` terminator that
/// supersedes the original (which goes to intermediate body
/// blocks now in the outlined fn).
fn emit_block_instructions(
    block: &BasicBlock,
    f: &Function,
    task_regions: &[TaskRegion],
    value_types: &BTreeMap<ValueId, Type>,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
    out: &mut String,
) -> Result<bool, EmitError> {
    let task_by_begin: std::collections::BTreeMap<(BlockId, usize), &TaskRegion> =
        task_regions
            .iter()
            .map(|r| ((r.begin_block, r.begin_idx), r))
            .collect();
    let task_join_by_loc: std::collections::BTreeMap<(BlockId, usize), &TaskRegion> =
        task_regions
            .iter()
            .map(|r| ((r.join_block, r.join_idx), r))
            .collect();
    // Skip-set for body instructions absorbed into the
    // outlined fn:
    //   - Single-block task: skip (begin..=end] in
    //     begin_block.
    //   - Multi-block: in begin_block, skip (begin..end_of_block].
    //     In end_block, skip [0..=end_idx].
    //   - Intermediate body blocks are caught at the
    //     parent's block-walk level (see
    //     `task_fully_skipped`).
    let mut skip_set: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();
    for region in task_regions {
        if region.begin_block == block.id {
            let upper = if region.begin_block == region.end_block {
                region.end_idx
            } else {
                block.instructions.len().saturating_sub(1)
            };
            for i in region.begin_idx..=upper {
                if i != region.begin_idx {
                    skip_set.insert(i);
                }
            }
        }
        if region.end_block == block.id && region.begin_block != region.end_block {
            for i in 0..=region.end_idx {
                skip_set.insert(i);
            }
        }
    }
    let mut terminator_skipped = false;
    for (idx, instr) in block.instructions.iter().enumerate() {
        if skip_set.contains(&idx) {
            continue;
        }
        if let Some(region) = task_by_begin.get(&(block.id, idx)) {
            let multi =
                emit_task_spawn_region_llvm(f, region, value_types, fn_sigs, out)?;
            if multi {
                terminator_skipped = true;
            }
            continue;
        }
        if let Some(region) = task_join_by_loc.get(&(block.id, idx)) {
            emit_task_join_llvm(region, out);
            continue;
        }
        emit_instr(instr, value_types, fn_sigs, out)?;
    }
    Ok(terminator_skipped)
}

/// One recognized task region: a `TaskBegin`/`TaskEnd` pair
/// in the same block, with a matching `TaskJoin` somewhere
/// in the function. The body instructions (those between
/// the Begin and End hints, exclusive) get lifted into an
/// outlined `@intent_task_<N>` function; the parent emits a
/// pthread_create at the Begin site and pthread_join + free
/// at the Join site. Multi-block task bodies (with control
/// flow inside the `task { … }` body) are unsupported in
/// v1 and surface as `EmitError`.
struct TaskRegion {
    handle: String,
    /// Spawn-site location: instruction index of the
    /// `TaskBegin` hint in `begin_block`.
    begin_block: BlockId,
    begin_idx: usize,
    /// Block holding the matching `TaskEnd` hint. Equal to
    /// `begin_block` for single-block bodies; differs for
    /// task bodies that contain `if`/`while`/etc.
    end_block: BlockId,
    end_idx: usize,
    /// Every block in `f.blocks` order belonging to the
    /// body: begin_block first, then any intermediate
    /// blocks, then end_block. Single-block bodies have
    /// `[begin_block]`.
    body_blocks: Vec<BlockId>,
    /// Join-site location: `TaskJoin` hint matched by
    /// `handle`.
    join_block: BlockId,
    join_idx: usize,
    /// Outline-fn id (counter assigned once per region).
    outline_id: u32,
}

fn collect_task_regions_llvm(f: &Function) -> Result<Vec<TaskRegion>, EmitError> {
    let mut regions: Vec<TaskRegion> = Vec::new();
    // Pass 1: find Begin/End pairs (same-block check).
    struct PendingBegin {
        handle: String,
        begin_block: BlockId,
        begin_idx: usize,
    }
    let mut pending: Vec<PendingBegin> = Vec::new();
    for block in &f.blocks {
        for (idx, instr) in block.instructions.iter().enumerate() {
            match &instr.kind {
                InstrKind::Hint(HintKind::TaskBegin { handle }) => {
                    pending.push(PendingBegin {
                        handle: handle.clone(),
                        begin_block: block.id,
                        begin_idx: idx,
                    });
                }
                InstrKind::Hint(HintKind::TaskEnd { handle }) => {
                    let begin = pending.pop().ok_or_else(|| EmitError {
                        message: format!(
                            "TaskEnd `{}` with no matching TaskBegin",
                            handle
                        ),
                    })?;
                    if begin.handle != *handle {
                        return Err(EmitError {
                            message: format!(
                                "TaskEnd `{}` doesn't match outermost TaskBegin `{}`",
                                handle, begin.handle
                            ),
                        });
                    }
                    // CFG-reachability for body_blocks
                    // (mirror of ssa_backend_c.rs's closure
                    // #191 fix). The earlier `(begin_id..=
                    // end_id)` range missed blocks whose ID
                    // was greater than end_id but which
                    // still belonged to the task body
                    // (post-end step blocks from closures
                    // #185/#187 etc.).
                    let body_blocks: Vec<BlockId> = {
                        let mut visited = std::collections::BTreeSet::new();
                        let mut stack = vec![begin.begin_block];
                        while let Some(bid) = stack.pop() {
                            if !visited.insert(bid) {
                                continue;
                            }
                            if bid == block.id {
                                continue;
                            }
                            if (bid.0 as usize) >= f.blocks.len() {
                                continue;
                            }
                            let blk = &f.blocks[bid.0 as usize];
                            match &blk.terminator {
                                Terminator::Jump { target, .. } => {
                                    stack.push(*target);
                                }
                                Terminator::Branch {
                                    then_block,
                                    else_block,
                                    ..
                                } => {
                                    stack.push(*then_block);
                                    stack.push(*else_block);
                                }
                                _ => {}
                            }
                        }
                        visited.insert(block.id);
                        visited.into_iter().collect()
                    };
                    regions.push(TaskRegion {
                        handle: handle.clone(),
                        begin_block: begin.begin_block,
                        begin_idx: begin.begin_idx,
                        end_block: block.id,
                        end_idx: idx,
                        body_blocks,
                        join_block: BlockId(0), // filled below
                        join_idx: 0,
                        outline_id: 0, // filled at emit time
                    });
                }
                _ => {}
            }
        }
    }
    if !pending.is_empty() {
        return Err(EmitError {
            message: "unclosed TaskBegin hints".to_string(),
        });
    }
    // Pass 2: match each region with its TaskJoin.
    for region in regions.iter_mut() {
        let mut found = false;
        'outer: for block in &f.blocks {
            for (idx, instr) in block.instructions.iter().enumerate() {
                if let InstrKind::Hint(HintKind::TaskJoin { handle }) = &instr.kind {
                    if handle == &region.handle {
                        region.join_block = block.id;
                        region.join_idx = idx;
                        found = true;
                        break 'outer;
                    }
                }
            }
        }
        if !found {
            return Err(EmitError {
                message: format!(
                    "task `{}` has no matching TaskJoin",
                    region.handle
                ),
            });
        }
    }
    // Assign outline IDs (stable order by recognition).
    for region in regions.iter_mut() {
        region.outline_id = OUTLINE_COUNTER.with(|c| {
            let v = c.get();
            c.set(v + 1);
            v
        });
    }
    Ok(regions)
}

/// Emit the spawn-site for a task region. Marshals captures
/// through a heap-allocated ctx struct, emits the outlined
/// function to `DEFERRED_FUNCTIONS`, and calls
/// `@pthread_create` (POSIX) or `@CreateThread` (Win32).
/// Mirrors tree-LLVM's `emit_task_via_pthread` but consumes
/// the SSA block-instruction shape.
/// Returns `Ok(true)` when this is a multi-block task —
/// caller skips the surrounding block's original terminator
/// because the spawn-site emit injected `br bb<end>`.
fn emit_task_spawn_region_llvm(
    f: &Function,
    region: &TaskRegion,
    value_types: &BTreeMap<ValueId, Type>,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
    out: &mut String,
) -> Result<bool, EmitError> {
    let multi_block_task = region.begin_block != region.end_block;
    // Compute the per-block body-instruction slice ranges so
    // capture analysis sees every body-internal instruction.
    let slice_ranges: Vec<(BlockId, usize, usize)> = region
        .body_blocks
        .iter()
        .map(|&bid| {
            let block = &f.blocks[bid.0 as usize];
            let (lo, hi) = if bid == region.begin_block && bid == region.end_block {
                (region.begin_idx + 1, region.end_idx)
            } else if bid == region.begin_block {
                (region.begin_idx + 1, block.instructions.len())
            } else if bid == region.end_block {
                (0, region.end_idx)
            } else {
                (0, block.instructions.len())
            };
            (bid, lo, hi)
        })
        .collect();
    // Build body_defined: every block param + every body
    // instruction's result across body_blocks.
    let mut body_defined: std::collections::BTreeSet<ValueId> =
        std::collections::BTreeSet::new();
    for &(bid, lo, hi) in &slice_ranges {
        let block = &f.blocks[bid.0 as usize];
        for (v, _) in &block.params {
            body_defined.insert(*v);
        }
        for i in lo..hi {
            body_defined.insert(block.instructions[i].result);
        }
    }
    let mut captures: Vec<(ValueId, Type)> = Vec::new();
    let mut capture_seen: std::collections::BTreeSet<ValueId> =
        std::collections::BTreeSet::new();
    let collect_capture = |v: ValueId, captures: &mut Vec<(ValueId, Type)>, capture_seen: &mut std::collections::BTreeSet<ValueId>| -> Result<(), EmitError> {
        if body_defined.contains(&v) {
            return Ok(());
        }
        if !capture_seen.insert(v) {
            return Ok(());
        }
        let ty = value_types.get(&v).cloned().ok_or_else(|| EmitError {
            message: format!(
                "task body captures v_{} but type is unknown",
                v.0
            ),
        })?;
        captures.push((v, ty));
        Ok(())
    };
    for &(bid, lo, hi) in &slice_ranges {
        let block = &f.blocks[bid.0 as usize];
        for i in lo..hi {
            for op in instr_operands(&block.instructions[i].kind) {
                if let Operand::Value(v) = op {
                    collect_capture(*v, &mut captures, &mut capture_seen)?;
                }
            }
        }
        // Non-final-block terminators are also part of the
        // body — their operands may reference captures.
        if multi_block_task && bid != region.end_block {
            match &block.terminator {
                Terminator::Jump { args, .. } => {
                    for op in args {
                        if let Operand::Value(v) = op {
                            collect_capture(*v, &mut captures, &mut capture_seen)?;
                        }
                    }
                }
                Terminator::Branch { cond, then_args, else_args, .. } => {
                    if let Operand::Value(v) = cond {
                        collect_capture(*v, &mut captures, &mut capture_seen)?;
                    }
                    for op in then_args.iter().chain(else_args.iter()) {
                        if let Operand::Value(v) = op {
                            collect_capture(*v, &mut captures, &mut capture_seen)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let fn_name = format!("intent_task_{}", region.outline_id);
    let capture_field_tys: Vec<String> = captures
        .iter()
        .map(|(_, ty)| llvm_type_string(ty))
        .collect::<Result<Vec<_>, _>>()?;
    let ctx_struct_ty = if capture_field_tys.is_empty() {
        // LLVM doesn't permit a literal empty struct
        // {`{}`} in a typed expression context. Use a
        // single-field struct holding a dummy i8 to keep
        // the IR well-typed when there are no captures.
        "{ i8 }".to_string()
    } else {
        format!("{{ {} }}", capture_field_tys.join(", "))
    };

    // Spawn-site: alloca handle, malloc ctx (heap so it
    // survives parent's stack frame), store captures, fire
    // pthread_create / CreateThread.
    let id = region.outline_id;
    let handle_addr = format!("%task_{}_h", region.handle);
    out.push_str(&format!(
        "  {} = alloca %intent_task_handle\n",
        handle_addr
    ));
    // Heap-allocate the ctx so the outlined fn can read it
    // after this function returns. Free at join.
    let ctx_size_bytes = if capture_field_tys.is_empty() {
        // 1 byte for the dummy field.
        1
    } else {
        // Conservative upper bound: sum of i64-sized
        // captures. Actually for byte-accurate sizing we'd
        // use getelementptr + ptrtoint trickery. For v1
        // assume 8 bytes per capture (works for our scalar
        // + ptr capture types). Round up to multiple of 8.
        capture_field_tys.len() * 8
    };
    let ctx_raw_name = format!("%task_{}_ctx_raw", region.handle);
    out.push_str(&format!(
        "  {} = call i8* @malloc(i64 {})\n",
        ctx_raw_name, ctx_size_bytes
    ));
    let ctx_typed_name = format!("%task_{}_ctx", region.handle);
    out.push_str(&format!(
        "  {} = bitcast i8* {} to {}*\n",
        ctx_typed_name, ctx_raw_name, ctx_struct_ty
    ));
    // Store each capture into its ctx field.
    for (i, ((cap_v, _), cap_ty_str)) in
        captures.iter().zip(capture_field_tys.iter()).enumerate()
    {
        let cap_field = format!("%task_{}_cap_{}_p", region.handle, i);
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* {}, i32 0, i32 {}\n",
            cap_field, ctx_struct_ty, ctx_struct_ty, ctx_typed_name, i
        ));
        out.push_str(&format!(
            "  store {} %v_{}, {}* {}\n",
            cap_ty_str, cap_v.0, cap_ty_str, cap_field
        ));
    }
    // Get the thread-id slot from the handle alloca.
    let tid_field = format!("%task_{}_tid_p", region.handle);
    out.push_str(&format!(
        "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 0\n",
        tid_field, handle_addr
    ));
    // Spawn (POSIX vs Win32 dispatch).
    if host_uses_win32_threading() {
        let h = format!("%task_{}_h_handle", region.handle);
        out.push_str(&format!(
            "  {} = call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @{}, i8* {}, i32 0, i32* null)\n",
            h, fn_name, ctx_raw_name
        ));
        let h_int = format!("%task_{}_h_int", region.handle);
        out.push_str(&format!(
            "  {} = ptrtoint i8* {} to i64\n",
            h_int, h
        ));
        out.push_str(&format!(
            "  store i64 {}, i64* {}\n",
            h_int, tid_field
        ));
    } else {
        let _spawn_ret = format!("%task_{}_spawn_ret", region.handle);
        out.push_str(&format!(
            "  {} = call i32 @pthread_create(i64* {}, i8* null, i8* (i8*)* @{}, i8* {})\n",
            _spawn_ret, tid_field, fn_name, ctx_raw_name
        ));
    }
    // Stash the ctx pointer in handle.ctx so join can free it.
    let ctx_field = format!("%task_{}_ctx_p", region.handle);
    out.push_str(&format!(
        "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 1\n",
        ctx_field, handle_addr
    ));
    out.push_str(&format!(
        "  store i8* {}, i8** {}\n",
        ctx_raw_name, ctx_field
    ));

    // --- Outlined function emit ---
    emit_outlined_task(
        &fn_name,
        f,
        region,
        &ctx_struct_ty,
        &captures,
        &capture_field_tys,
        value_types,
        fn_sigs,
    )?;
    let _ = id;
    // For multi-block tasks, the parent emits its own
    // `br bb<end>` terminator (overriding the begin_block's
    // original Jump/Branch which went into the body). Tell
    // the caller to skip the standard terminator emit.
    if multi_block_task {
        out.push_str(&format!(
            "  br label %bb{}\n",
            region.end_block.0
        ));
    }
    Ok(multi_block_task)
}

fn emit_outlined_task(
    fn_name: &str,
    f: &Function,
    region: &TaskRegion,
    ctx_struct_ty: &str,
    captures: &[(ValueId, Type)],
    capture_field_tys: &[String],
    value_types: &BTreeMap<ValueId, Type>,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
) -> Result<(), EmitError> {
    let multi_block = region.begin_block != region.end_block;
    let mut out = String::new();
    out.push_str(&format!(
        "define internal i8* @{}(i8* %_ctx) {{\n",
        fn_name
    ));
    out.push_str("entry:\n");
    out.push_str(&format!(
        "  %ctx = bitcast i8* %_ctx to {}*\n",
        ctx_struct_ty
    ));
    // Capture loads (tree-LLVM-compatible naming).
    for (i, ((cap_v, cap_ty), cap_ty_str)) in
        captures.iter().zip(capture_field_tys.iter()).enumerate()
    {
        let cap_field = format!("%cap_{}_p", i);
        let cap_loaded = format!("%cap_{}", i);
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* %ctx, i32 0, i32 {}\n",
            cap_field, ctx_struct_ty, ctx_struct_ty, i
        ));
        out.push_str(&format!(
            "  {} = load {}, {}* {}\n",
            cap_loaded, cap_ty_str, cap_ty_str, cap_field
        ));
        let alias_emit = match cap_ty {
            Type::Bool => format!("  %v_{} = or i1 false, {}\n", cap_v.0, cap_loaded),
            Type::F32 => format!(
                "  %v_{} = fadd float 0.0, {}\n",
                cap_v.0, cap_loaded
            ),
            Type::F64 => format!(
                "  %v_{} = fadd double 0.0, {}\n",
                cap_v.0, cap_loaded
            ),
            t if t.is_integer() => format!(
                "  %v_{} = add {} 0, {}\n",
                cap_v.0, cap_ty_str, cap_loaded
            ),
            _ => format!(
                "  %v_{} = bitcast {} {} to {}\n",
                cap_v.0, cap_ty_str, cap_loaded, cap_ty_str
            ),
        };
        out.push_str(&alias_emit);
    }
    // entry: jump to the body's first block. For
    // single-block tasks we just continue inline (no jump,
    // no label), matching the existing emit.
    if multi_block {
        out.push_str(&format!(
            "  br label %bb{}\n",
            region.begin_block.0
        ));
    }
    // outline_value_types = captures + every body block
    // param/instruction's type + scoped scalars from the
    // parent (for `operand_type` fallbacks).
    let mut outline_value_types: BTreeMap<ValueId, Type> = BTreeMap::new();
    for (v, ty) in captures {
        outline_value_types.insert(*v, ty.clone());
    }
    for &bid in &region.body_blocks {
        let block = &f.blocks[bid.0 as usize];
        for (v, ty) in &block.params {
            outline_value_types.insert(*v, ty.clone());
        }
        for instr in &block.instructions {
            outline_value_types.insert(instr.result, instr.ty.clone());
        }
    }
    for (k, v) in value_types {
        outline_value_types.entry(*k).or_insert_with(|| v.clone());
    }
    // Restrict predecessors to body blocks (so phi nodes
    // inside the outlined fn only reference predecessors
    // that also live there).
    let body_set: std::collections::BTreeSet<BlockId> =
        region.body_blocks.iter().copied().collect();
    let all_preds = compute_predecessors(f);
    // Emit each body block.
    for &bid in &region.body_blocks {
        let block = &f.blocks[bid.0 as usize];
        if multi_block {
            out.push_str(&format!("bb{}:\n", bid.0));
        }
        // Phi for block params — predecessors restricted to
        // body blocks. begin_block has no body-internal
        // predecessor (entry branches to it), so it gets no
        // phi.
        if multi_block && bid != region.begin_block {
            let preds = all_preds
                .get(&bid)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|(src, _)| body_set.contains(src))
                .collect::<Vec<_>>();
            if !preds.is_empty() {
                for (param_idx, (param_vid, param_ty)) in block.params.iter().enumerate() {
                    let ty_str = llvm_type_string(param_ty)?;
                    let pairs: Vec<String> = preds
                        .iter()
                        .map(|(pred_id, args)| {
                            let arg = args
                                .get(param_idx)
                                .map(operand_str)
                                .unwrap_or_else(|| "undef".to_string());
                            format!("[{}, %bb{}]", arg, pred_id.0)
                        })
                        .collect();
                    out.push_str(&format!(
                        "  %v_{} = phi {} {}\n",
                        param_vid.0,
                        ty_str,
                        pairs.join(", ")
                    ));
                }
            }
        }
        // Instructions: skip [<= begin_idx] in begin_block
        // and [>= end_idx] in end_block.
        let (lo, hi) = if bid == region.begin_block && bid == region.end_block {
            (region.begin_idx + 1, region.end_idx)
        } else if bid == region.begin_block {
            (region.begin_idx + 1, block.instructions.len())
        } else if bid == region.end_block {
            (0, region.end_idx)
        } else {
            (0, block.instructions.len())
        };
        for i in lo..hi {
            emit_instr(&block.instructions[i], &outline_value_types, fn_sigs, &mut out)?;
        }
        // Terminator: end_block ends with `ret i8* null`;
        // non-final body blocks emit their original
        // terminator (which jumps to other body blocks).
        if !multi_block || bid == region.end_block {
            out.push_str("  ret i8* null\n");
        } else {
            emit_terminator(&block.terminator, &Type::I64, &outline_value_types, &mut out)?;
        }
    }
    out.push_str("}\n\n");
    DEFERRED_FUNCTIONS.with(|b| b.borrow_mut().push_str(&out));
    Ok(())
}

/// Emit the join-site: load the handle's thread-id, call
/// `pthread_join` (POSIX) / `WaitForSingleObject +
/// CloseHandle` (Win32), then free the heap ctx.
fn emit_task_join_llvm(region: &TaskRegion, out: &mut String) {
    let handle_addr = format!("%task_{}_h", region.handle);
    let tid_p = format!("%task_{}_join_tid_p", region.handle);
    out.push_str(&format!(
        "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 0\n",
        tid_p, handle_addr
    ));
    let tid_v = format!("%task_{}_join_tid", region.handle);
    out.push_str(&format!(
        "  {} = load i64, i64* {}\n",
        tid_v, tid_p
    ));
    if host_uses_win32_threading() {
        let h = format!("%task_{}_join_h", region.handle);
        out.push_str(&format!(
            "  {} = inttoptr i64 {} to i8*\n",
            h, tid_v
        ));
        let _wait = format!("%task_{}_join_wait", region.handle);
        out.push_str(&format!(
            "  {} = call i32 @WaitForSingleObject(i8* {}, i32 -1)\n",
            _wait, h
        ));
        let _close = format!("%task_{}_join_close", region.handle);
        out.push_str(&format!(
            "  {} = call i32 @CloseHandle(i8* {})\n",
            _close, h
        ));
    } else {
        let _join_ret = format!("%task_{}_join_ret", region.handle);
        out.push_str(&format!(
            "  {} = call i32 @pthread_join(i64 {}, i8** null)\n",
            _join_ret, tid_v
        ));
    }
    // Free the heap ctx.
    let ctx_p = format!("%task_{}_join_ctx_p", region.handle);
    out.push_str(&format!(
        "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 1\n",
        ctx_p, handle_addr
    ));
    let ctx_v = format!("%task_{}_join_ctx", region.handle);
    out.push_str(&format!(
        "  {} = load i8*, i8** {}\n",
        ctx_v, ctx_p
    ));
    out.push_str(&format!("  call void @free(i8* {})\n", ctx_v));
}

/// Walk every block's instructions for a `ParallelForBegin`
/// hint and call SSA-C's shared shape recognizer. We wrap
/// `ssa_backend_c::EmitError` in our own error type so the
/// fallback path in `main.rs` sees a uniform Result.
fn collect_parallel_regions_llvm(
    f: &Function,
) -> Result<Vec<ParallelRegion>, EmitError> {
    let mut out = Vec::new();
    for block in &f.blocks {
        for instr in &block.instructions {
            if let InstrKind::Hint(HintKind::ParallelForBegin { reductions, shape }) =
                &instr.kind
            {
                let region = recognize_parallel_region_c(f, block.id, shape, reductions)
                    .map_err(|e| EmitError {
                        message: format!("{}", e),
                    })?;
                out.push(region);
            }
        }
    }
    Ok(out)
}

/// Emit a parallel-for region as `@__intent_par_<N>` +
/// `@GOMP_parallel`. v1 covers the simplest case only —
/// no captures, no reductions, single-block body. Anything
/// more surfaces `EmitError` (→ tree-LLVM fallback in
/// `main.rs::emit_llvm_via_ssa`).
fn emit_parallel_for_region_llvm(
    f: &Function,
    pre_header: &BasicBlock,
    region: &ParallelRegion,
    value_types: &BTreeMap<ValueId, Type>,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
    out: &mut String,
) -> Result<(), EmitError> {
    // Reduction op + element-type gate. The supported set
    // mirrors tree-LLVM's reduction op table; anything
    // outside falls back to tree-LLVM via `EmitError`.
    for (_, op, red_ty) in &region.reductions {
        let ok = match op {
            // `+` works at any integer width via `atomicrmw add`.
            crate::ast::ReductionOp::Add => is_supported_int(red_ty),
            // `*` needs a `cmpxchg` retry loop because
            // `atomicrmw` doesn't expose multiplication.
            crate::ast::ReductionOp::Mul => is_supported_int(red_ty),
            // Bool `&&` / `||` reductions are widened through
            // an i8 shadow in the parent and use
            // `atomicrmw and/or` on the i8 width (i1 isn't
            // byte-aligned so the native form is rejected).
            crate::ast::ReductionOp::And | crate::ast::ReductionOp::Or => {
                matches!(red_ty, Type::Bool)
            }
            // Bitwise integer reductions go through native-
            // width `atomicrmw and/or/xor`.
            crate::ast::ReductionOp::BitAnd
            | crate::ast::ReductionOp::BitOr
            | crate::ast::ReductionOp::BitXor => is_supported_int(red_ty),
            // Min/Max use signed (`smin`/`smax`) or unsigned
            // (`umin`/`umax`) `atomicrmw` per the element's
            // signedness.
            crate::ast::ReductionOp::Min | crate::ast::ReductionOp::Max => {
                is_supported_int(red_ty)
            }
        };
        if !ok {
            return Err(EmitError {
                message: format!(
                    "SSA-LLVM parallel-for: unsupported reduction op {:?} on {:?}",
                    op, red_ty
                ),
            });
        }
    }
    // Closure #252: SSA-LLVM only optimizes single-block
    // parallel-for bodies today. Multi-block bodies (which the
    // recognizer accepts via #241 since 2026-05-26) need
    // Phi-traceback to find the actual `+`/`*`/etc. update
    // operation inside the conditional branch — the back-edge
    // arg is a block-param (`v_<merge>` Phi result), not the
    // arithmetic instruction itself. SSA-C handles this via
    // labels + gotos in #251, but SSA-LLVM's atomicrmw-based
    // reduction strategy needs to identify where the update
    // physically lives so it can replace it in place. That
    // analysis is deferred — the tree-LLVM fallback (already
    // wired via `emit_llvm_via_ssa` in main.rs) handles multi-
    // block bodies correctly using GOMP's non-atomic reduction
    // combine. Surfacing the gate as a clear EmitError keeps
    // the fallback automatic and gives a precise reason in
    // debug builds.
    if region.region_blocks.len() != 1 {
        return Err(EmitError {
            message: format!(
                "SSA-LLVM parallel-for: body has {} blocks (multi-block); \
                 falling back to tree-LLVM. Single-block bodies (no internal \
                 control flow) are the only shape SSA-LLVM lowers to \
                 atomicrmw directly today.",
                region.region_blocks.len()
            ),
        });
    }

    // Capture analysis: collect every body-instruction
    // operand that's NOT the counter, NOT a Const, NOT a
    // value defined IN the body block, AND not a reduction
    // carry (reductions are marshalled through atomicrmw
    // against parent-side allocas, not as captures).
    let body = &f.blocks[region.shape.body_block.0 as usize];
    let mut body_defined: std::collections::BTreeSet<ValueId> =
        std::collections::BTreeSet::new();
    body_defined.insert(region.shape.counter_header_value);
    for instr in &body.instructions {
        body_defined.insert(instr.result);
    }
    // Reduction carries are header params for the
    // accumulator; treat them as "body-defined" for capture
    // purposes (they're handled via atomicrmw, not as
    // captures).
    let reduction_carry_set: std::collections::BTreeSet<ValueId> =
        region.reduction_carries.iter().copied().collect();
    for v in &region.reduction_carries {
        body_defined.insert(*v);
    }
    let mut captures: Vec<(ValueId, Type)> = Vec::new();
    let mut capture_seen: std::collections::BTreeSet<ValueId> =
        std::collections::BTreeSet::new();
    let mut collect_capture = |v: ValueId| -> Result<(), EmitError> {
        if body_defined.contains(&v) {
            return Ok(());
        }
        if !capture_seen.insert(v) {
            return Ok(());
        }
        let ty = value_types.get(&v).cloned().ok_or_else(|| EmitError {
            message: format!(
                "parallel-for body captures v_{} but type is unknown",
                v.0
            ),
        })?;
        captures.push((v, ty));
        Ok(())
    };
    for instr in &body.instructions {
        for op in instr_operands(&instr.kind) {
            if let Operand::Value(v) = op {
                collect_capture(*v)?;
            }
        }
    }
    if let Terminator::Jump { args, .. } = &body.terminator {
        for op in args {
            if let Operand::Value(v) = op {
                if !reduction_carry_set.contains(v) {
                    // Back-edge reduction-carry args are
                    // synthesized values defined inside the
                    // body (the `%v_<update>` from `%v_<carry>
                    // + %v_<rhs>`); they don't escape as
                    // captures. The back-edge counter is
                    // similarly body-defined. Skip both.
                    collect_capture(*v)?;
                }
            }
        }
    }
    // Reduction analysis: extract the per-iteration
    // increment for each reduction carry. The body has, for
    // each reduction, an instruction
    //   %v_<update> = add T %v_<carry>, %v_<rhs>
    // (or symmetric — operands may be swapped). We need
    // %v_<rhs> as the atomicrmw operand inside the outlined
    // fn. Reject if the shape doesn't match.
    let mut reduction_increments: Vec<Operand> =
        Vec::with_capacity(region.reductions.len());
    for ((carry_v, update_v), (_, red_op, _)) in region
        .reduction_carries
        .iter()
        .zip(region.reduction_update_values.iter())
        .zip(region.reductions.iter())
    {
        let update_instr = body
            .instructions
            .iter()
            .find(|i| i.result == *update_v)
            .ok_or_else(|| EmitError {
                message: format!(
                    "reduction update v_{} not found in body block",
                    update_v.0
                ),
            })?;
        // Figure out which Binary op the reduction-update
        // SSA instruction should have, based on the
        // reduction's source-level op. `min`/`max` come
        // through as Call instructions; everything else is
        // a Binary.
        let expected_binary_op: Option<BinaryOp> = match red_op {
            crate::ast::ReductionOp::Add => Some(BinaryOp::Add),
            crate::ast::ReductionOp::Mul => Some(BinaryOp::Mul),
            crate::ast::ReductionOp::And => Some(BinaryOp::And),
            crate::ast::ReductionOp::Or => Some(BinaryOp::Or),
            crate::ast::ReductionOp::BitAnd => Some(BinaryOp::BitAnd),
            crate::ast::ReductionOp::BitOr => Some(BinaryOp::BitOr),
            crate::ast::ReductionOp::BitXor => Some(BinaryOp::BitXor),
            crate::ast::ReductionOp::Min | crate::ast::ReductionOp::Max => None,
        };
        let (l, r): (Operand, Operand) = match (&update_instr.kind, expected_binary_op) {
            (InstrKind::Binary { op, l, r, .. }, Some(expected))
                if std::mem::discriminant(op) == std::mem::discriminant(&expected) =>
            {
                (l.clone(), r.clone())
            }
            (InstrKind::Call { name, args, .. }, None)
                if (name == "min" || name == "max") && args.len() == 2 =>
            {
                (args[0].clone(), args[1].clone())
            }
            (other, _) => {
                return Err(EmitError {
                    message: format!(
                        "reduction-update v_{} shape doesn't match {:?} (got {:?})",
                        update_v.0, red_op, other
                    ),
                });
            }
        };
        let increment = match (&l, &r) {
            (Operand::Value(lv), other) if lv == carry_v => other.clone(),
            (other, Operand::Value(rv)) if rv == carry_v => other.clone(),
            _ => {
                return Err(EmitError {
                    message: format!(
                        "reduction-update v_{} doesn't reference carry v_{}",
                        update_v.0, carry_v.0
                    ),
                });
            }
        };
        // The increment operand may itself reference outer
        // values (e.g., `xs[i]` is loaded into a body-local
        // SSA value, but a constant or non-loop-local value
        // is a capture). Make sure to register it as a
        // capture if needed.
        if let Operand::Value(v) = &increment {
            collect_capture(*v)?;
        }
        reduction_increments.push(increment);
    }
    // Drop the unused-binding `_` from the variable name.
    let _ = reduction_carry_set;

    // --- Parent-side emit ---
    // First: every pre-header instruction EXCEPT the Begin
    // hint (the SSA lowerer materializes the start operand
    // AFTER the Begin via placeholder-then-patch, so a strict
    // up-to-Begin emit would drop the start definition).
    for instr in &pre_header.instructions {
        if matches!(
            &instr.kind,
            InstrKind::Hint(HintKind::ParallelForBegin { .. })
        ) {
            continue;
        }
        emit_instr(instr, value_types, fn_sigs, out)?;
    }

    // Reserve an outline-fn id + name.
    let id = OUTLINE_COUNTER.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    let fn_name = format!("__intent_par_{}", id);

    // Build the ctx struct holding `{ i64 start, i64 end, …
    // captures, …reduction_ptrs }`. start/end are widened
    // to i64 so the outlined fn's slice math is uniform;
    // each capture gets a field of its source LLVM type;
    // each reduction gets a `<red_ty>*` field pointing at
    // a parent-side alloca that all threads atomicrmw
    // against.
    let counter_ty_str = llvm_type(&region.shape.counter_ty)?;
    let capture_field_tys: Vec<String> = captures
        .iter()
        .map(|(_, ty)| llvm_type_string(ty))
        .collect::<Result<Vec<_>, _>>()?;
    // For bool reductions (`&&` / `||`) the parent-side
    // accumulator is widened to i8 — `atomicrmw and/or`
    // doesn't accept i1. Bitwise + integer + min/max
    // reductions use the source type directly.
    let reduction_field_tys: Vec<String> = region
        .reductions
        .iter()
        .map(|(_, _, ty)| Ok(format!("{}*", red_storage_llvm(ty)?)))
        .collect::<Result<Vec<_>, EmitError>>()?;
    let mut ctx_field_tys: Vec<String> =
        vec!["i64".to_string(), "i64".to_string()];
    ctx_field_tys.extend(capture_field_tys.iter().cloned());
    ctx_field_tys.extend(reduction_field_tys.iter().cloned());
    let ctx_struct_ty = format!("{{ {} }}", ctx_field_tys.join(", "));

    // Coerce start/end operands to i64.
    let start_i64 = widen_to_i64_for_ctx(
        &region.shape.start,
        &region.shape.counter_ty,
        value_types,
        out,
    )?;
    let end_i64 = widen_to_i64_for_ctx(
        &region.shape.end,
        &region.shape.counter_ty,
        value_types,
        out,
    )?;

    // alloca ctx, store fields, bitcast to i8* for the
    // generic GOMP signature.
    let ctx_ptr = format!("%v_par{}.ctx", id);
    out.push_str(&format!("  {} = alloca {}\n", ctx_ptr, ctx_struct_ty));
    let start_field = format!("%v_par{}.sf", id);
    out.push_str(&format!(
        "  {} = getelementptr {}, {}* {}, i32 0, i32 0\n",
        start_field, ctx_struct_ty, ctx_struct_ty, ctx_ptr
    ));
    out.push_str(&format!(
        "  store i64 {}, i64* {}\n",
        start_i64, start_field
    ));
    let end_field = format!("%v_par{}.ef", id);
    out.push_str(&format!(
        "  {} = getelementptr {}, {}* {}, i32 0, i32 1\n",
        end_field, ctx_struct_ty, ctx_struct_ty, ctx_ptr
    ));
    out.push_str(&format!(
        "  store i64 {}, i64* {}\n",
        end_i64, end_field
    ));
    // Store each capture into its ctx field (by value).
    for (i, ((cap_v, _), cap_ty_str)) in
        captures.iter().zip(capture_field_tys.iter()).enumerate()
    {
        let cap_field = format!("%v_par{}.cf{}", id, i);
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* {}, i32 0, i32 {}\n",
            cap_field,
            ctx_struct_ty,
            ctx_struct_ty,
            ctx_ptr,
            2 + i
        ));
        out.push_str(&format!(
            "  store {} %v_{}, {}* {}\n",
            cap_ty_str, cap_v.0, cap_ty_str, cap_field
        ));
    }
    // For each reduction, alloca a parent-side accumulator,
    // initialize it from the carry's incoming value, and
    // store the pointer in the ctx struct. All threads in
    // the outlined fn `atomicrmw` against this pointer.
    let reduction_storage: Vec<String> = (0..region.reductions.len())
        .map(|i| format!("%v_par{}.red{}", id, i))
        .collect();
    let red_field_offset = 2 + captures.len();
    for (i, ((_, _, red_ty), storage)) in
        region.reductions.iter().zip(reduction_storage.iter()).enumerate()
    {
        let storage_ty = red_storage_llvm(red_ty)?;
        out.push_str(&format!("  {} = alloca {}\n", storage, storage_ty));
        // Initial value: pre-header carries it as
        // `region.reduction_inits[i]` (an Operand). Bool
        // gets zext-widened to i8 first.
        let init_op = &region.reduction_inits[i];
        if matches!(red_ty, Type::Bool) {
            let zext_tmp = format!("%v_par{}.red{}.zext", id, i);
            out.push_str(&format!(
                "  {} = zext i1 {} to i8\n",
                zext_tmp,
                operand_str(init_op)
            ));
            out.push_str(&format!(
                "  store i8 {}, i8* {}\n",
                zext_tmp, storage
            ));
        } else {
            out.push_str(&format!(
                "  store {} {}, {}* {}\n",
                storage_ty,
                operand_str(init_op),
                storage_ty,
                storage
            ));
        }
        let red_field = format!("%v_par{}.rf{}", id, i);
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* {}, i32 0, i32 {}\n",
            red_field,
            ctx_struct_ty,
            ctx_struct_ty,
            ctx_ptr,
            red_field_offset + i
        ));
        out.push_str(&format!(
            "  store {}* {}, {}** {}\n",
            storage_ty, storage, storage_ty, red_field
        ));
    }
    let ctx_raw = format!("%v_par{}.raw", id);
    out.push_str(&format!(
        "  {} = bitcast {}* {} to i8*\n",
        ctx_raw, ctx_struct_ty, ctx_ptr
    ));
    if host_uses_win32_threading() {
        // Win32 fan-out: see tree-LLVM's `emit_parallel_for_via_gomp`
        // for the matching shape. N=4 hardcoded worker threads; tid 0
        // runs in the calling thread. Each thread receives a
        // `WinParArg { i8* ctx, i64 tid, i64 nt }` whose layout the
        // outlined fn unpacks at entry.
        const N: u64 = 4;
        let warr = format!("%v_par{}.warr", id);
        out.push_str(&format!(
            "  {} = alloca [{} x {{ i8*, i64, i64 }}]\n",
            warr, N
        ));
        let mut wp_names: Vec<String> = Vec::with_capacity(N as usize);
        for i in 0..N {
            let wpi = format!("%v_par{}.wp{}", id, i);
            out.push_str(&format!(
                "  {} = getelementptr [{} x {{ i8*, i64, i64 }}], [{} x {{ i8*, i64, i64 }}]* {}, i64 0, i64 {}\n",
                wpi, N, N, warr, i
            ));
            let cf = format!("%v_par{}.wp{}.c", id, i);
            out.push_str(&format!(
                "  {} = getelementptr {{ i8*, i64, i64 }}, {{ i8*, i64, i64 }}* {}, i32 0, i32 0\n",
                cf, wpi
            ));
            out.push_str(&format!(
                "  store i8* {}, i8** {}\n",
                ctx_raw, cf
            ));
            let tf = format!("%v_par{}.wp{}.t", id, i);
            out.push_str(&format!(
                "  {} = getelementptr {{ i8*, i64, i64 }}, {{ i8*, i64, i64 }}* {}, i32 0, i32 1\n",
                tf, wpi
            ));
            out.push_str(&format!(
                "  store i64 {}, i64* {}\n",
                i, tf
            ));
            let nf = format!("%v_par{}.wp{}.n", id, i);
            out.push_str(&format!(
                "  {} = getelementptr {{ i8*, i64, i64 }}, {{ i8*, i64, i64 }}* {}, i32 0, i32 2\n",
                nf, wpi
            ));
            out.push_str(&format!(
                "  store i64 {}, i64* {}\n",
                N, nf
            ));
            wp_names.push(wpi);
        }
        let hs = format!("%v_par{}.hs", id);
        out.push_str(&format!(
            "  {} = alloca [{} x i8*]\n",
            hs,
            N - 1
        ));
        let mut handle_ps: Vec<String> = Vec::with_capacity((N - 1) as usize);
        for i in 1..N {
            let raw = format!("%v_par{}.argraw{}", id, i);
            out.push_str(&format!(
                "  {} = bitcast {{ i8*, i64, i64 }}* {} to i8*\n",
                raw, wp_names[i as usize]
            ));
            let h = format!("%v_par{}.h{}", id, i);
            out.push_str(&format!(
                "  {} = call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @{}, i8* {}, i32 0, i32* null)\n",
                h, fn_name, raw
            ));
            let hp = format!("%v_par{}.hp{}", id, i);
            out.push_str(&format!(
                "  {} = getelementptr [{} x i8*], [{} x i8*]* {}, i64 0, i64 {}\n",
                hp,
                N - 1,
                N - 1,
                hs,
                i - 1
            ));
            out.push_str(&format!(
                "  store i8* {}, i8** {}\n",
                h, hp
            ));
            handle_ps.push(hp);
        }
        let raw0 = format!("%v_par{}.argraw0", id);
        out.push_str(&format!(
            "  {} = bitcast {{ i8*, i64, i64 }}* {} to i8*\n",
            raw0, wp_names[0]
        ));
        let ret0 = format!("%v_par{}.ret0", id);
        out.push_str(&format!(
            "  {} = call i8* @{}(i8* {})\n",
            ret0, fn_name, raw0
        ));
        for (j, hp) in handle_ps.iter().enumerate() {
            let hl = format!("%v_par{}.hl{}", id, j);
            out.push_str(&format!(
                "  {} = load i8*, i8** {}\n",
                hl, hp
            ));
            let wait = format!("%v_par{}.w{}", id, j);
            out.push_str(&format!(
                "  {} = call i32 @WaitForSingleObject(i8* {}, i32 -1)\n",
                wait, hl
            ));
            let close = format!("%v_par{}.c{}", id, j);
            out.push_str(&format!(
                "  {} = call i32 @CloseHandle(i8* {})\n",
                close, hl
            ));
        }
    } else {
        out.push_str(&format!(
            "  call void @GOMP_parallel(void (i8*)* @{}, i8* {}, i32 0, i32 0)\n",
            fn_name, ctx_raw
        ));
    }
    // Now transition to the exit block. The header's
    // terminator is `Branch(cond, body, [], exit, exit_args)`;
    // we want the exit-args side. After GOMP_parallel the
    // counter is `end` and reduction carries (none in this v1
    // case) hold their final values. Emit the header→exit
    // block-arg assignments + branch to exit.
    let header = &f.blocks[region.shape.header_block.0 as usize];
    let exit_args = match &header.terminator {
        Terminator::Branch { else_block, else_args, .. }
            if *else_block == region.shape.exit_block =>
        {
            else_args.clone()
        }
        Terminator::Branch { then_block, then_args, .. }
            if *then_block == region.shape.exit_block =>
        {
            then_args.clone()
        }
        other => {
            return Err(EmitError {
                message: format!(
                    "parallel-for header terminator must be Branch into exit, got {:?}",
                    other
                ),
            });
        }
    };
    // The exit args reference header param ValueIds. After
    // the parallel-for completes the counter has reached
    // `end`. Materialize each exit-bound value as i64 = end
    // for the counter slot, and propagate body-defined values
    // for any non-counter slots. (For the v1 no-reduction
    // case there's only the counter, but the loop is general.)
    let exit_block = &f.blocks[region.shape.exit_block.0 as usize];
    // Build a map from reduction carry ValueId → index so we
    // can identify reduction exit-args and load their final
    // values from the parent-side accumulator alloca.
    let carry_to_red_idx: std::collections::BTreeMap<ValueId, usize> = region
        .reduction_carries
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, i))
        .collect();
    for (i, op) in exit_args.iter().enumerate() {
        let Some((dst_v, dst_ty)) = exit_block.params.get(i) else {
            break;
        };
        let dst_ty_str = llvm_type_string(dst_ty)?;
        if let Operand::Value(v) = op {
            if *v == region.shape.counter_header_value {
                // Counter at exit = end (truncated back to
                // its source type).
                if counter_ty_str == "i64" {
                    out.push_str(&format!(
                        "  %v_{} = add i64 {}, 0\n",
                        dst_v.0, end_i64
                    ));
                } else {
                    out.push_str(&format!(
                        "  %v_{} = trunc i64 {} to {}\n",
                        dst_v.0, end_i64, counter_ty_str
                    ));
                }
                continue;
            }
            if let Some(red_idx) = carry_to_red_idx.get(v) {
                // Reduction final value: load from the
                // parent-side accumulator alloca. Bool
                // (stored as i8) gets icmp-ne-0 back to i1.
                let red_ty = &region.reductions[*red_idx].2;
                let storage = &reduction_storage[*red_idx];
                let storage_ty_str = red_storage_llvm(red_ty)?;
                if matches!(red_ty, Type::Bool) {
                    let i8_v = format!("%v_{}.i8", dst_v.0);
                    out.push_str(&format!(
                        "  {} = load i8, i8* {}\n",
                        i8_v, storage
                    ));
                    out.push_str(&format!(
                        "  %v_{} = icmp ne i8 {}, 0\n",
                        dst_v.0, i8_v
                    ));
                } else {
                    out.push_str(&format!(
                        "  %v_{} = load {}, {}* {}\n",
                        dst_v.0, storage_ty_str, storage_ty_str, storage
                    ));
                }
                continue;
            }
        }
        // Fallback: emit operand as-is (works for Consts).
        out.push_str(&format!(
            "  %v_{} = add {} 0, {}\n",
            dst_v.0,
            dst_ty_str,
            operand_str(op)
        ));
    }
    out.push_str(&format!("  br label %bb{}\n", region.shape.exit_block.0));

    // --- Outlined function emit ---
    emit_outlined_parallel_for(
        &fn_name,
        f,
        region,
        &ctx_struct_ty,
        &captures,
        &capture_field_tys,
        &reduction_increments,
        value_types,
        fn_sigs,
    )?;
    Ok(())
}

/// Recover the `(T, N)` shape from a `&Channel<T, N>` /
/// `&mut Channel<T, N>` type. Used by the channel
/// intrinsic dispatch.
fn channel_inner_from_ty_llvm(ty: &Type) -> Result<(Type, u64), EmitError> {
    let inner = match ty {
        Type::Ref(inner) | Type::RefMut(inner) => &**inner,
        other => other,
    };
    match inner {
        Type::Channel(element, capacity) => Ok(((**element).clone(), *capacity)),
        other => Err(EmitError {
            message: format!(
                "channel op expected &Channel<T, N> (or Channel<T, N>), got {:?}",
                other
            ),
        }),
    }
}

/// Recover the inner element type of an `&Atomic<T>` /
/// `&mut Atomic<T>` operand. Used by the atomic intrinsic
/// dispatch (`atomic_load` and friends) to pick the LLVM
/// storage width.
fn atomic_element_of_operand(
    op: &Operand,
    value_types: &BTreeMap<ValueId, Type>,
) -> Result<Type, EmitError> {
    let ty = operand_type(op, value_types).ok_or_else(|| EmitError {
        message: "atomic-op operand has unknown type".to_string(),
    })?;
    let inner = match ty {
        Type::Ref(inner) | Type::RefMut(inner) => *inner,
        other => other,
    };
    match inner {
        Type::Atomic(elt) => Ok(*elt),
        other => Err(EmitError {
            message: format!(
                "atomic-op operand expected &Atomic<T> (or Atomic<T>), got {:?}",
                other
            ),
        }),
    }
}

/// True when an integer type is one of the widths we
/// support for parallel-for reductions today (every signed
/// + unsigned integer width except 1-bit, which needs an
/// i8 shadow because `atomicrmw` rejects i1).
fn is_supported_int(ty: &Type) -> bool {
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
    )
}

/// True when the integer type uses signed arithmetic (drives
/// `smin/smax` vs `umin/umax` and signed-vs-unsigned cmpxchg
/// considerations).
fn is_signed_int(ty: &Type) -> bool {
    matches!(ty, Type::I8 | Type::I16 | Type::I32 | Type::I64)
}

/// LLVM storage type for a parallel-for reduction's parent-
/// side accumulator. Bool gets widened to i8 because
/// `atomicrmw` rejects i1; everything else uses the native
/// width.
fn red_storage_llvm(ty: &Type) -> Result<&'static str, EmitError> {
    match ty {
        Type::Bool => Ok("i8"),
        _ => llvm_type(ty),
    }
}

/// Widen an i32/i16/i8 operand to i64 for ctx storage so
/// the outlined fn's slice math is uniform width. For
/// Const operands we just emit the literal as i64; for
/// Values we materialize a sext (signed types) or zext
/// (unsigned).
fn widen_to_i64_for_ctx(
    op: &Operand,
    src_ty: &Type,
    value_types: &BTreeMap<ValueId, Type>,
    out: &mut String,
) -> Result<String, EmitError> {
    match op {
        Operand::Const(Const::Int(v)) => Ok(format!("{}", v)),
        Operand::Const(_) => Err(EmitError {
            message: "parallel-for bound must be an integer Const or Value".to_string(),
        }),
        Operand::Value(v) => {
            let bits = type_bits(src_ty);
            if bits == 64 {
                return Ok(format!("%v_{}", v.0));
            }
            let src_llvm = llvm_type(src_ty)?;
            let widened = format!("%v_{}.w64", v.0);
            let opcode = if matches!(
                src_ty,
                Type::I8 | Type::I16 | Type::I32 | Type::I64
            ) {
                "sext"
            } else {
                "zext"
            };
            let _ = value_types;
            out.push_str(&format!(
                "  {} = {} {} %v_{} to i64\n",
                widened, opcode, src_llvm, v.0
            ));
            Ok(widened)
        }
    }
}

/// Emit the outlined `@__intent_par_<N>` function into the
/// `DEFERRED_FUNCTIONS` buffer. Loads start/end from the ctx
/// struct, computes this thread's iteration slice via
/// `omp_get_thread_num` / `omp_get_num_threads`, runs the
/// body block's instructions inside its own loop, then
/// returns.
fn emit_outlined_parallel_for(
    fn_name: &str,
    f: &Function,
    region: &ParallelRegion,
    ctx_struct_ty: &str,
    captures: &[(ValueId, Type)],
    capture_field_tys: &[String],
    reduction_increments: &[Operand],
    value_types: &BTreeMap<ValueId, Type>,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
) -> Result<(), EmitError> {
    let mut out = String::new();
    let counter_ty_str = llvm_type(&region.shape.counter_ty)?;
    let use_win32 = host_uses_win32_threading();

    // On Win32 the outlined fn matches `@CreateThread`'s
    // start-routine ABI (`i8* (i8*)`) and unpacks tid/nt from
    // a `WinParArg { i8* ctx, i64 tid, i64 nt }` instead of
    // calling `omp_get_*`. On Linux/macOS the GOMP-compatible
    // `void (i8*)` signature is preserved and tid/nt come
    // from `omp_get_thread_num` / `omp_get_num_threads`.
    if use_win32 {
        out.push_str(&format!(
            "define internal i8* @{}(i8* %_ctx) {{\n",
            fn_name
        ));
    } else {
        out.push_str(&format!(
            "define internal void @{}(i8* %_ctx) {{\n",
            fn_name
        ));
    }
    out.push_str("entry:\n");
    if use_win32 {
        // _ctx is actually a WinParArg*; unpack ctx_raw, tid, nt.
        out.push_str(
            "  %winarg_p = bitcast i8* %_ctx to { i8*, i64, i64 }*\n",
        );
        out.push_str(
            "  %ctx_raw_p = getelementptr { i8*, i64, i64 }, { i8*, i64, i64 }* %winarg_p, i32 0, i32 0\n",
        );
        out.push_str(
            "  %ctx_raw = load i8*, i8** %ctx_raw_p\n",
        );
        out.push_str(&format!(
            "  %ctx = bitcast i8* %ctx_raw to {}*\n",
            ctx_struct_ty
        ));
    } else {
        // Unmarshal ctx → start, end (as i64; narrow at use sites).
        out.push_str(&format!(
            "  %ctx = bitcast i8* %_ctx to {}*\n",
            ctx_struct_ty
        ));
    }
    out.push_str(&format!(
        "  %start_p = getelementptr {}, {}* %ctx, i32 0, i32 0\n",
        ctx_struct_ty, ctx_struct_ty
    ));
    out.push_str("  %start = load i64, i64* %start_p\n");
    out.push_str(&format!(
        "  %end_p = getelementptr {}, {}* %ctx, i32 0, i32 1\n",
        ctx_struct_ty, ctx_struct_ty
    ));
    out.push_str("  %end = load i64, i64* %end_p\n");
    // Unmarshal each capture from its ctx field. Use the
    // same `%cap_<i>_p` / `%cap_<i>` naming convention as
    // tree-LLVM (existing tests pin those exact prefixes),
    // then alias to `%v_<id>` so body instructions referring
    // to the capture's SSA value resolve naturally.
    for (i, ((cap_v, cap_ty), cap_ty_str)) in
        captures.iter().zip(capture_field_tys.iter()).enumerate()
    {
        let cap_field = format!("%cap_{}_p", i);
        let cap_loaded = format!("%cap_{}", i);
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* %ctx, i32 0, i32 {}\n",
            cap_field,
            ctx_struct_ty,
            ctx_struct_ty,
            2 + i
        ));
        out.push_str(&format!(
            "  {} = load {}, {}* {}\n",
            cap_loaded, cap_ty_str, cap_ty_str, cap_field
        ));
        // Alias to `%v_<id>` so the body's references to
        // this ValueId resolve. Use a type-appropriate
        // identity op: `add 0` for ints, `fadd 0.0` for
        // floats, `or false` for bool, `bitcast` for
        // aggregates/pointers (always-valid same-type
        // bitcast).
        let alias_emit = match cap_ty {
            Type::Bool => format!(
                "  %v_{} = or i1 false, {}\n",
                cap_v.0, cap_loaded
            ),
            Type::F32 => format!(
                "  %v_{} = fadd float 0.0, {}\n",
                cap_v.0, cap_loaded
            ),
            Type::F64 => format!(
                "  %v_{} = fadd double 0.0, {}\n",
                cap_v.0, cap_loaded
            ),
            t if t.is_integer() => format!(
                "  %v_{} = add {} 0, {}\n",
                cap_v.0, cap_ty_str, cap_loaded
            ),
            _ => format!(
                "  %v_{} = bitcast {} {} to {}\n",
                cap_v.0, cap_ty_str, cap_loaded, cap_ty_str
            ),
        };
        out.push_str(&alias_emit);
    }
    // Load each reduction's parent-accumulator pointer from
    // its ctx field. Each thread does `atomicrmw <op>`
    // against this pointer when the body's reduction-update
    // instruction would otherwise fire.
    let red_field_offset = 2 + captures.len();
    let reduction_ptr_names: Vec<String> = (0..region.reductions.len())
        .map(|i| format!("%red{}_p", i))
        .collect();
    for (i, ((_, _, red_ty), ptr_name)) in
        region.reductions.iter().zip(reduction_ptr_names.iter()).enumerate()
    {
        let storage_ty_str = red_storage_llvm(red_ty)?;
        let field_name = format!("%red{}_field", i);
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* %ctx, i32 0, i32 {}\n",
            field_name,
            ctx_struct_ty,
            ctx_struct_ty,
            red_field_offset + i
        ));
        out.push_str(&format!(
            "  {} = load {}*, {}** {}\n",
            ptr_name, storage_ty_str, storage_ty_str, field_name
        ));
    }

    // Work distribution: OMP on Linux/macOS, Win32-arg loads
    // on Windows.
    if use_win32 {
        out.push_str(
            "  %tid_p = getelementptr { i8*, i64, i64 }, { i8*, i64, i64 }* %winarg_p, i32 0, i32 1\n",
        );
        out.push_str("  %tid = load i64, i64* %tid_p\n");
        out.push_str(
            "  %nth_p = getelementptr { i8*, i64, i64 }, { i8*, i64, i64 }* %winarg_p, i32 0, i32 2\n",
        );
        out.push_str("  %nth = load i64, i64* %nth_p\n");
    } else {
        out.push_str("  %tid32 = call i32 @omp_get_thread_num()\n");
        out.push_str("  %nth32 = call i32 @omp_get_num_threads()\n");
        out.push_str("  %tid = sext i32 %tid32 to i64\n");
        out.push_str("  %nth = sext i32 %nth32 to i64\n");
    }
    // span = end - start
    out.push_str("  %span = sub i64 %end, %start\n");
    // chunk = (span + nth - 1) / nth  (ceil division)
    out.push_str("  %nth_m1 = sub i64 %nth, 1\n");
    out.push_str("  %sum = add i64 %span, %nth_m1\n");
    out.push_str("  %chunk = sdiv i64 %sum, %nth\n");
    // my_lo_off = tid * chunk; my_hi_off = my_lo_off + chunk
    out.push_str("  %my_lo_off = mul i64 %tid, %chunk\n");
    out.push_str("  %my_hi_off_raw = add i64 %my_lo_off, %chunk\n");
    // my_lo = start + my_lo_off, my_hi = min(start +
    // my_hi_off, end)
    out.push_str("  %my_lo = add i64 %start, %my_lo_off\n");
    out.push_str("  %my_hi_raw = add i64 %start, %my_hi_off_raw\n");
    out.push_str("  %too_far = icmp sgt i64 %my_hi_raw, %end\n");
    out.push_str("  %my_hi = select i1 %too_far, i64 %end, i64 %my_hi_raw\n");
    // Bound check: skip if my_lo >= my_hi.
    out.push_str("  %has_work = icmp slt i64 %my_lo, %my_hi\n");
    out.push_str("  br i1 %has_work, label %check, label %done\n");

    // Iteration loop.
    out.push_str("check:\n");
    out.push_str("  %i = phi i64 [%my_lo, %entry], [%i_next, %body_end]\n");
    out.push_str("  %cond = icmp slt i64 %i, %my_hi\n");
    out.push_str("  br i1 %cond, label %body, label %done\n");
    out.push_str("body:\n");
    // Bind the body's SSA counter ValueId to %i (narrowing
    // back to its source-level integer type if needed).
    let counter_id = region.shape.counter_header_value.0;
    if counter_ty_str == "i64" {
        out.push_str(&format!(
            "  %v_{} = add i64 %i, 0\n",
            counter_id
        ));
    } else {
        out.push_str(&format!(
            "  %v_{} = trunc i64 %i to {}\n",
            counter_id, counter_ty_str
        ));
    }
    // Emit each body instruction except the counter-
    // increment (the loop's %i_next handles that) and the
    // back-edge terminator. Build a value_types map scoped
    // to the outlined fn — it sees the counter binding and
    // every body-defined value.
    let body = &f.blocks[region.shape.body_block.0 as usize];
    let mut outline_value_types: BTreeMap<ValueId, Type> = BTreeMap::new();
    outline_value_types.insert(
        region.shape.counter_header_value,
        region.shape.counter_ty.clone(),
    );
    for instr in &body.instructions {
        outline_value_types.insert(instr.result, instr.ty.clone());
    }
    // Carry over outer value-types referenced indirectly by
    // type-dispatched ops (the no-capture check above proves
    // no body instruction reads a non-body-defined ValueId,
    // but `operand_type` lookups during type-dispatched emit
    // may still consult the outer map for completeness).
    for (k, v) in value_types {
        outline_value_types.entry(*k).or_insert_with(|| v.clone());
    }
    // Map reduction-update ValueId → (reduction index,
    // increment operand, reduction type). When emitting body
    // instructions, intercept these IDs and emit
    // `atomicrmw <op>` against the parent accumulator
    // pointer instead of the normal Binary emit.
    let red_update_to_idx: std::collections::BTreeMap<ValueId, usize> = region
        .reduction_update_values
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, i))
        .collect();
    for instr in &body.instructions {
        if instr.result == region.counter_increment_value {
            continue;
        }
        if let Some(red_idx) = red_update_to_idx.get(&instr.result) {
            emit_reduction_update(
                &region.reductions[*red_idx],
                &reduction_ptr_names[*red_idx],
                &reduction_increments[*red_idx],
                instr.result,
                &mut out,
            )?;
            continue;
        }
        emit_instr(instr, &outline_value_types, fn_sigs, &mut out)?;
    }
    out.push_str("  br label %body_end\n");
    out.push_str("body_end:\n");
    out.push_str("  %i_next = add i64 %i, 1\n");
    out.push_str("  br label %check\n");
    out.push_str("done:\n");
    if use_win32 {
        out.push_str("  ret i8* null\n");
    } else {
        out.push_str("  ret void\n");
    }
    out.push_str("}\n\n");

    DEFERRED_FUNCTIONS.with(|b| b.borrow_mut().push_str(&out));
    Ok(())
}

/// Emit the body's reduction-update instruction as an
/// atomic op against the parent-side accumulator. The
/// dispatch table mirrors tree-LLVM's
/// `emit_parallel_for_via_gomp` so cross-backend parity
/// holds. Bool reductions widen through an i8 shadow on
/// both sides; `*` (Mul) uses a `cmpxchg` retry loop since
/// `atomicrmw mul` isn't a thing.
fn emit_reduction_update(
    reduction: &(String, crate::ast::ReductionOp, Type),
    ptr: &str,
    increment: &Operand,
    update_result: ValueId,
    out: &mut String,
) -> Result<(), EmitError> {
    let (_, op, red_ty) = reduction;
    let storage_ty = red_storage_llvm(red_ty)?;
    let native_ty = llvm_type(red_ty).ok();
    // Bool path: zext i1 → i8, atomicrmw and/or on the
    // shadow.
    if matches!(red_ty, Type::Bool) {
        let opcode = match op {
            crate::ast::ReductionOp::And => "and",
            crate::ast::ReductionOp::Or => "or",
            other => {
                return Err(EmitError {
                    message: format!(
                        "bool reduction expects `&&` or `||` (got {:?})",
                        other
                    ),
                });
            }
        };
        let incr_8 = format!("%v_{}.incr8", update_result.0);
        out.push_str(&format!(
            "  {} = zext i1 {} to i8\n",
            incr_8,
            operand_str(increment)
        ));
        out.push_str(&format!(
            "  %v_{}.old = atomicrmw {} i8* {}, i8 {} seq_cst\n",
            update_result.0, opcode, ptr, incr_8
        ));
        // Materialize `%v_<update>` as an i1 so anything
        // referencing it downstream type-checks; the value
        // itself is the *new* combined result, post-atomic-
        // update — but here we just need a definition.
        // Since the back-edge is skipped, the actual value
        // doesn't matter.
        out.push_str(&format!(
            "  %v_{} = icmp ne i8 %v_{}.old, 0\n",
            update_result.0, update_result.0
        ));
        return Ok(());
    }
    // Mul: cmpxchg retry loop. Load current; multiply by
    // increment; cmpxchg; on failure reload and retry.
    if matches!(op, crate::ast::ReductionOp::Mul) {
        let nty = native_ty.ok_or_else(|| EmitError {
            message: "Mul reduction needs a native integer type".to_string(),
        })?;
        let n = update_result.0;
        // Initial load before the retry loop.
        out.push_str(&format!(
            "  br label %red{}.try\n",
            n
        ));
        out.push_str(&format!("red{}.try:\n", n));
        out.push_str(&format!(
            "  %v_{}.cur = load {}, {}* {}\n",
            n, nty, nty, ptr
        ));
        out.push_str(&format!(
            "  %v_{}.new = mul {} %v_{}.cur, {}\n",
            n,
            nty,
            n,
            operand_str(increment)
        ));
        out.push_str(&format!(
            "  %v_{}.xchg = cmpxchg {}* {}, {} %v_{}.cur, {} %v_{}.new seq_cst seq_cst\n",
            n, nty, ptr, nty, n, nty, n
        ));
        out.push_str(&format!(
            "  %v_{}.ok = extractvalue {{ {}, i1 }} %v_{}.xchg, 1\n",
            n, nty, n
        ));
        out.push_str(&format!(
            "  br i1 %v_{}.ok, label %red{}.ok, label %red{}.try\n",
            n, n, n
        ));
        out.push_str(&format!("red{}.ok:\n", n));
        // Materialize `%v_<update>` so downstream code (if any)
        // has a definition. Value = the new product.
        out.push_str(&format!(
            "  %v_{} = mul {} %v_{}.cur, {}\n",
            n,
            nty,
            n,
            operand_str(increment)
        ));
        return Ok(());
    }
    // Other ops map cleanly to atomicrmw.
    let opcode = match op {
        crate::ast::ReductionOp::Add => "add",
        crate::ast::ReductionOp::BitAnd => "and",
        crate::ast::ReductionOp::BitOr => "or",
        crate::ast::ReductionOp::BitXor => "xor",
        crate::ast::ReductionOp::Min => {
            if is_signed_int(red_ty) {
                "min"
            } else {
                "umin"
            }
        }
        crate::ast::ReductionOp::Max => {
            if is_signed_int(red_ty) {
                "max"
            } else {
                "umax"
            }
        }
        crate::ast::ReductionOp::Mul
        | crate::ast::ReductionOp::And
        | crate::ast::ReductionOp::Or => {
            // Handled above.
            unreachable!()
        }
    };
    out.push_str(&format!(
        "  %v_{} = atomicrmw {} {}* {}, {} {} seq_cst\n",
        update_result.0,
        opcode,
        storage_ty,
        ptr,
        storage_ty,
        operand_str(increment)
    ));
    Ok(())
}

/// Collect every `Operand` referenced by an instruction. The
/// outlined-emit free-variable check walks these to verify
/// the body is closed under {counter, body-defined, Const}.
fn instr_operands(kind: &InstrKind) -> Vec<&Operand> {
    let mut out = Vec::new();
    match kind {
        InstrKind::Const(_) => {}
        InstrKind::Unary { x, .. } => out.push(x),
        InstrKind::Binary { l, r, .. } => {
            out.push(l);
            out.push(r);
        }
        InstrKind::Cast { x, .. } => out.push(x),
        InstrKind::Call { args, .. } => out.extend(args.iter()),
        InstrKind::ArrayLit { elements } => out.extend(elements.iter()),
        InstrKind::Index { array, index, .. } => {
            out.push(array);
            out.push(index);
        }
        InstrKind::IndexAssign { array, index, value, .. } => {
            out.push(array);
            out.push(index);
            out.push(value);
        }
        InstrKind::Len { array, .. } => out.push(array),
        InstrKind::Drop { source, .. } => out.push(source),
        InstrKind::RefOf { source, .. } => out.push(source),
        InstrKind::StrLit(_) => {}
        InstrKind::FnRef { .. } => {}
        InstrKind::CallIndirect { callee, args } => {
            out.push(callee);
            out.extend(args.iter());
        }
        InstrKind::Hint(_) => {}
    }
    out
}

fn compute_predecessors(f: &Function) -> BTreeMap<BlockId, Vec<(BlockId, Vec<Operand>)>> {
    let mut map: BTreeMap<BlockId, Vec<(BlockId, Vec<Operand>)>> = BTreeMap::new();
    for block in &f.blocks {
        match &block.terminator {
            Terminator::Jump { target, args } => {
                map.entry(*target)
                    .or_default()
                    .push((block.id, args.clone()));
            }
            Terminator::Branch {
                cond: _,
                then_block,
                then_args,
                else_block,
                else_args,
            } => {
                map.entry(*then_block)
                    .or_default()
                    .push((block.id, then_args.clone()));
                map.entry(*else_block)
                    .or_default()
                    .push((block.id, else_args.clone()));
            }
            Terminator::Return(_) | Terminator::Unreachable => {}
        }
    }
    map
}

fn emit_instr(
    instr: &crate::ssa::Instruction,
    value_types: &BTreeMap<ValueId, Type>,
    fn_sigs: &BTreeMap<String, (Vec<Type>, Type)>,
    out: &mut String,
) -> Result<(), EmitError> {
    match &instr.kind {
        InstrKind::Const(c) => {
            // Materialize a constant as `%vN = add <T> 0, <c>`
            // — there's no direct "const" instruction in LLVM
            // IR. For bool we use `or i1 false, <c>`; for
            // float/double we use `fadd <T> 0.0, <c>` so the
            // identity-element type matches the opcode.
            let ty_str = llvm_type(&instr.ty)?;
            let (op, zero) = match &instr.ty {
                Type::Bool => ("or", "false".to_string()),
                Type::F32 | Type::F64 => ("fadd", "0.0".to_string()),
                _ => ("add", "0".to_string()),
            };
            out.push_str(&format!(
                "  %v_{} = {} {} {}, {}\n",
                instr.result.0,
                op,
                ty_str,
                zero,
                const_str(c)
            ));
        }
        InstrKind::Unary { op, x } => {
            let ty_str = llvm_type(&instr.ty)?;
            match op {
                UnaryOp::Neg => {
                    // Integer negation uses `sub 0, x`; float
                    // negation needs `fsub` (the integer `sub`
                    // instruction rejects float operands as
                    // "integer constant must have integer
                    // type").
                    let op_name = if instr.ty.is_float() {
                        "fsub"
                    } else {
                        "sub"
                    };
                    let zero = if instr.ty.is_float() {
                        "0.0"
                    } else {
                        "0"
                    };
                    out.push_str(&format!(
                        "  %v_{} = {} {} {}, {}\n",
                        instr.result.0,
                        op_name,
                        ty_str,
                        zero,
                        operand_str(x)
                    ));
                }
                UnaryOp::Not => {
                    // Boolean negate via xor with 1.
                    out.push_str(&format!(
                        "  %v_{} = xor {} {}, 1\n",
                        instr.result.0,
                        ty_str,
                        operand_str(x)
                    ));
                }
            }
        }
        InstrKind::Binary { op, l, r } => {
            // Operand type: try the LHS first, then the RHS,
            // then the result type. The result type covers
            // arithmetic ops (where operand_ty == result_ty);
            // comparisons need an operand_ty different from
            // the bool result, so we MUST find at least one
            // typed operand for comparisons over consts. The
            // fallback to i64 matches the language default
            // for unannotated integer literals.
            let lhs_ty = operand_type(l, value_types)
                .or_else(|| operand_type(r, value_types))
                .unwrap_or_else(|| {
                    if instr.ty == Type::Bool {
                        // Comparison: both operands are
                        // Consts; default to i64.
                        Type::I64
                    } else {
                        instr.ty.clone()
                    }
                });
            emit_binary(*op, l, r, &lhs_ty, &instr.ty, instr.result, out)?;
        }
        InstrKind::Cast { x, to } => {
            let from_ty = operand_type(x, value_types).unwrap_or_else(|| to.clone());
            emit_cast(x, &from_ty, to, instr.result, out)?;
        }
        InstrKind::Call { name, args } => {
            // `intent_print` / `intent_assert_fail` are
            // synthetic IR calls that the SSA lowerer
            // introduces for `print` and assert-with-message
            // statements. Today's tests on the SSA-LLVM path
            // don't print, so emit them as no-ops / abort.
            if name == "intent_print_item" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "intent_print_item expects one argument".to_string(),
                })?;
                let aty = operand_type(arg, value_types).unwrap_or(Type::I64);
                let (fmt_text, conv_ty, arg_expr) = match &aty {
                    Type::Bool => {
                        // Render bool as "true" / "false" via
                        // a `select` between two string-literal
                        // globals. Mirrors the tree-LLVM path
                        // (closure #117). The globals are
                        // registered idempotently by
                        // `STR_GLOBALS`.
                        STR_GLOBALS.with(|b| {
                            let mut buf = b.borrow_mut();
                            if !buf.contains("@.bool_true") {
                                buf.push_str(
                                    "@.bool_true = private unnamed_addr constant [5 x i8] c\"true\\00\"\n",
                                );
                            }
                            if !buf.contains("@.bool_false") {
                                buf.push_str(
                                    "@.bool_false = private unnamed_addr constant [6 x i8] c\"false\\00\"\n",
                                );
                            }
                        });
                        let t_ptr = format!("%v_{}.pt", instr.result.0);
                        let f_ptr = format!("%v_{}.pf", instr.result.0);
                        let sel = format!("%v_{}.psel", instr.result.0);
                        out.push_str(&format!(
                            "  {} = getelementptr [5 x i8], [5 x i8]* @.bool_true, i64 0, i64 0\n",
                            t_ptr
                        ));
                        out.push_str(&format!(
                            "  {} = getelementptr [6 x i8], [6 x i8]* @.bool_false, i64 0, i64 0\n",
                            f_ptr
                        ));
                        out.push_str(&format!(
                            "  {} = select i1 {}, i8* {}, i8* {}\n",
                            sel,
                            operand_str(arg),
                            t_ptr,
                            f_ptr
                        ));
                        ("%s", "i8*".to_string(), sel)
                    }
                    Type::F32 => {
                        let d = format!("%v_{}.pd", instr.result.0);
                        out.push_str(&format!(
                            "  {} = fpext float {} to double\n",
                            d,
                            operand_str(arg)
                        ));
                        ("%g", "double".to_string(), d)
                    }
                    Type::F64 => ("%g", "double".to_string(), operand_str(arg)),
                    Type::Str | Type::OwnedStr => {
                        ("%s", "i8*".to_string(), operand_str(arg))
                    }
                    _ => {
                        let ity = llvm_type_string(&aty)?;
                        let arg_str = operand_str(arg);
                        let widened = if ity == "i64" {
                            arg_str
                        } else {
                            let w = format!("%v_{}.pw", instr.result.0);
                            out.push_str(&format!(
                                "  {} = sext {} {} to i64\n",
                                w, ity, arg_str
                            ));
                            w
                        };
                        ("%lld", "i64".to_string(), widened)
                    }
                };
                let n = STR_COUNTER.with(|c| {
                    let v = c.get();
                    c.set(v + 1);
                    v
                });
                let bytes_len = fmt_text.len() + 1;
                STR_GLOBALS.with(|b| {
                    b.borrow_mut().push_str(&format!(
                        "@.str.{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"\n",
                        n, bytes_len, fmt_text
                    ));
                });
                let fmt_ptr = format!("%v_{}.fmt", instr.result.0);
                out.push_str(&format!(
                    "  {} = getelementptr [{} x i8], [{} x i8]* @.str.{}, i64 0, i64 0\n",
                    fmt_ptr, bytes_len, bytes_len, n
                ));
                out.push_str(&format!(
                    "  call i32 (i8*, ...) @printf(i8* {}, {} {})\n",
                    fmt_ptr, conv_ty, arg_expr
                ));
                return Ok(());
            }
            if name == "intent_print_putc" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "intent_print_putc expects one argument".to_string(),
                })?;
                // The arg is an i64 const at the SSA layer
                // (32 for space, 10 for newline); putchar
                // takes i32. Materialize the truncation
                // inline.
                let c_str = operand_str(arg);
                let conv = format!("%v_{}.putc", instr.result.0);
                out.push_str(&format!(
                    "  {} = trunc i64 {} to i32\n",
                    conv, c_str
                ));
                out.push_str(&format!(
                    "  call i32 @putchar(i32 {})\n",
                    conv
                ));
                return Ok(());
            }
            if name == "intent_assert_fail" {
                // When the SSA lowerer attaches a custom
                // message (as a `StrLit` `Str` argument),
                // write `assertion failed: <msg>\n` to stderr
                // via `dprintf(2, …)` before aborting. Matches
                // tree-LLVM's shape so the stderr-scraping
                // tests agree.
                if let Some(arg) = args.first() {
                    let n = STR_COUNTER.with(|c| {
                        let v = c.get();
                        c.set(v + 1);
                        v
                    });
                    let fmt_text = "assertion failed: %s\\0A";
                    let bytes_len = "assertion failed: %s\n".len() + 1;
                    STR_GLOBALS.with(|b| {
                        b.borrow_mut().push_str(&format!(
                            "@.str.{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"\n",
                            n, bytes_len, fmt_text
                        ));
                    });
                    let fmt_ptr = format!("%v_{}.afmt", instr.result.0);
                    out.push_str(&format!(
                        "  {} = getelementptr [{} x i8], [{} x i8]* @.str.{}, i64 0, i64 0\n",
                        fmt_ptr, bytes_len, bytes_len, n
                    ));
                    out.push_str(&format!(
                        "  call i32 (i32, i8*, ...) @dprintf(i32 2, i8* {}, i8* {})\n",
                        fmt_ptr,
                        operand_str(arg)
                    ));
                }
                out.push_str("  call void @abort()\n");
                out.push_str("  unreachable\n");
                return Ok(());
            }
            if name == "intent_str_cmp" {
                let lhs = args.get(0).ok_or_else(|| EmitError {
                    message: "intent_str_cmp expects 2 args".to_string(),
                })?;
                let rhs = args.get(1).ok_or_else(|| EmitError {
                    message: "intent_str_cmp expects 2 args".to_string(),
                })?;
                // `strcmp` returns `int` (i32). The SSA call
                // result is i64; sign-extend before binding.
                let raw = format!("%v_{}.scmp", instr.result.0);
                out.push_str(&format!(
                    "  {} = call i32 @strcmp(i8* {}, i8* {})\n",
                    raw,
                    operand_str(lhs),
                    operand_str(rhs)
                ));
                out.push_str(&format!(
                    "  %v_{} = sext i32 {} to i64\n",
                    instr.result.0, raw
                ));
                return Ok(());
            }
            if name == "intent_str_len" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "intent_str_len expects 1 arg".to_string(),
                })?;
                // Closure #262: if the operand is a borrow
                // (`ref s` / `mut ref s` for `s: OwnedStr` /
                // `Str`), the SSA value points at the alloca
                // (`i8**`) rather than the inner pointer
                // (`i8*`). `strlen` wants `i8*`, so load
                // through the borrow first. Without this,
                // `len(ref s)` produced LLVM IR that `lli`
                // rejected.
                let arg_str = operand_str(arg);
                let inner = match operand_type(arg, value_types) {
                    Some(Type::Ref(_)) | Some(Type::RefMut(_)) => {
                        let tmp = format!("%v_{}.deref", instr.result.0);
                        out.push_str(&format!(
                            "  {} = load i8*, i8** {}\n",
                            tmp, arg_str
                        ));
                        tmp
                    }
                    _ => arg_str,
                };
                out.push_str(&format!(
                    "  %v_{} = call i64 @strlen(i8* {})\n",
                    instr.result.0, inner
                ));
                return Ok(());
            }
            if name == "intent_str_concat" {
                // 4-arg call (l, l_owned, r, r_owned) → i8*.
                // The runtime helper is emitted in the
                // preamble via the shared
                // `emit_intent_str_concat_definition`.
                let l = args.get(0).ok_or_else(|| EmitError {
                    message: "intent_str_concat expects 4 args".to_string(),
                })?;
                let lo = args.get(1).ok_or_else(|| EmitError {
                    message: "intent_str_concat expects 4 args".to_string(),
                })?;
                let r = args.get(2).ok_or_else(|| EmitError {
                    message: "intent_str_concat expects 4 args".to_string(),
                })?;
                let ro = args.get(3).ok_or_else(|| EmitError {
                    message: "intent_str_concat expects 4 args".to_string(),
                })?;
                // Owned flags arrive as i64 consts (0/1) at
                // the SSA layer; the helper signature takes
                // i32, so truncate.
                let lo_t = format!("%v_{}.lot", instr.result.0);
                let ro_t = format!("%v_{}.rot", instr.result.0);
                out.push_str(&format!(
                    "  {} = trunc i64 {} to i32\n",
                    lo_t,
                    operand_str(lo)
                ));
                out.push_str(&format!(
                    "  {} = trunc i64 {} to i32\n",
                    ro_t,
                    operand_str(ro)
                ));
                out.push_str(&format!(
                    "  %v_{} = call i8* @intent_str_concat(i8* {}, i32 {}, i8* {}, i32 {})\n",
                    instr.result.0,
                    operand_str(l),
                    lo_t,
                    operand_str(r),
                    ro_t
                ));
                return Ok(());
            }
            // Atomic intrinsics — five `<stdatomic.h>` ops
            // dispatched by name (mirror tree-LLVM's
            // emit, shared shape via
            // `backend_llvm::atomic_storage_llvm`/`atomic_align`).
            // Bool reductions go through an i8 shadow.
            if name == "atomic_new" {
                // Allocate the cell here so all subsequent
                // `&counter` references reuse the same
                // address. The SSA value `%v_<id>` binds to
                // the alloca pointer (`<storage>*`); body
                // code that treats `Type::Atomic(_)` as a
                // pointer-like value relies on this
                // (RefOf, atomic_load/store/fetch_add/CAS).
                // For `Atomic<bool>` the storage is i8 and
                // we zext-store the i1 initial.
                let initial = args.first().ok_or_else(|| EmitError {
                    message: "atomic_new expects 1 arg".to_string(),
                })?;
                let element = match &instr.ty {
                    Type::Atomic(elt) => (**elt).clone(),
                    other => {
                        return Err(EmitError {
                            message: format!(
                                "atomic_new result must be Atomic<T>, got {:?}",
                                other
                            ),
                        });
                    }
                };
                let storage = crate::backend_llvm::atomic_storage_llvm(&element);
                let align = crate::backend_llvm::atomic_align(&element);
                out.push_str(&format!(
                    "  %v_{} = alloca {}, align {}\n",
                    instr.result.0, storage, align
                ));
                if matches!(element, Type::Bool) {
                    let zext_tmp = format!("%v_{}.zb", instr.result.0);
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        zext_tmp,
                        operand_str(initial)
                    ));
                    out.push_str(&format!(
                        "  store i8 {}, i8* %v_{}\n",
                        zext_tmp, instr.result.0
                    ));
                } else {
                    out.push_str(&format!(
                        "  store {} {}, {}* %v_{}\n",
                        storage,
                        operand_str(initial),
                        storage,
                        instr.result.0
                    ));
                }
                return Ok(());
            }
            if name == "atomic_load" {
                // args[0]: &Atomic<T>. Element type comes from
                // the operand's resolved type. Result widens
                // back to i1 for bool.
                let cell = args.first().ok_or_else(|| EmitError {
                    message: "atomic_load expects 1 arg".to_string(),
                })?;
                let element = atomic_element_of_operand(cell, value_types)?;
                let storage = crate::backend_llvm::atomic_storage_llvm(&element);
                let align = crate::backend_llvm::atomic_align(&element);
                if matches!(element, Type::Bool) {
                    let raw = format!("%v_{}.raw", instr.result.0);
                    out.push_str(&format!(
                        "  {} = load atomic {}, {}* {} seq_cst, align {}\n",
                        raw, storage, storage, operand_str(cell), align
                    ));
                    out.push_str(&format!(
                        "  %v_{} = icmp ne i8 {}, 0\n",
                        instr.result.0, raw
                    ));
                } else {
                    out.push_str(&format!(
                        "  %v_{} = load atomic {}, {}* {} seq_cst, align {}\n",
                        instr.result.0,
                        storage,
                        storage,
                        operand_str(cell),
                        align
                    ));
                }
                return Ok(());
            }
            if name == "atomic_store" {
                let cell = args.get(0).ok_or_else(|| EmitError {
                    message: "atomic_store expects 2 args".to_string(),
                })?;
                let val = args.get(1).ok_or_else(|| EmitError {
                    message: "atomic_store expects 2 args".to_string(),
                })?;
                // Element type comes from the cell's
                // `&Atomic<T>` operand (val might be a Const
                // whose type isn't tracked in value_types).
                let element = atomic_element_of_operand(cell, value_types)?;
                let storage = crate::backend_llvm::atomic_storage_llvm(&element);
                let align = crate::backend_llvm::atomic_align(&element);
                let stored = if matches!(element, Type::Bool) {
                    let promoted = format!("%v_{}.zb", instr.result.0);
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        promoted,
                        operand_str(val)
                    ));
                    promoted
                } else {
                    operand_str(val)
                };
                out.push_str(&format!(
                    "  store atomic {} {}, {}* {} seq_cst, align {}\n",
                    storage,
                    stored,
                    storage,
                    operand_str(cell),
                    align
                ));
                // Echo: result is the value the user-level
                // store call returns (same type as val).
                let echo_ty = llvm_type(&element)?;
                if matches!(element, Type::Bool) {
                    out.push_str(&format!(
                        "  %v_{} = or i1 false, {}\n",
                        instr.result.0,
                        operand_str(val)
                    ));
                } else {
                    out.push_str(&format!(
                        "  %v_{} = add {} 0, {}\n",
                        instr.result.0,
                        echo_ty,
                        operand_str(val)
                    ));
                }
                return Ok(());
            }
            if name == "atomic_fetch_add" {
                let cell = args.get(0).ok_or_else(|| EmitError {
                    message: "atomic_fetch_add expects 2 args".to_string(),
                })?;
                let delta = args.get(1).ok_or_else(|| EmitError {
                    message: "atomic_fetch_add expects 2 args".to_string(),
                })?;
                let element = atomic_element_of_operand(cell, value_types)?;
                let storage = crate::backend_llvm::atomic_storage_llvm(&element);
                out.push_str(&format!(
                    "  %v_{} = atomicrmw add {}* {}, {} {} seq_cst\n",
                    instr.result.0,
                    storage,
                    operand_str(cell),
                    storage,
                    operand_str(delta)
                ));
                return Ok(());
            }
            if name == "atomic_compare_exchange" {
                let cell = args.get(0).ok_or_else(|| EmitError {
                    message: "atomic_compare_exchange expects 3 args".to_string(),
                })?;
                let exp = args.get(1).ok_or_else(|| EmitError {
                    message: "atomic_compare_exchange expects 3 args".to_string(),
                })?;
                let new_v = args.get(2).ok_or_else(|| EmitError {
                    message: "atomic_compare_exchange expects 3 args".to_string(),
                })?;
                // Element type comes from the CELL's
                // `&Atomic<T>` operand (Const expected/new
                // values don't carry the type). CAS returns
                // `{ <T>, i1 }`; we extract the i1.
                let element = atomic_element_of_operand(cell, value_types)?;
                let storage = crate::backend_llvm::atomic_storage_llvm(&element);
                let (exp_str, new_str) = if matches!(element, Type::Bool) {
                    let exp_p = format!("%v_{}.exp8", instr.result.0);
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        exp_p,
                        operand_str(exp)
                    ));
                    let new_p = format!("%v_{}.new8", instr.result.0);
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        new_p,
                        operand_str(new_v)
                    ));
                    (exp_p, new_p)
                } else {
                    (operand_str(exp), operand_str(new_v))
                };
                let pair = format!("%v_{}.cas", instr.result.0);
                out.push_str(&format!(
                    "  {} = cmpxchg {}* {}, {} {}, {} {} seq_cst seq_cst\n",
                    pair,
                    storage,
                    operand_str(cell),
                    storage,
                    exp_str,
                    storage,
                    new_str
                ));
                out.push_str(&format!(
                    "  %v_{} = extractvalue {{ {}, i1 }} {}, 1\n",
                    instr.result.0, storage, pair
                ));
                return Ok(());
            }
            // Mutex / Guard intrinsics — i64-only in v1.
            // `mutex_new` allocas the cell (so refs share the
            // address) and initializes both fields.
            if name == "mutex_new" {
                let initial = args.first().ok_or_else(|| EmitError {
                    message: "mutex_new expects 1 arg".to_string(),
                })?;
                let n = instr.result.0;
                out.push_str(&format!(
                    "  %v_{} = alloca %intent_mutex_i64, align 8\n",
                    n
                ));
                let val_p = format!("%v_{}.vp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* %v_{}, i32 0, i32 0\n",
                    val_p, n
                ));
                out.push_str(&format!(
                    "  store i64 {}, i64* {}\n",
                    operand_str(initial),
                    val_p
                ));
                let lock_p = format!("%v_{}.lp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* %v_{}, i32 0, i32 1\n",
                    lock_p, n
                ));
                out.push_str(&format!(
                    "  store i32 0, i32* {}\n",
                    lock_p
                ));
                return Ok(());
            }
            // `mutex_lock` runs Drepper's three-state futex
            // protocol and returns a Guard pointer aliased to
            // the same mutex. We allocate the guard's
            // `{ %intent_mutex_i64* }` slot, fill it, and use
            // the alloca pointer as the SSA value.
            if name == "mutex_lock" {
                let m_arg = args.first().ok_or_else(|| EmitError {
                    message: "mutex_lock expects 1 arg".to_string(),
                })?;
                let n = instr.result.0;
                let m_ptr = operand_str(m_arg);
                let locked_p = format!("%v_{}.lockp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 1\n",
                    locked_p, m_ptr
                ));
                let c_p = format!("%v_{}.cp", n);
                out.push_str(&format!("  {} = alloca i32\n", c_p));
                // Per-region labels — keyed on the SSA value
                // id so each mutex_lock site gets unique
                // labels.
                let l_slow = format!("mu{}.slow", n);
                let l_mark = format!("mu{}.mark", n);
                let l_store_init = format!("mu{}.store_init", n);
                let l_loop = format!("mu{}.loop", n);
                let l_park = format!("mu{}.park", n);
                let l_acquired = format!("mu{}.acquired", n);
                // Fast-path CAS 0→1.
                let cx0 = format!("%v_{}.cx0", n);
                out.push_str(&format!(
                    "  {} = cmpxchg i32* {}, i32 0, i32 1 seq_cst seq_cst\n",
                    cx0, locked_p
                ));
                let won0 = format!("%v_{}.won0", n);
                out.push_str(&format!(
                    "  {} = extractvalue {{ i32, i1 }} {}, 1\n",
                    won0, cx0
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    won0, l_acquired, l_slow
                ));
                // Slow path.
                out.push_str(&format!("{}:\n", l_slow));
                let c0 = format!("%v_{}.c0", n);
                out.push_str(&format!(
                    "  {} = extractvalue {{ i32, i1 }} {}, 0\n",
                    c0, cx0
                ));
                let need_mark = format!("%v_{}.need_mark", n);
                out.push_str(&format!(
                    "  {} = icmp ne i32 {}, 2\n",
                    need_mark, c0
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    need_mark, l_mark, l_store_init
                ));
                out.push_str(&format!("{}:\n", l_mark));
                let c_marked = format!("%v_{}.c_marked", n);
                out.push_str(&format!(
                    "  {} = atomicrmw xchg i32* {}, i32 2 seq_cst\n",
                    c_marked, locked_p
                ));
                out.push_str(&format!(
                    "  store i32 {}, i32* {}\n",
                    c_marked, c_p
                ));
                out.push_str(&format!("  br label %{}\n", l_loop));
                out.push_str(&format!("{}:\n", l_store_init));
                out.push_str(&format!(
                    "  store i32 {}, i32* {}\n",
                    c0, c_p
                ));
                out.push_str(&format!("  br label %{}\n", l_loop));
                // Loop head.
                out.push_str(&format!("{}:\n", l_loop));
                let c_v = format!("%v_{}.c", n);
                out.push_str(&format!(
                    "  {} = load i32, i32* {}\n",
                    c_v, c_p
                ));
                let still_locked = format!("%v_{}.still", n);
                out.push_str(&format!(
                    "  {} = icmp ne i32 {}, 0\n",
                    still_locked, c_v
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    still_locked, l_park, l_acquired
                ));
                // Park.
                out.push_str(&format!("{}:\n", l_park));
                if crate::backend_llvm::host_uses_win32_threading() {
                    let cmp_slot = format!("%v_{}.cmp_slot", n);
                    out.push_str(&format!("  {} = alloca i32\n", cmp_slot));
                    out.push_str(&format!(
                        "  store i32 2, i32* {}\n",
                        cmp_slot
                    ));
                    let addr_i8 = format!("%v_{}.addr_i8", n);
                    let cmp_i8 = format!("%v_{}.cmp_i8", n);
                    out.push_str(&format!(
                        "  {} = bitcast i32* {} to i8*\n",
                        addr_i8, locked_p
                    ));
                    out.push_str(&format!(
                        "  {} = bitcast i32* {} to i8*\n",
                        cmp_i8, cmp_slot
                    ));
                    let wait_ret = format!("%v_{}.wait_ret", n);
                    out.push_str(&format!(
                        "  {} = call i32 @WaitOnAddress(i8* {}, i8* {}, i64 4, i32 -1)\n",
                        wait_ret, addr_i8, cmp_i8
                    ));
                } else {
                    let futex_ret = format!("%v_{}.futex_ret", n);
                    out.push_str(&format!(
                        "  {} = call i64 (i64, ...) @syscall(i64 {}, i32* {}, i32 128, i32 2, i8* null, i8* null, i32 0)\n",
                        futex_ret,
                        crate::backend_llvm::sys_futex_for_host(),
                        locked_p
                    ));
                }
                let c_after = format!("%v_{}.c_after", n);
                out.push_str(&format!(
                    "  {} = atomicrmw xchg i32* {}, i32 2 seq_cst\n",
                    c_after, locked_p
                ));
                out.push_str(&format!(
                    "  store i32 {}, i32* {}\n",
                    c_after, c_p
                ));
                out.push_str(&format!("  br label %{}\n", l_loop));
                // Acquired: build the guard alloca + populate.
                out.push_str(&format!("{}:\n", l_acquired));
                out.push_str(&format!(
                    "  %v_{} = alloca %intent_guard_i64\n",
                    n
                ));
                let g_mp = format!("%v_{}.g_mp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* %v_{}, i32 0, i32 0\n",
                    g_mp, n
                ));
                out.push_str(&format!(
                    "  store %intent_mutex_i64* {}, %intent_mutex_i64** {}\n",
                    m_ptr, g_mp
                ));
                return Ok(());
            }
            if name == "guard_get" {
                let g = args.first().ok_or_else(|| EmitError {
                    message: "guard_get expects 1 arg".to_string(),
                })?;
                let n = instr.result.0;
                let mp_p = format!("%v_{}.mp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* {}, i32 0, i32 0\n",
                    mp_p, operand_str(g)
                ));
                let m_ptr = format!("%v_{}.mptr", n);
                out.push_str(&format!(
                    "  {} = load %intent_mutex_i64*, %intent_mutex_i64** {}\n",
                    m_ptr, mp_p
                ));
                let value_p = format!("%v_{}.vp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 0\n",
                    value_p, m_ptr
                ));
                out.push_str(&format!(
                    "  %v_{} = load i64, i64* {}\n",
                    n, value_p
                ));
                return Ok(());
            }
            if name == "guard_set" {
                let g = args.get(0).ok_or_else(|| EmitError {
                    message: "guard_set expects 2 args".to_string(),
                })?;
                let v = args.get(1).ok_or_else(|| EmitError {
                    message: "guard_set expects 2 args".to_string(),
                })?;
                let n = instr.result.0;
                let mp_p = format!("%v_{}.mp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* {}, i32 0, i32 0\n",
                    mp_p, operand_str(g)
                ));
                let m_ptr = format!("%v_{}.mptr", n);
                out.push_str(&format!(
                    "  {} = load %intent_mutex_i64*, %intent_mutex_i64** {}\n",
                    m_ptr, mp_p
                ));
                let value_p = format!("%v_{}.vp", n);
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 0\n",
                    value_p, m_ptr
                ));
                out.push_str(&format!(
                    "  store i64 {}, i64* {}\n",
                    operand_str(v),
                    value_p
                ));
                // Echo the stored value so downstream code
                // sees the assignment's result.
                out.push_str(&format!(
                    "  %v_{} = add i64 0, {}\n",
                    n,
                    operand_str(v)
                ));
                return Ok(());
            }
            // Channel intrinsics — Vyukov MPSC ring buffer.
            // `channel_new` allocas the per-(T, N) struct and
            // initializes seq[i]=i, head=0, tail=0, buf=zero.
            if name == "channel_new" {
                let (element, capacity) = match &instr.ty {
                    Type::Channel(elt, cap) => ((**elt).clone(), *cap),
                    other => {
                        return Err(EmitError {
                            message: format!(
                                "channel_new must return Channel<T, N>, got {:?}",
                                other
                            ),
                        });
                    }
                };
                let struct_ty =
                    crate::backend_llvm::llvm_channel_struct(&element, capacity);
                let slot = crate::backend_llvm::channel_slot_llvm(&element);
                let n = instr.result.0;
                out.push_str(&format!(
                    "  %v_{} = alloca {}, align 8\n",
                    n, struct_ty
                ));
                // Zero-init the buf field via store of
                // `[N x slot] zeroinitializer`.
                let buf_p = format!("%v_{}.bufp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* %v_{}, i32 0, i32 0\n",
                    buf_p, struct_ty, struct_ty, n
                ));
                out.push_str(&format!(
                    "  store [{} x {}] zeroinitializer, [{} x {}]* {}\n",
                    capacity, slot, capacity, slot, buf_p
                ));
                // Initialize seq = [0, 1, 2, ..., N-1].
                let mut seq_init = String::from("[");
                for i in 0..capacity {
                    if i > 0 {
                        seq_init.push_str(", ");
                    }
                    seq_init.push_str(&format!("i64 {}", i));
                }
                seq_init.push(']');
                let seq_p = format!("%v_{}.seqp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* %v_{}, i32 0, i32 1\n",
                    seq_p, struct_ty, struct_ty, n
                ));
                out.push_str(&format!(
                    "  store [{} x i64] {}, [{} x i64]* {}\n",
                    capacity, seq_init, capacity, seq_p
                ));
                // head = 0, tail = 0.
                let head_p = format!("%v_{}.headp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* %v_{}, i32 0, i32 2\n",
                    head_p, struct_ty, struct_ty, n
                ));
                out.push_str(&format!(
                    "  store i64 0, i64* {}\n",
                    head_p
                ));
                let tail_p = format!("%v_{}.tailp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* %v_{}, i32 0, i32 3\n",
                    tail_p, struct_ty, struct_ty, n
                ));
                out.push_str(&format!(
                    "  store i64 0, i64* {}\n",
                    tail_p
                ));
                return Ok(());
            }
            // `channel_send` — Vyukov producer protocol: load
            // tail, check seq[tail&MASK] == tail (slot ready
            // for round `tail`), CAS-claim tail, then write
            // buf[tail&MASK] = v and publish seq[idx] = tail+1.
            if name == "channel_send" {
                let chan = args.get(0).ok_or_else(|| EmitError {
                    message: "channel_send expects 2 args".to_string(),
                })?;
                let val_op = args.get(1).ok_or_else(|| EmitError {
                    message: "channel_send expects 2 args".to_string(),
                })?;
                let chan_ty = operand_type(chan, value_types).ok_or_else(|| EmitError {
                    message: "channel_send arg has unknown type".to_string(),
                })?;
                let (element, capacity) = channel_inner_from_ty_llvm(&chan_ty)?;
                let struct_ty =
                    crate::backend_llvm::llvm_channel_struct(&element, capacity);
                let slot = crate::backend_llvm::channel_slot_llvm(&element);
                let mask = capacity - 1;
                let p = operand_str(chan);
                let n = instr.result.0;

                // Widen i1 → i8 for bool channels.
                let v_in = operand_str(val_op);
                let stored = if matches!(element, Type::Bool) {
                    let promoted = format!("%v_{}.zb", n);
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        promoted, v_in
                    ));
                    promoted
                } else {
                    v_in.clone()
                };
                let tail_p = format!("%v_{}.tailp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 3\n",
                    tail_p, struct_ty, struct_ty, p
                ));
                let l_spin = format!("ch{}.send_spin", n);
                let l_try = format!("ch{}.send_try", n);
                let l_write = format!("ch{}.send_write", n);
                out.push_str(&format!("  br label %{}\n", l_spin));
                out.push_str(&format!("{}:\n", l_spin));
                let cur_t = format!("%v_{}.cur_t", n);
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_t, tail_p
                ));
                let idx = format!("%v_{}.idx", n);
                out.push_str(&format!(
                    "  {} = and i64 {}, {}\n",
                    idx, cur_t, mask
                ));
                let seq_p = format!("%v_{}.seqp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 1, i64 {}\n",
                    seq_p, struct_ty, struct_ty, p, idx
                ));
                let cur_s = format!("%v_{}.cur_s", n);
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_s, seq_p
                ));
                let ready = format!("%v_{}.ready", n);
                out.push_str(&format!(
                    "  {} = icmp eq i64 {}, {}\n",
                    ready, cur_s, cur_t
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    ready, l_try, l_spin
                ));
                out.push_str(&format!("{}:\n", l_try));
                let next_t = format!("%v_{}.next_t", n);
                out.push_str(&format!(
                    "  {} = add i64 {}, 1\n",
                    next_t, cur_t
                ));
                let cx = format!("%v_{}.cx", n);
                out.push_str(&format!(
                    "  {} = cmpxchg i64* {}, i64 {}, i64 {} seq_cst seq_cst\n",
                    cx, tail_p, cur_t, next_t
                ));
                let won = format!("%v_{}.won", n);
                out.push_str(&format!(
                    "  {} = extractvalue {{ i64, i1 }} {}, 1\n",
                    won, cx
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    won, l_write, l_spin
                ));
                out.push_str(&format!("{}:\n", l_write));
                let slot_p = format!("%v_{}.slot_p", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 0, i64 {}\n",
                    slot_p, struct_ty, struct_ty, p, idx
                ));
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    slot, stored, slot, slot_p
                ));
                out.push_str(&format!(
                    "  store atomic i64 {}, i64* {} seq_cst, align 8\n",
                    next_t, seq_p
                ));
                // Echo the source-level value (i1 for bool).
                if matches!(element, Type::Bool) {
                    out.push_str(&format!(
                        "  %v_{} = or i1 false, {}\n",
                        n, v_in
                    ));
                } else {
                    let nty = llvm_type(&element)?;
                    out.push_str(&format!(
                        "  %v_{} = add {} 0, {}\n",
                        n, nty, v_in
                    ));
                }
                return Ok(());
            }
            if name == "channel_recv" {
                let chan = args.first().ok_or_else(|| EmitError {
                    message: "channel_recv expects 1 arg".to_string(),
                })?;
                let chan_ty = operand_type(chan, value_types).ok_or_else(|| EmitError {
                    message: "channel_recv arg has unknown type".to_string(),
                })?;
                let (element, capacity) = channel_inner_from_ty_llvm(&chan_ty)?;
                let struct_ty =
                    crate::backend_llvm::llvm_channel_struct(&element, capacity);
                let slot = crate::backend_llvm::channel_slot_llvm(&element);
                let mask = capacity - 1;
                let p = operand_str(chan);
                let n = instr.result.0;
                let head_p = format!("%v_{}.headp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 2\n",
                    head_p, struct_ty, struct_ty, p
                ));
                let cur_h = format!("%v_{}.cur_h", n);
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_h, head_p
                ));
                let idx = format!("%v_{}.idx", n);
                out.push_str(&format!(
                    "  {} = and i64 {}, {}\n",
                    idx, cur_h, mask
                ));
                let seq_p = format!("%v_{}.seqp", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 1, i64 {}\n",
                    seq_p, struct_ty, struct_ty, p, idx
                ));
                let target = format!("%v_{}.target", n);
                out.push_str(&format!(
                    "  {} = add i64 {}, 1\n",
                    target, cur_h
                ));
                let l_spin = format!("ch{}.recv_spin", n);
                let l_body = format!("ch{}.recv_body", n);
                out.push_str(&format!("  br label %{}\n", l_spin));
                out.push_str(&format!("{}:\n", l_spin));
                let cur_s = format!("%v_{}.cur_s", n);
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_s, seq_p
                ));
                let ready = format!("%v_{}.ready", n);
                out.push_str(&format!(
                    "  {} = icmp eq i64 {}, {}\n",
                    ready, cur_s, target
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    ready, l_body, l_spin
                ));
                out.push_str(&format!("{}:\n", l_body));
                let slot_p = format!("%v_{}.slot_p", n);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 0, i64 {}\n",
                    slot_p, struct_ty, struct_ty, p, idx
                ));
                let val = format!("%v_{}.val", n);
                out.push_str(&format!(
                    "  {} = load {}, {}* {}\n",
                    val, slot, slot, slot_p
                ));
                let release = format!("%v_{}.release", n);
                out.push_str(&format!(
                    "  {} = add i64 {}, {}\n",
                    release, cur_h, capacity
                ));
                out.push_str(&format!(
                    "  store atomic i64 {}, i64* {} seq_cst, align 8\n",
                    release, seq_p
                ));
                let next_h = format!("%v_{}.next_h", n);
                out.push_str(&format!(
                    "  {} = add i64 {}, 1\n",
                    next_h, cur_h
                ));
                out.push_str(&format!(
                    "  store atomic i64 {}, i64* {} seq_cst, align 8\n",
                    next_h, head_p
                ));
                // Bool slots stored as i8 — trunc back to i1.
                if matches!(element, Type::Bool) {
                    out.push_str(&format!(
                        "  %v_{} = icmp ne i8 {}, 0\n",
                        n, val
                    ));
                } else {
                    let nty = llvm_type(&element)?;
                    out.push_str(&format!(
                        "  %v_{} = add {} 0, {}\n",
                        n, nty, val
                    ));
                }
                return Ok(());
            }
            // clone_at(xs, i) returns the element type, not
            // a Vec — handle separately from the Vec-returning
            // builtins below. Refines #7 phase 2d.
            if name == "clone_at" {
                let xs_ty = operand_type(&args[0], value_types).ok_or_else(|| {
                    EmitError {
                        message: "clone_at xs operand has unknown type".to_string(),
                    }
                })?;
                let (element_ty, xs_via_ref) = match &xs_ty {
                    Type::Ref(inner) | Type::RefMut(inner) => match &**inner {
                        Type::Vec(e) => ((**e).clone(), true),
                        other => {
                            return Err(EmitError {
                                message: format!(
                                    "clone_at expects &Vec<T> or Vec<T>, got {:?}",
                                    other
                                ),
                            });
                        }
                    },
                    Type::Vec(e) => ((**e).clone(), false),
                    other => {
                        return Err(EmitError {
                            message: format!(
                                "clone_at expects &Vec<T> or Vec<T>, got {:?}",
                                other
                            ),
                        });
                    }
                };
                let struct_ty =
                    crate::backend_llvm::vec_struct_name(&element_ty);
                let elt_ty =
                    crate::backend_llvm::vec_element_value_str(&element_ty);
                let result = instr.result;
                // Materialize xs as a struct-pointer so we can
                // GEP its `data` field. If xs is already a
                // ref (the typical use), the operand IS the
                // pointer. If xs is a Vec value, alloca a
                // shadow + store the value first.
                let xs_ptr = if xs_via_ref {
                    operand_str(&args[0]).to_string()
                } else {
                    let p = format!("%v_{}.cat_p", result.0);
                    out.push_str(&format!(
                        "  {} = alloca {}\n",
                        p, struct_ty
                    ));
                    out.push_str(&format!(
                        "  store {} {}, {}* {}\n",
                        struct_ty,
                        operand_str(&args[0]),
                        struct_ty,
                        p
                    ));
                    p
                };
                let data_pp = format!("%v_{}.cat_dp", result.0);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 0\n",
                    data_pp, struct_ty, struct_ty, xs_ptr
                ));
                let data_p = format!("%v_{}.cat_d", result.0);
                out.push_str(&format!(
                    "  {} = load {}*, {}** {}\n",
                    data_p, elt_ty, elt_ty, data_pp
                ));
                let slot_p = format!("%v_{}.cat_sp", result.0);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 {}\n",
                    slot_p, elt_ty, elt_ty, data_p, operand_str(&args[1])
                ));
                if element_ty.is_copy() {
                    // Copy element: load the slot, return its
                    // value. Struct-copy semantics are a fresh
                    // independent value.
                    out.push_str(&format!(
                        "  %v_{} = load {}, {}* {}\n",
                        result.0, elt_ty, elt_ty, slot_p
                    ));
                } else if let Type::Vec(inner) = &element_ty {
                    // Vec element: load the slot then call
                    // ITS OWN __clone helper. The helper is
                    // tagged by the inner's element type
                    // (`vec_helper(inner, "clone")`), since
                    // we're cloning a Vec<inner> value.
                    let slot_v = format!("%v_{}.cat_sv", result.0);
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        slot_v, elt_ty, elt_ty, slot_p
                    ));
                    let clone_name = format!(
                        "@intent_vec_{}__clone",
                        crate::backend_llvm::vec_struct_tag(inner),
                    );
                    out.push_str(&format!(
                        "  %v_{} = call {} {}({} {})\n",
                        result.0, elt_ty, clone_name, elt_ty, slot_v
                    ));
                } else {
                    return Err(EmitError {
                        message: format!(
                            "clone_at on element type {:?} not yet supported",
                            element_ty
                        ),
                    });
                }
                return Ok(());
            }
            // Vec builtins call through the shared runtime
            // helpers emitted in the module preamble.
            if matches!(name.as_str(), "vec" | "push" | "set" | "clone") {
                let element = match &instr.ty {
                    Type::Vec(elt) => (**elt).clone(),
                    other => {
                        return Err(EmitError {
                            message: format!(
                                "{} call must return Vec<T>, got {:?}",
                                name, other
                            ),
                        });
                    }
                };
                emit_vec_call(name, &element, args, instr.result, value_types, out)?;
                return Ok(());
            }
            let ret_ty = llvm_type_string(&instr.ty)?;
            // Resolve each arg's type via, in order: the
            // operand's value-type, the callee's declared
            // parameter type, and (last resort) i64. The
            // middle step matters for Const operands —
            // `operand_type` returns None for them, and the
            // call's RETURN type is the wrong default for the
            // arg.
            let param_tys: Option<&Vec<Type>> =
                fn_sigs.get(name).map(|(p, _)| p);
            let arg_pairs: Result<Vec<String>, EmitError> = args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let aty = operand_type(a, value_types)
                        .or_else(|| {
                            param_tys
                                .and_then(|tys| tys.get(i))
                                .cloned()
                        })
                        .unwrap_or(Type::I64);
                    Ok(format!("{} {}", llvm_type_string(&aty)?, operand_str(a)))
                })
                .collect();
            let arg_pairs = arg_pairs?;
            out.push_str(&format!(
                "  %v_{} = call {} @fn_{}({})\n",
                instr.result.0,
                ret_ty,
                name,
                arg_pairs.join(", ")
            ));
        }
        InstrKind::ArrayLit { elements } => {
            // alloca the array, store each element via GEP,
            // and hand back the alloca pointer as the SSA
            // value. `llvm_type_string` for Type::Array
            // returns `[N x T]*` so the value-type machinery
            // (phi, calls) stays consistent.
            let (element_ty, length) = match &instr.ty {
                Type::Array { element, length } => ((**element).clone(), *length),
                other => {
                    return Err(EmitError {
                        message: format!(
                            "ArrayLit result type must be Array, got {:?}",
                            other
                        ),
                    });
                }
            };
            let elt_ty = llvm_type(&element_ty)?;
            let array_ty = format!("[{} x {}]", length, elt_ty);
            // alloca the array.
            out.push_str(&format!(
                "  %v_{} = alloca {}\n",
                instr.result.0, array_ty
            ));
            // Populate each slot via GEP + store.
            for (i, e) in elements.iter().enumerate() {
                let p = format!("%v_{}.s{}", instr.result.0, i);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* %v_{}, i64 0, i64 {}\n",
                    p, array_ty, array_ty, instr.result.0, i
                ));
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    elt_ty,
                    operand_str(e),
                    elt_ty,
                    p
                ));
            }
        }
        InstrKind::Index { array, index, .. } => {
            // Vec or Array? Dispatch on source operand type.
            let elt_ty = llvm_type(&instr.ty)?;
            let array_ty = operand_type(array, value_types);
            match array_ty.as_ref().map(|t| t.deref().clone()) {
                Some(Type::Vec(elt)) => {
                    // Vec: read .data, GEP, load. If the source
                    // is Ref(Vec) (param case), load the
                    // aggregate from the pointer first.
                    let struct_ty = crate::backend_llvm::vec_struct_name(&elt);
                    let elt_llvm = crate::backend_llvm::llvm_type(&elt);
                    let agg = vec_aggregate_operand(
                        array, value_types, &struct_ty, "agg", instr.result, out,
                    );
                    let data_p = format!("%v_{}.dp", instr.result.0);
                    out.push_str(&format!(
                        "  {} = extractvalue {} {}, 0\n",
                        data_p, struct_ty, agg
                    ));
                    let p = format!("%v_{}.p", instr.result.0);
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, {}\n",
                        p,
                        elt_llvm,
                        elt_llvm,
                        data_p,
                        index_typed(index, value_types)?
                    ));
                    out.push_str(&format!(
                        "  %v_{} = load {}, {}* {}\n",
                        instr.result.0, elt_llvm, elt_llvm, p
                    ));
                }
                _ => {
                    // Fixed-size array. If the source is
                    // Ref([T; N]) (param case), load the array
                    // pointer through the double-pointer first.
                    let array_ty_str = array_operand_pointee(array, value_types)?;
                    let arr_ptr = array_pointer_operand(
                        array,
                        value_types,
                        &array_ty_str,
                        "ap",
                        instr.result,
                        out,
                    );
                    let p = format!("%v_{}.p", instr.result.0);
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, {}\n",
                        p,
                        array_ty_str,
                        array_ty_str,
                        arr_ptr,
                        index_typed(index, value_types)?
                    ));
                    out.push_str(&format!(
                        "  %v_{} = load {}, {}* {}\n",
                        instr.result.0, elt_ty, elt_ty, p
                    ));
                }
            }
        }
        InstrKind::IndexAssign { array, index, value, .. } => {
            // GEP into the array, store. The instruction
            // produces no useful SSA value at the source
            // level; we still need an SSA name (forward-
            // declared by `value_types`) but don't use it.
            // To avoid an unused-result issue, emit nothing
            // bound to %v_<id>.
            // base_ty has the array shape we need for the
            // GEP-into type; fall back via the value's
            // operand type otherwise.
            let array_ty_str = array_operand_pointee(array, value_types)?;
            let elt_ty = match operand_type(value, value_types) {
                Some(t) => llvm_type_string(&t)?,
                None => {
                    // Const value — derive from base_ty.
                    if let InstrKind::IndexAssign { base_ty, .. } = &instr.kind {
                        if let Type::Array { element, .. } = base_ty {
                            llvm_type(element)?.to_string()
                        } else {
                            "i64".to_string()
                        }
                    } else {
                        "i64".to_string()
                    }
                }
            };
            let arr_ptr = array_pointer_operand(
                array, value_types, &array_ty_str, "ap", instr.result, out,
            );
            let p = format!("%v_{}.p", instr.result.0);
            out.push_str(&format!(
                "  {} = getelementptr {}, {}* {}, i64 0, {}\n",
                p,
                array_ty_str,
                array_ty_str,
                arr_ptr,
                index_typed(index, value_types)?
            ));
            out.push_str(&format!(
                "  store {} {}, {}* {}\n",
                elt_ty,
                operand_str(value),
                elt_ty,
                p
            ));
        }
        InstrKind::Len { array, length } => {
            // Array: compile-time length constant. Vec:
            // extract the .len field at runtime (loading the
            // aggregate first if the source is Ref(Vec)).
            let array_ty = operand_type(array, value_types);
            match array_ty.as_ref().map(|t| t.deref().clone()) {
                Some(Type::Vec(elt)) => {
                    let struct_ty = crate::backend_llvm::vec_struct_name(&elt);
                    let agg = vec_aggregate_operand(
                        array, value_types, &struct_ty, "agg", instr.result, out,
                    );
                    out.push_str(&format!(
                        "  %v_{} = extractvalue {} {}, 1\n",
                        instr.result.0, struct_ty, agg
                    ));
                }
                _ => {
                    out.push_str(&format!(
                        "  %v_{} = add i64 0, {}\n",
                        instr.result.0, length
                    ));
                }
            }
        }
        InstrKind::Drop { source, ty, .. } => {
            // Vec: extract the data pointer, free the heap
            // buffer inline (loading the aggregate first if
            // the source is Ref(Vec)). Stack types are no-ops.
            match ty {
                Type::Vec(element) => {
                    // Route through the per-element-type
                    // `__free` helper so nested-Vec elements
                    // get recursively released. Refines #7
                    // (previously called `@free` directly on
                    // `xs.data`, leaking each inner element's
                    // heap when the element type was non-Copy).
                    let struct_ty = crate::backend_llvm::vec_struct_name(element);
                    let agg = vec_aggregate_operand(
                        source, value_types, &struct_ty, "drop_agg", instr.result, out,
                    );
                    let free_name = format!(
                        "@intent_vec_{}__free",
                        crate::backend_llvm::vec_struct_tag(element)
                    );
                    out.push_str(&format!(
                        "  call void {}({} {})\n",
                        free_name, struct_ty, agg
                    ));
                }
                Type::Array { .. }
                | Type::Task
                | Type::Atomic(_)
                | Type::Mutex(_)
                | Type::Channel(_, _) => {
                    // Stack-allocated; alloca goes away at
                    // function return. No-op. (Atomic / Mutex
                    // / Channel cells live in an alloca whose
                    // lifetime ends with the function.)
                }
                Type::Guard(_) => {
                    // RAII unlock + futex/WaitOnAddress wake.
                    // Drepper protocol: atomicrmw sub 1
                    // returns the OLD state; if it was 1
                    // (no waiters), state is now 0 and we're
                    // done. If it was 2 (waiters present),
                    // we reset state to 0 and wake one
                    // waiter.
                    let n = instr.result.0;
                    let g_ptr = operand_str(source);
                    let mp_p = format!("%v_{}.unlock_mp", n);
                    out.push_str(&format!(
                        "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* {}, i32 0, i32 0\n",
                        mp_p, g_ptr
                    ));
                    let m_ptr = format!("%v_{}.unlock_mptr", n);
                    out.push_str(&format!(
                        "  {} = load %intent_mutex_i64*, %intent_mutex_i64** {}\n",
                        m_ptr, mp_p
                    ));
                    let locked_p = format!("%v_{}.unlock_lp", n);
                    out.push_str(&format!(
                        "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 1\n",
                        locked_p, m_ptr
                    ));
                    let old = format!("%v_{}.unlock_old", n);
                    out.push_str(&format!(
                        "  {} = atomicrmw sub i32* {}, i32 1 seq_cst\n",
                        old, locked_p
                    ));
                    let had_waiters = format!("%v_{}.unlock_had_w", n);
                    out.push_str(&format!(
                        "  {} = icmp ne i32 {}, 1\n",
                        had_waiters, old
                    ));
                    let l_wake = format!("mu{}.unlock_wake", n);
                    let l_done = format!("mu{}.unlock_done", n);
                    out.push_str(&format!(
                        "  br i1 {}, label %{}, label %{}\n",
                        had_waiters, l_wake, l_done
                    ));
                    out.push_str(&format!("{}:\n", l_wake));
                    // Reset state to 0 (the fetch_sub left
                    // it at 1) and wake one waiter via the
                    // host's kernel-wait primitive.
                    out.push_str(&format!(
                        "  store atomic i32 0, i32* {} seq_cst, align 4\n",
                        locked_p
                    ));
                    if crate::backend_llvm::host_uses_win32_threading() {
                        let locked_i8 = format!("%v_{}.locked_i8", n);
                        out.push_str(&format!(
                            "  {} = bitcast i32* {} to i8*\n",
                            locked_i8, locked_p
                        ));
                        out.push_str(&format!(
                            "  call void @WakeByAddressSingle(i8* {})\n",
                            locked_i8
                        ));
                    } else {
                        let wake_ret = format!("%v_{}.wake_ret", n);
                        out.push_str(&format!(
                            "  {} = call i64 (i64, ...) @syscall(i64 {}, i32* {}, i32 129, i32 1, i8* null, i8* null, i32 0)\n",
                            wake_ret,
                            crate::backend_llvm::sys_futex_for_host(),
                            locked_p
                        ));
                    }
                    out.push_str(&format!("  br label %{}\n", l_done));
                    out.push_str(&format!("{}:\n", l_done));
                }
                Type::OwnedStr => {
                    // OwnedStr is an i8* heap pointer
                    // returned by `intent_str_concat`. Drop
                    // frees the buffer; no metadata to
                    // extract.
                    out.push_str(&format!(
                        "  call void @free(i8* {})\n",
                        operand_str(source)
                    ));
                }
                _ => {
                    return Err(EmitError {
                        message: format!(
                            "Drop of {:?} not yet lowered in SSA-LLVM",
                            ty
                        ),
                    });
                }
            }
        }
        InstrKind::FnRef { name } => {
            // First-class function pointer to a top-level
            // function. The LLVM symbol is `@fn_<name>`; we
            // materialize an SSA value via `bitcast … to …`
            // which gives us a typed pointer of the
            // expected fn-ptr type.
            let fn_ptr_ty = llvm_type_string(&instr.ty)?;
            out.push_str(&format!(
                "  %v_{} = bitcast {} @fn_{} to {}\n",
                instr.result.0, fn_ptr_ty, name, fn_ptr_ty
            ));
        }
        InstrKind::CallIndirect { callee, args } => {
            // Indirect call: `call <ret> (<params>) %fp(args)`.
            // The callee's source-language type is FnPtr,
            // which spells the function-pointer type. Args
            // need their LLVM types prepended for the call
            // syntax.
            let callee_ty = operand_type(callee, value_types).ok_or_else(|| EmitError {
                message: "indirect-call callee has unknown SSA type".to_string(),
            })?;
            let (param_tys, ret_ty) = match callee_ty {
                Type::FnPtr(params, ret) => (params, *ret),
                other => {
                    return Err(EmitError {
                        message: format!(
                            "indirect-call callee must have FnPtr type, got {:?}",
                            other
                        ),
                    });
                }
            };
            let ret_ty_str = llvm_type_string(&ret_ty)?;
            let param_strs: Result<Vec<String>, EmitError> =
                param_tys.iter().map(llvm_type_string).collect();
            let signature = format!("{} ({})", ret_ty_str, param_strs?.join(", "));
            let arg_pairs: Result<Vec<String>, EmitError> = args
                .iter()
                .zip(param_tys.iter())
                .map(|(a, t)| Ok(format!("{} {}", llvm_type_string(t)?, operand_str(a))))
                .collect();
            out.push_str(&format!(
                "  %v_{} = call {} {}({})\n",
                instr.result.0,
                signature,
                operand_str(callee),
                arg_pairs?.join(", ")
            ));
        }
        InstrKind::StrLit(text) => {
            // Emit a private constant global holding the
            // bytes + NUL terminator, then GEP into it to
            // produce an `i8*` SSA value.
            let n = STR_COUNTER.with(|c| {
                let v = c.get();
                c.set(v + 1);
                v
            });
            let bytes_len = text.len() + 1;
            let escaped = escape_llvm_str(text);
            STR_GLOBALS.with(|b| {
                b.borrow_mut().push_str(&format!(
                    "@.str.{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"\n",
                    n, bytes_len, escaped
                ));
            });
            out.push_str(&format!(
                "  %v_{} = getelementptr [{} x i8], [{} x i8]* @.str.{}, i64 0, i64 0\n",
                instr.result.0, bytes_len, bytes_len, n
            ));
        }
        InstrKind::RefOf { source, mut_ } => {
            // `&v` / `&mut v`. If `source` already produces a
            // pointer-typed SSA value (an alloca'd array, or
            // a ref), use it directly (with a bitcast to the
            // declared result type). Otherwise the source is
            // a scalar SSA value — we materialize an alloca,
            // store the value into it, and return the
            // alloca's address.
            let src_ty = operand_type(source, value_types);
            let result_ty_str = llvm_type_string(&instr.ty)?;
            let _ = mut_;
            let needs_alloca = match &src_ty {
                Some(t) => !matches!(
                    t,
                    Type::Array { .. }
                        | Type::Ref(_)
                        | Type::RefMut(_)
                        | Type::Atomic(_)
                        | Type::Mutex(_)
                        | Type::Guard(_)
                        | Type::Channel(_, _)
                ),
                None => true, // Const operand: scalar — alloca.
            };
            if needs_alloca {
                let elt_ty = src_ty
                    .clone()
                    .map(|t| llvm_type_string(&t))
                    .unwrap_or(Ok("i64".to_string()))?;
                let alloca = format!("%v_{}.snap", instr.result.0);
                out.push_str(&format!("  {} = alloca {}\n", alloca, elt_ty));
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    elt_ty,
                    operand_str(source),
                    elt_ty,
                    alloca
                ));
                out.push_str(&format!(
                    "  %v_{} = bitcast {}* {} to {}\n",
                    instr.result.0, elt_ty, alloca, result_ty_str
                ));
            } else {
                // Already a pointer — just bitcast.
                let src_ty_str = llvm_type_string(src_ty.as_ref().unwrap())?;
                out.push_str(&format!(
                    "  %v_{} = bitcast {} {} to {}\n",
                    instr.result.0,
                    src_ty_str,
                    operand_str(source),
                    result_ty_str
                ));
            }
        }
        InstrKind::Hint(_) => {
            // Structural marker (parallel-for / task region
            // boundary). Today's SSA-LLVM treats these as
            // no-ops so the body lowers sequentially —
            // semantics-preserving because the verifier
            // already proved race-freedom.
        }
    }
    Ok(())
}

fn emit_binary(
    op: BinaryOp,
    l: &Operand,
    r: &Operand,
    op_ty: &Type,
    result_ty: &Type,
    result: ValueId,
    out: &mut String,
) -> Result<(), EmitError> {
    // Comparisons return i1; the operand type determines the
    // op flavor (icmp / fcmp) and the signedness for icmp.
    let signed = matches!(op_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64);
    let is_float = matches!(op_ty, Type::F32 | Type::F64);
    let ty_str = llvm_type(op_ty)?;
    let l_s = operand_str(l);
    let r_s = operand_str(r);
    let opcode = match op {
        BinaryOp::Add => if is_float { "fadd" } else { "add" },
        BinaryOp::Sub => if is_float { "fsub" } else { "sub" },
        BinaryOp::Mul => if is_float { "fmul" } else { "mul" },
        BinaryOp::Div => {
            if is_float { "fdiv" } else if signed { "sdiv" } else { "udiv" }
        }
        BinaryOp::Rem => {
            if is_float { "frem" } else if signed { "srem" } else { "urem" }
        }
        BinaryOp::Shl => "shl",
        BinaryOp::Shr => if signed { "ashr" } else { "lshr" },
        BinaryOp::BitAnd | BinaryOp::And => "and",
        BinaryOp::BitOr | BinaryOp::Or => "or",
        BinaryOp::BitXor => "xor",
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            // Comparison.
            let (pred, prefix) = if is_float {
                let p = match op {
                    BinaryOp::Eq => "oeq",
                    BinaryOp::Ne => "one",
                    BinaryOp::Lt => "olt",
                    BinaryOp::Le => "ole",
                    BinaryOp::Gt => "ogt",
                    BinaryOp::Ge => "oge",
                    _ => unreachable!(),
                };
                (p, "fcmp")
            } else if signed {
                let p = match op {
                    BinaryOp::Eq => "eq",
                    BinaryOp::Ne => "ne",
                    BinaryOp::Lt => "slt",
                    BinaryOp::Le => "sle",
                    BinaryOp::Gt => "sgt",
                    BinaryOp::Ge => "sge",
                    _ => unreachable!(),
                };
                (p, "icmp")
            } else {
                let p = match op {
                    BinaryOp::Eq => "eq",
                    BinaryOp::Ne => "ne",
                    BinaryOp::Lt => "ult",
                    BinaryOp::Le => "ule",
                    BinaryOp::Gt => "ugt",
                    BinaryOp::Ge => "uge",
                    _ => unreachable!(),
                };
                (p, "icmp")
            };
            out.push_str(&format!(
                "  %v_{} = {} {} {} {}, {}\n",
                result.0, prefix, pred, ty_str, l_s, r_s
            ));
            let _ = result_ty;
            return Ok(());
        }
    };
    out.push_str(&format!(
        "  %v_{} = {} {} {}, {}\n",
        result.0, opcode, ty_str, l_s, r_s
    ));
    Ok(())
}

fn emit_cast(
    x: &Operand,
    from_ty: &Type,
    to_ty: &Type,
    result: ValueId,
    out: &mut String,
) -> Result<(), EmitError> {
    // Two source-language types may share the same LLVM
    // backing (e.g. `i64` and `u64` both lower to `i64`).
    // Identity at the LLVM level — even when `from_ty !=
    // to_ty` at the source level — needs an identity op,
    // not a zext/sext/trunc (LLVM rejects `zext i64 … to
    // i64`).
    let from_llvm = llvm_type(from_ty)?;
    let to_llvm = llvm_type(to_ty)?;
    if from_llvm == to_llvm {
        // Closure #263: when both sides lower to the SAME LLVM
        // type, we need an identity op. The previous form
        // `add T 0, x` works for integers and `fadd double 0.0,
        // x` for floats — but for pointer-typed identity
        // (e.g. `OwnedStr → Str`, both `i8*`) LLVM rejects
        // `add i8* 0, x` with "integer constant must have
        // integer type". Use `bitcast T x to T` (a no-op
        // bitcast to the same type) for pointers. Same shape
        // tree-LLVM uses elsewhere for ptr-typed identity.
        let is_ptr = matches!(
            to_ty,
            Type::Str | Type::OwnedStr | Type::Vec(_) | Type::Ref(_) | Type::RefMut(_)
        );
        if is_ptr {
            out.push_str(&format!(
                "  %v_{} = bitcast {} {} to {}\n",
                result.0,
                to_llvm,
                operand_str(x),
                to_llvm
            ));
            return Ok(());
        }
        let (op, zero) = match to_ty {
            Type::Bool => ("or", "false"),
            Type::F32 | Type::F64 => ("fadd", "0.0"),
            _ => ("add", "0"),
        };
        out.push_str(&format!(
            "  %v_{} = {} {} {}, {}\n",
            result.0,
            op,
            to_llvm,
            zero,
            operand_str(x)
        ));
        return Ok(());
    }
    let from_bits = type_bits(from_ty);
    let to_bits = type_bits(to_ty);
    let from_signed = matches!(from_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64);
    let to_signed = matches!(to_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64);
    let from_is_int = from_ty.is_integer() || matches!(from_ty, Type::Bool);
    let to_is_int = to_ty.is_integer() || matches!(to_ty, Type::Bool);
    let from_is_float = from_ty.is_float();
    let to_is_float = to_ty.is_float();
    let op = match (from_is_int, to_is_int, from_is_float, to_is_float) {
        (true, true, _, _) => {
            // int-to-int: trunc / zext / sext.
            if to_bits < from_bits {
                "trunc"
            } else if from_signed {
                "sext"
            } else {
                "zext"
            }
        }
        (true, _, _, true) => {
            // int-to-float: sitofp / uitofp.
            if from_signed { "sitofp" } else { "uitofp" }
        }
        (_, true, true, _) => {
            // float-to-int.
            if to_signed { "fptosi" } else { "fptoui" }
        }
        (_, _, true, true) => {
            if to_bits < from_bits { "fptrunc" } else { "fpext" }
        }
        _ => {
            return Err(EmitError {
                message: format!(
                    "unsupported SSA-LLVM cast: {:?} -> {:?}",
                    from_ty, to_ty
                ),
            })
        }
    };
    out.push_str(&format!(
        "  %v_{} = {} {} {} to {}\n",
        result.0,
        op,
        llvm_type(from_ty)?,
        operand_str(x),
        llvm_type(to_ty)?
    ));
    Ok(())
}

fn emit_terminator(
    term: &Terminator,
    return_type: &Type,
    value_types: &BTreeMap<ValueId, Type>,
    out: &mut String,
) -> Result<(), EmitError> {
    match term {
        Terminator::Return(None) => out.push_str("  ret void\n"),
        Terminator::Return(Some(op)) => {
            // Prefer the function's declared return type. For
            // Const operands the operand's type is unknown, but
            // the signature is authoritative anyway.
            let ty = match operand_type(op, value_types) {
                Some(t) => t,
                None => return_type.clone(),
            };
            let _ = ty;
            let ret_ty_str = llvm_type_string(return_type)?;
            out.push_str(&format!("  ret {} {}\n", ret_ty_str, operand_str(op)));
        }
        Terminator::Jump { target, .. } => {
            out.push_str(&format!("  br label %bb{}\n", target.0));
        }
        Terminator::Branch {
            cond,
            then_block,
            else_block,
            ..
        } => {
            out.push_str(&format!(
                "  br i1 {}, label %bb{}, label %bb{}\n",
                operand_str(cond),
                then_block.0,
                else_block.0
            ));
        }
        Terminator::Unreachable => out.push_str("  unreachable\n"),
    }
    Ok(())
}

fn operand_str(op: &Operand) -> String {
    match op {
        Operand::Value(v) => format!("%v_{}", v.0),
        Operand::Const(c) => const_str(c),
    }
}

fn const_str(c: &Const) -> String {
    match c {
        Const::Int(v) => format!("{}", v),
        Const::Bool(true) => "1".to_string(),
        Const::Bool(false) => "0".to_string(),
        Const::Float(v) => {
            // LLVM IR requires float literals to look like a
            // floating-point number (have a `.` or exponent);
            // `{}` on a whole number prints `7` which LLVM
            // rejects as an integer constant in a float
            // context.
            let s = format!("{}", v);
            if s.contains('.') || s.contains('e') || s.contains('E') || s == "inf" || s == "-inf" || s == "NaN" {
                s
            } else {
                format!("{}.0", s)
            }
        }
    }
}

fn operand_type(op: &Operand, value_types: &BTreeMap<ValueId, Type>) -> Option<Type> {
    match op {
        Operand::Value(v) => value_types.get(v).cloned(),
        Operand::Const(_) => None,
    }
}

/// If `op` has type `Ref(Vec(T))` / `RefMut(Vec(T))`, the LLVM-
/// level value is a `%intent_vec_T*` (pointer to the struct).
/// Code that consumes the Vec aggregate via `extractvalue`
/// needs the aggregate value, not the pointer; load it through
/// the pointer and return the new SSA name.
///
/// If `op` already carries the aggregate type (`Vec(T)`), return
/// `operand_str(op)` unchanged.
fn vec_aggregate_operand(
    op: &Operand,
    value_types: &BTreeMap<ValueId, Type>,
    struct_ty: &str,
    suffix: &str,
    result: ValueId,
    out: &mut String,
) -> String {
    let ty = operand_type(op, value_types);
    let was_ref = matches!(ty, Some(Type::Ref(_)) | Some(Type::RefMut(_)));
    if was_ref {
        let loaded = format!("%v_{}.{}", result.0, suffix);
        out.push_str(&format!(
            "  {} = load {}, {}* {}\n",
            loaded,
            struct_ty,
            struct_ty,
            operand_str(op)
        ));
        loaded
    } else {
        operand_str(op)
    }
}

/// Array and Ref(Array) share the same `[N x T]*` LLVM
/// representation in SSA-LLVM (see `llvm_type_string`), so an
/// array operand is already a valid GEP base regardless of
/// whether it was Ref-wrapped at the source level. Kept as a
/// thin pass-through helper to mirror `vec_aggregate_operand`
/// and document the invariant.
fn array_pointer_operand(
    op: &Operand,
    _value_types: &BTreeMap<ValueId, Type>,
    _array_ty_str: &str,
    _suffix: &str,
    _result: ValueId,
    _out: &mut String,
) -> String {
    operand_str(op)
}

fn emit_vec_call(
    name: &str,
    element: &Type,
    args: &[Operand],
    result: ValueId,
    value_types: &BTreeMap<ValueId, Type>,
    out: &mut String,
) -> Result<(), EmitError> {
    let struct_ty = crate::backend_llvm::vec_struct_name(element);
    // In-buffer value spelling: arrays as `[N x T]`, not
    // `[N x T]*` (the SSA-value form). Phase 2c.
    let elt_ty = crate::backend_llvm::vec_element_value_str(element);
    let elt_tag = crate::backend_llvm::vec_struct_tag(element);
    match name {
        "vec" => {
            // Inline malloc + per-element store + struct
            // build. Mirrors the tree-LLVM
            // `emit_vec_let_from_literal` shape.
            let n = args.len() as i64;
            let elt_bits =
                crate::backend_llvm::vec_element_byte_size(element) as i64;
            let bytes = (n * elt_bits).max(elt_bits.max(1));
            let raw = format!("%v_{}.raw", result.0);
            out.push_str(&format!(
                "  {} = call i8* @malloc(i64 {})\n",
                raw, bytes
            ));
            let buf = format!("%v_{}.buf", result.0);
            out.push_str(&format!(
                "  {} = bitcast i8* {} to {}*\n",
                buf, raw, elt_ty
            ));
            // Array elements: the SSA operand is an alloca
            // pointer (`[N x T]*`). Load its value before
            // storing into the buffer slot so we move the
            // array by value, not the pointer. Phase 2c.
            let element_is_array = matches!(element, Type::Array { .. });
            for (i, a) in args.iter().enumerate() {
                let p = format!("%v_{}.s{}", result.0, i);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 {}\n",
                    p, elt_ty, elt_ty, buf, i
                ));
                let value_str = if element_is_array {
                    let v = format!("%v_{}.lv{}", result.0, i);
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        v, elt_ty, elt_ty, operand_str(a)
                    ));
                    v
                } else {
                    operand_str(a).to_string()
                };
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    elt_ty, value_str, elt_ty, p
                ));
            }
            let cap = if n == 0 { 1 } else { n };
            let iv0 = format!("%v_{}.iv0", result.0);
            out.push_str(&format!(
                "  {} = insertvalue {} undef, {}* {}, 0\n",
                iv0, struct_ty, elt_ty, buf
            ));
            let iv1 = format!("%v_{}.iv1", result.0);
            out.push_str(&format!(
                "  {} = insertvalue {} {}, i64 {}, 1\n",
                iv1, struct_ty, iv0, n
            ));
            out.push_str(&format!(
                "  %v_{} = insertvalue {} {}, i64 {}, 2\n",
                result.0, struct_ty, iv1, cap
            ));
        }
        op => {
            // push / set / clone — call the shared helpers
            // emitted in the module preamble. Each helper
            // has a known signature; fall back to that when
            // an operand is a Const (operand_type returns
            // None). Closure #158: previously fell back to
            // `element` for every Const, which typed the
            // `i` index of `set(xs, i, v)` as the element
            // type (i8* for Vec<OwnedStr>) — a real type
            // mismatch the lli verifier warned about and
            // tolerated. The signatures by name:
            //   - push(Vec<T>, T)         → (struct, elt)
            //   - set(Vec<T>, i64, T)     → (struct, i64, elt)
            //   - clone(Vec<T>)           → (struct)
            //   - push_mut(*Vec<T>, T)    → (struct*, elt)
            let sig_at = |pos: usize| -> Type {
                match op {
                    "push" => {
                        if pos == 0 { Type::Vec(Box::new(element.clone())) }
                        else { element.clone() }
                    }
                    "set" => {
                        if pos == 0 { Type::Vec(Box::new(element.clone())) }
                        else if pos == 1 { Type::I64 }
                        else { element.clone() }
                    }
                    "clone" => Type::Vec(Box::new(element.clone())),
                    _ => element.clone(),
                }
            };
            let arg_pairs: Result<Vec<String>, EmitError> = args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let aty = operand_type(a, value_types)
                        .unwrap_or_else(|| sig_at(i));
                    Ok(format!("{} {}", llvm_type_string(&aty)?, operand_str(a)))
                })
                .collect();
            out.push_str(&format!(
                "  %v_{} = call {} @intent_vec_{}__{}({})\n",
                result.0,
                struct_ty,
                elt_tag,
                op,
                arg_pairs?.join(", ")
            ));
        }
    }
    Ok(())
}

fn collect_channel_specs_in_ty_llvm(
    ty: &Type,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<(Type, u64)>,
) {
    match ty {
        Type::Channel(element, capacity) => {
            let key = crate::backend_llvm::llvm_channel_struct(element, *capacity);
            if seen.insert(key) {
                out.push(((**element).clone(), *capacity));
            }
            collect_channel_specs_in_ty_llvm(element, seen, out);
        }
        Type::Array { element, .. } => collect_channel_specs_in_ty_llvm(element, seen, out),
        Type::Vec(element) => collect_channel_specs_in_ty_llvm(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => {
            collect_channel_specs_in_ty_llvm(inner, seen, out)
        }
        _ => {}
    }
}

fn collect_vec_elt(
    ty: &Type,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    match ty {
        Type::Vec(element) => {
            let key = format!("{}", element);
            if seen.insert(key) {
                out.push((**element).clone());
            }
            collect_vec_elt(element, seen, out);
        }
        Type::Array { element, .. } => collect_vec_elt(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => collect_vec_elt(inner, seen, out),
        _ => {}
    }
}

/// Escape a Rust string for inclusion in an LLVM IR
/// `c"…"` literal. LLVM uses `\HH` for non-printable /
/// non-ASCII bytes. Backslash and quote get escaped too.
fn escape_llvm_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'\\' => out.push_str("\\5C"),
            b'"' => out.push_str("\\22"),
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("\\{:02X}", other)),
        }
    }
    out
}

/// Recover the `[N x T]` "pointee" type for an array
/// operand. Arrays are stored as `[N x T]*` (alloca pointer),
/// so the GEP needs the un-pointer type as the first
/// argument. The lookup goes through `value_types` and
/// strips the outer pointer.
fn array_operand_pointee(
    op: &Operand,
    value_types: &BTreeMap<ValueId, Type>,
) -> Result<String, EmitError> {
    let ty = operand_type(op, value_types).ok_or_else(|| EmitError {
        message: format!("array operand has unknown type: {:?}", op),
    })?;
    match ty.deref() {
        Type::Array { element, length } => Ok(format!(
            "[{} x {}]",
            length,
            llvm_type(element)?
        )),
        other => Err(EmitError {
            message: format!(
                "array operand must be Array (or ref-to-Array), got {:?}",
                other
            ),
        }),
    }
}

/// LLVM GEP requires each index to be paired with its type
/// (e.g. `i64 %v_3`). Const operands default to i64 since
/// the language's default integer width is i64.
fn index_typed(
    op: &Operand,
    value_types: &BTreeMap<ValueId, Type>,
) -> Result<String, EmitError> {
    let ty = operand_type(op, value_types).unwrap_or(Type::I64);
    Ok(format!("{} {}", llvm_type(&ty)?, operand_str(op)))
}

fn llvm_type(ty: &Type) -> Result<&'static str, EmitError> {
    Ok(match ty {
        Type::I8 | Type::U8 => "i8",
        Type::I16 | Type::U16 => "i16",
        Type::I32 | Type::U32 => "i32",
        Type::I64 | Type::U64 => "i64",
        Type::Bool => "i1",
        Type::F32 => "float",
        Type::F64 => "double",
        Type::Str | Type::OwnedStr => "i8*",
        // `Atomic<T>` is an addressable cell — its LLVM
        // storage is `atomic_storage_llvm(T)` (i8/i16/i32/i64,
        // with bool widened to i8). Shared with tree-LLVM
        // via the now-`pub(crate)` helper.
        Type::Atomic(element) => {
            crate::backend_llvm::atomic_storage_llvm(element)
        }
        // `Mutex<i64>` / `Guard<i64>` — i64-only in v1.
        // Both are aggregates at the LLVM level. Like
        // Atomic the SSA value binds to the alloca pointer,
        // so the `pointer-like` Ref handling in
        // `llvm_type_string` keeps refs at one `*` level.
        Type::Mutex(_) => "%intent_mutex_i64",
        Type::Guard(_) => "%intent_guard_i64",
        other => {
            return Err(EmitError {
                message: format!(
                    "type {:?} not yet handled in SSA-LLVM scalar emit (try llvm_type_string)",
                    other
                ),
            })
        }
    })
}

/// Owned LLVM-type spelling that handles aggregates the
/// scalar `llvm_type` can't return as `&'static str`. Arrays
/// lower as `[N x T]*` (the SSA value is the alloca pointer
/// — `ArrayLit` emits the alloca + per-element store and
/// hands the pointer back as the SSA result). Refs become
/// `T*`. Fn pointers spell out the full LLVM function-pointer
/// type.
fn llvm_type_string(ty: &Type) -> Result<String, EmitError> {
    Ok(match ty {
        Type::Array { element, length } => {
            format!("[{} x {}]*", length, llvm_type(element)?)
        }
        Type::Vec(element) => crate::backend_llvm::vec_struct_name(element),
        // `Atomic<T>` SSA values are alloca-pointers, so the
        // LLVM type is the storage pointer (`<storage>*`).
        // `&Atomic<T>` / `&mut Atomic<T>` produce the same
        // pointer at the LLVM level — see the Ref arm.
        Type::Atomic(element) => {
            format!("{}*", crate::backend_llvm::atomic_storage_llvm(element))
        }
        // `Mutex<i64>` / `Guard<i64>` SSA values likewise bind
        // to an alloca pointer (so refs share the address).
        // i64-only in v1.
        Type::Mutex(_) => "%intent_mutex_i64*".to_string(),
        Type::Guard(_) => "%intent_guard_i64*".to_string(),
        // `Channel<T, N>` SSA values bind to alloca pointer
        // of the per-(T, N) struct; struct typedef is emitted
        // in the module preamble.
        Type::Channel(element, capacity) => {
            format!("{}*", crate::backend_llvm::llvm_channel_struct(element, *capacity))
        }
        Type::Ref(inner) | Type::RefMut(inner) => match inner.as_ref() {
            // SSA-LLVM represents Array values as `[N x T]*`
            // (the alloca pointer). A reference to an array is
            // the same pointer at the LLVM level — adding a
            // second `*` would mismatch the body's GEP, which
            // expects `[N x T]*`.
            Type::Array { .. } => llvm_type_string(inner)?,
            // Same story for `Atomic<T>` / `Mutex<T>` /
            // `Guard<T>`: the SSA value is already a
            // `<storage>*` (the alloca), so a reference
            // shares the pointer.
            Type::Atomic(_) | Type::Mutex(_) | Type::Guard(_) | Type::Channel(_, _) => {
                llvm_type_string(inner)?
            }
            _ => format!("{}*", llvm_type_string(inner)?),
        },
        Type::FnPtr(params, ret) => {
            let param_strs: Result<Vec<String>, EmitError> =
                params.iter().map(llvm_type_string).collect();
            format!(
                "{} ({})*",
                llvm_type_string(ret)?,
                param_strs?.join(", ")
            )
        }
        _ => llvm_type(ty)?.to_string(),
    })
}

fn type_bits(ty: &Type) -> u32 {
    match ty {
        Type::I8 | Type::U8 | Type::Bool => 8,
        Type::I16 | Type::U16 => 16,
        Type::I32 | Type::U32 | Type::F32 => 32,
        Type::I64 | Type::U64 | Type::F64 => 64,
        _ => 64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile;
    use crate::ssa::lower_program;

    #[test]
    fn emits_minimal_fn_main_returning_literal() {
        let src = "fn main() -> i64 { return 42; }";
        let checked = compile(src).expect("compiles");
        let (module, errs) = lower_program(&checked.ir);
        assert!(errs.is_empty(), "lower errors: {:?}", errs);
        let ll = emit(&module).expect("emit succeeds");
        assert!(ll.contains("define i64 @fn_main()"));
        // The return materializes via add 0, 42, then ret.
        assert!(
            ll.contains("ret i64 ") || ll.contains("ret void"),
            "expected ret in IR:\n{}",
            ll
        );
    }

    #[test]
    fn emits_arithmetic_program() {
        let src = "fn main() -> i64 { let x: i64 = 41; return x + 1; }";
        let checked = compile(src).expect("compiles");
        let (module, errs) = lower_program(&checked.ir);
        assert!(errs.is_empty());
        let ll = emit(&module).expect("emit succeeds");
        assert!(ll.contains("add i64"), "expected add i64:\n{}", ll);
    }

    #[test]
    fn emits_comparison_and_branch() {
        let src = "fn main() -> i64 { if 1 < 2 { return 1; } else { return 0; } }";
        let checked = compile(src).expect("compiles");
        let (module, errs) = lower_program(&checked.ir);
        assert!(errs.is_empty());
        let ll = emit(&module).expect("emit succeeds");
        assert!(
            ll.contains("icmp slt i64") || ll.contains("br i1"),
            "expected compare + branch:\n{}",
            ll
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parallel_for_uses_create_thread_fanout_on_windows() {
        // SSA-LLVM mirror of the tree-LLVM Win32 parallel-for
        // test: libgomp isn't available on Windows, so the
        // call site fans out via `@CreateThread` (N=4) and the
        // outlined fn reads tid/nt from a WinParArg struct
        // instead of via `omp_get_*`. Returns `i8*` to match
        // the CreateThread start-routine ABI.
        let src = r#"
            fn square(x: i64) -> i64 { return x * x; }
            fn main() -> i64 {
              parallel for i from 0 to 8 { let _ = square(i); }
              return 0;
            }
        "#;
        let checked = compile(src).expect("parallel-for compiles");
        let (module, errs) = lower_program(&checked.ir);
        assert!(errs.is_empty(), "lower errors: {:?}", errs);
        let ll = emit(&module).expect("emit succeeds");
        assert!(
            !ll.contains("@GOMP_parallel") && !ll.contains("@omp_get_thread_num"),
            "expected GOMP/omp_get_* to be absent on Windows:\n{ll}"
        );
        assert!(
            ll.contains("define internal i8* @__intent_par_"),
            "expected outlined fn to use i8* (i8*) ABI:\n{ll}"
        );
        assert!(
            ll.contains("alloca [4 x { i8*, i64, i64 }]"),
            "expected per-thread WinParArg array (N=4):\n{ll}"
        );
        let create_calls = ll
            .matches("call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @__intent_par_")
            .count();
        assert_eq!(
            create_calls, 3,
            "expected 3 CreateThread calls (N-1 workers):\n{ll}"
        );
    }
}
