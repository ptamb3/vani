//! SSA-consuming C backend (milestone 6g, proof-of-concept).
//!
//! Emits C source from an `ssa::Module` covering: integers
//! (`i8..i64`, `u8..u64`), `bool`, `f32`/`f64`, comparisons,
//! arithmetic, shifts, bitwise ops, casts, function calls,
//! if/while, return, fn-pointer indirect calls, fixed-size
//! arrays (`ArrayLit` / `Index` / `IndexAssign` / `Len`),
//! `Vec<T>` (creation + push/set/clone + index + len + drop)
//! via shared runtime helpers, `RefOf`, string literals, and
//! `Drop` (with Vec-buffer free). The parallel-for and task
//! `Hint` markers pass through as no-ops so the body executes
//! sequentially — semantics-preserving because the verifier
//! already proved race-freedom; real pthread / libgomp
//! parallelism via SSA-C is tracked as a follow-up.
//!
//! Strategy:
//!   - Each SSA `Function` becomes a C function.
//!   - Block parameters become local C variables at the top of
//!     the function (one per (block, param) pair).
//!   - Each `BasicBlock` becomes a labeled section. Predecessors
//!     assign block-arg values before issuing `goto bbN`.
//!   - SSA values map to C identifiers `v_<id>`.
//!   - Terminators expand to braced blocks that copy args then
//!     branch.

use std::fmt::Write;

use crate::ast::{BinaryOp, ReductionOp, Type, UnaryOp};
use crate::ssa::{
    BlockId, Const, Function, HintKind, InstrKind, Module, Operand,
    ParallelForShape, Terminator, ValueId,
};

thread_local! {
    /// Module-scope buffer for outlined task functions
    /// (each `task <name> { … }` becomes a `static void*
    /// intent_task_<N>(void*)` definition). Spliced into the
    /// module output between the runtime helpers and the
    /// main functions so spawn-site calls resolve.
    static TASK_OUTLINES: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    /// Per-emit counter for outlined-fn names. Reset on
    /// every `emit` call so naming is stable.
    static TASK_OUTLINE_COUNTER: std::cell::Cell<u32> = std::cell::Cell::new(0);
}

/// Top-level entry point. Returns the generated C source for the
/// entire module, including a small preamble of forward
/// declarations and a `main()` shim that calls `fn_main` and
/// returns its exit code.
pub fn emit(module: &Module) -> Result<String, EmitError> {
    TASK_OUTLINES.with(|b| b.borrow_mut().clear());
    TASK_OUTLINE_COUNTER.with(|c| c.set(0));
    let mut out = String::new();
    preamble(&mut out);
    // `intent_task_handle` carries the pthread/Win32 thread
    // handle (via the cross-platform `intent_thread_t`
    // typedef in the preamble) alongside the heap ctx
    // pointer. Emitted unconditionally — small overhead and
    // the task lowering refers to it from the SSA-C task
    // outline machinery.
    out.push_str(
        "typedef struct { intent_thread_t thread; void* ctx; } intent_task_handle;\n\n",
    );
    // Walk every block's instructions for Vec result types,
    // collect unique element types, and emit one
    // `intent_vec_<T>` runtime bundle (struct + helpers) per
    // element. Reuses the tree-C backend's emit_vec_bundle
    // so the runtime stays in lock-step.
    let mut seen: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut vec_elements: Vec<Type> = Vec::new();
    for f in &module.functions {
        for (_, ty, _) in &f.params {
            collect_vec_types(ty, &mut seen, &mut vec_elements);
        }
        collect_vec_types(&f.return_type, &mut seen, &mut vec_elements);
        for block in &f.blocks {
            for (_, ty) in &block.params {
                collect_vec_types(ty, &mut seen, &mut vec_elements);
            }
            for instr in &block.instructions {
                collect_vec_types(&instr.ty, &mut seen, &mut vec_elements);
            }
        }
    }
    for element in &vec_elements {
        crate::backend_c::emit_vec_bundle(element, &mut out);
    }
    // Walk the module for `Type::Channel(T, N)` specs and
    // emit one per-(T, N) runtime bundle (struct + new /
    // send / recv helpers) per unique pair. Reuses
    // tree-C's `emit_channel_bundle_pub` so the runtime
    // stays in lock-step.
    let mut chan_seen: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut chan_specs: Vec<(Type, u64)> = Vec::new();
    for f in &module.functions {
        for (_, ty, _) in &f.params {
            collect_channel_specs_in_ty(ty, &mut chan_seen, &mut chan_specs);
        }
        collect_channel_specs_in_ty(&f.return_type, &mut chan_seen, &mut chan_specs);
        for block in &f.blocks {
            for (_, ty) in &block.params {
                collect_channel_specs_in_ty(ty, &mut chan_seen, &mut chan_specs);
            }
            for instr in &block.instructions {
                collect_channel_specs_in_ty(&instr.ty, &mut chan_seen, &mut chan_specs);
            }
        }
    }
    for (element, capacity) in &chan_specs {
        crate::backend_c::emit_channel_bundle_pub(element, *capacity, &mut out);
    }
    // Emit forward declarations of every user function so
    // task outlines (which may call them) can see the
    // prototypes — task outlines get spliced between the
    // prototypes and the function bodies.
    for f in &module.functions {
        emit_function_prototype(f, &mut out)?;
    }
    out.push('\n');
    // Emit function bodies to a side buffer first so that
    // task outlines (which write to `TASK_OUTLINES` as a
    // side effect during the body walk) can be spliced
    // between the prototypes and the bodies.
    let mut functions = String::new();
    for f in &module.functions {
        emit_function(f, &mut functions)?;
    }
    TASK_OUTLINES.with(|b| {
        let s = std::mem::take(&mut *b.borrow_mut());
        if !s.is_empty() {
            out.push_str(&s);
        }
    });
    out.push_str(&functions);
    if module.functions.iter().any(|f| f.name == "main") {
        out.push_str("\nint main(void) { return (int)fn_main(); }\n");
    }
    Ok(out)
}

/// Walk a type for `Type::Channel(T, N)` specs, dedup by
/// the struct name, accumulate into `out`. Mirrors
/// `backend_c::collect_channel_specs` (the tree-C side
/// walks TypedExpr/TypedStmt; the SSA-C side walks the
/// already-typed SSA `Type`s).
fn collect_channel_specs_in_ty(
    ty: &Type,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<(Type, u64)>,
) {
    match ty {
        Type::Channel(element, capacity) => {
            let key = crate::backend_c::c_channel_storage(element, *capacity);
            if seen.insert(key) {
                out.push(((**element).clone(), *capacity));
            }
            collect_channel_specs_in_ty(element, seen, out);
        }
        Type::Array { element, .. } => collect_channel_specs_in_ty(element, seen, out),
        Type::Vec(element) => collect_channel_specs_in_ty(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => {
            collect_channel_specs_in_ty(inner, seen, out)
        }
        _ => {}
    }
}

fn collect_vec_types(
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
            collect_vec_types(element, seen, out);
        }
        Type::Array { element, .. } => collect_vec_types(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => collect_vec_types(inner, seen, out),
        _ => {}
    }
}

#[derive(Debug, Clone)]
pub struct EmitError {
    pub message: String,
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ssa-c emit: {}", self.message)
    }
}

fn preamble(out: &mut String) {
    out.push_str("#include <assert.h>\n");
    out.push_str("#include <stdatomic.h>\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include <stdbool.h>\n");
    out.push_str("#include <stdio.h>\n");
    out.push_str("#include <stdlib.h>\n");
    out.push_str("#include <string.h>\n\n");
    // Match the tree-C preamble's INTENT_UNUSED macro so Vec
    // runtime helpers (emitted via the shared
    // `emit_vec_bundle`) compile cleanly. The helpers tag
    // every function with this attribute so unused ones don't
    // raise `-Wunused-function`.
    out.push_str("#if defined(__GNUC__) || defined(__clang__)\n");
    out.push_str("#define INTENT_UNUSED __attribute__((unused))\n");
    out.push_str("#else\n");
    out.push_str("#define INTENT_UNUSED\n");
    out.push_str("#endif\n\n");

    // Cross-platform threading wrappers (`intent_thread_t`
    // typedef + `intent_thread_create`/`intent_thread_join`/
    // `intent_thread_yield`). Required by the task outline
    // machinery, mutex backoff, and the
    // `intent_task_handle` typedef. Shared with tree-C so
    // both backends emit the same wrapper definitions.
    crate::backend_c::emit_intent_thread_wrappers_c(out);
    // Shared `intent_str_concat` runtime helper used by Str
    // `+` lowering. Always emitted; small and may be unused.
    crate::backend_c::emit_intent_str_concat_c(out);
    // Shared Mutex/Guard runtime helpers (i64-only for v1).
    // Always emitted; the i64 mutex bundle is small and
    // INTENT_UNUSED-tagged so unused helpers don't warn.
    // Same runtime as tree-C uses.
    crate::backend_c::emit_intent_mutex_helpers_c(out);
}

fn emit_function_prototype(f: &Function, out: &mut String) -> Result<(), EmitError> {
    let ret_c = c_type(&f.return_type)?;
    write!(out, "{} fn_{}(", ret_c, f.name).unwrap();
    // Closure #202: empty-param prototypes must say `(void)`,
    // not `()`. Empty parens mean "unspecified prototype" in
    // C (not "no args"), tripping `-Wstrict-prototypes` and
    // breaking -Werror builds. Tree-C already emits `(void)`
    // via `emit_params`; mirror that here.
    if f.params.is_empty() {
        out.push_str("void");
    }
    for (i, (_, ty, vid)) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let p_decl = c_declarator(ty, &format!("v_{}", vid.0))?;
        write!(out, "{}", p_decl).unwrap();
    }
    out.push_str(");\n");
    Ok(())
}

fn emit_function(f: &Function, out: &mut String) -> Result<(), EmitError> {
    let ret_c = c_type(&f.return_type)?;
    write!(out, "{} fn_{}(", ret_c, f.name).unwrap();
    // Closure #202: see `emit_function_prototype` for why
    // empty parens must be `(void)`.
    if f.params.is_empty() {
        out.push_str("void");
    }
    for (i, (name, ty, vid)) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let p_decl = c_declarator(ty, &format!("v_{}", vid.0))?;
        write!(out, "{} /* {} */", p_decl, name).unwrap();
    }
    out.push_str(") {\n");

    // Track each SSA value's source type so instruction
    // emit can dispatch on shape (e.g., Vec vs Array for
    // Index). Built during the forward-declaration walk.
    let mut value_types: std::collections::BTreeMap<ValueId, Type> =
        std::collections::BTreeMap::new();
    for (_, ty, vid) in &f.params {
        value_types.insert(*vid, ty.clone());
    }

    // Forward-declare every SSA value (instruction results and
    // block params) so blocks can refer to them out of order
    // via gotos. Skip params — they're already declared in the
    // C signature.
    let mut declared: std::collections::BTreeSet<ValueId> = f
        .params
        .iter()
        .map(|(_, _, v)| *v)
        .collect();
    for block in &f.blocks {
        for (v, ty) in &block.params {
            value_types.insert(*v, ty.clone());
            if declared.insert(*v) {
                writeln!(out, "  {};", c_declarator(ty, &format!("v_{}", v.0))?)
                    .unwrap();
            }
        }
        for instr in &block.instructions {
            value_types.insert(instr.result, instr.ty.clone());
            if !is_void_kind(&instr.kind) && declared.insert(instr.result) {
                writeln!(
                    out,
                    "  {};",
                    c_declarator(&instr.ty, &format!("v_{}", instr.result.0))?
                )
                .unwrap();
            }
        }
    }

    // Emit a goto to the entry block (if it isn't the first
    // block, which it usually is) so the label sequencing stays
    // simple.
    if f.entry.0 != 0 {
        writeln!(out, "  goto bb{};", f.entry.0).unwrap();
    }

    // Pre-scan: find `ParallelForBegin` regions whose shape we
    // can lower as a structured `#pragma omp parallel for`.
    // The recognizer also returns a set of blocks to skip
    // during the normal block walk (header + body) — those
    // get inlined into the parallel-for emit at the
    // pre-header.
    let par_regions = collect_parallel_regions(f)?;
    let skip_blocks: std::collections::BTreeSet<BlockId> = par_regions
        .iter()
        .flat_map(|r| {
            // Closure #187 added the `step_block` between
            // body and header. Skip it alongside the others
            // — it's absorbed into the C for-loop's update
            // clause (i++), not emitted as a free-standing
            // basic block.
            //
            // Closure #251 (Step 3b emit half): also skip every
            // block in `region_blocks`. For single-block bodies
            // that set is just `[body_block]`; for multi-block
            // bodies (recognizer accepts them via #241) it
            // additionally contains the in-region if/then/else
            // / merge blocks. All of these get inlined into the
            // for-loop body during `emit_parallel_for_region`.
            let mut blocks = vec![
                r.shape.header_block,
                r.shape.body_block,
                r.shape.step_block,
            ];
            blocks.extend(r.region_blocks.iter().copied());
            blocks
        })
        .collect();
    let par_by_begin: std::collections::BTreeMap<BlockId, &ParallelRegion> =
        par_regions.iter().map(|r| (r.begin_block, r)).collect();

    // Pre-scan for `TaskBegin`/`TaskEnd` pairs. Single-block
    // bodies fit in one block-instruction skip-set; multi-
    // block bodies (with `if`/`while` inside the task body)
    // additionally need the intermediate + end blocks to be
    // skipped entirely during the parent walk.
    let task_regions = collect_task_regions_c(f)?;
    let task_by_begin: std::collections::BTreeMap<(BlockId, usize), &TaskRegionC> =
        task_regions
            .iter()
            .map(|r| ((r.begin_block, r.begin_idx), r))
            .collect();
    let task_join_by_loc: std::collections::BTreeMap<(BlockId, usize), &TaskRegionC> =
        task_regions
            .iter()
            .map(|r| ((r.join_block, r.join_idx), r))
            .collect();
    // Blocks fully absorbed into outlined task fns — emit
    // nothing for them in the parent.
    let task_fully_skipped: std::collections::BTreeSet<BlockId> = task_regions
        .iter()
        .flat_map(|r| {
            // Intermediate body blocks (not begin or end).
            r.body_blocks
                .iter()
                .filter(move |b| **b != r.begin_block && **b != r.end_block)
                .copied()
        })
        .collect();
    // For end_block of a multi-block task: skip everything
    // before (and including) the TaskEnd hint; emit
    // everything after. Track per-block "skip up to" index.
    let mut multi_end_skip_until: std::collections::BTreeMap<BlockId, usize> =
        std::collections::BTreeMap::new();
    for r in &task_regions {
        if r.begin_block != r.end_block {
            multi_end_skip_until.insert(r.end_block, r.end_idx);
        }
    }

    // Closure #204: collect the set of block IDs that are
    // actually referenced by some terminator (Jump / Branch).
    // Only those need a `bbN:` label emit; for blocks reached
    // only via fall-through (typically the entry block of a
    // straight-line fn), the label is unused and trips
    // `-Wunused-label`. The collected set also includes
    // explicit `goto` targets emitted by special regions
    // (parallel-for exit, task end_block, etc.).
    let mut referenced_blocks: std::collections::BTreeSet<BlockId> =
        std::collections::BTreeSet::new();
    if f.entry.0 != 0 {
        referenced_blocks.insert(f.entry);
    }
    for block in &f.blocks {
        match &block.terminator {
            Terminator::Jump { target, .. } => {
                referenced_blocks.insert(*target);
            }
            Terminator::Branch { then_block, else_block, .. } => {
                referenced_blocks.insert(*then_block);
                referenced_blocks.insert(*else_block);
            }
            _ => {}
        }
    }
    // Special-region emit paths add explicit `goto bbN;`
    // statements that we also need to count as references.
    // Note: task region's `begin_block` is referenced inside
    // the OUTLINED task fn (separate string buffer), not by
    // the parent fn emit, so we don't add it here. Same for
    // intermediate task body blocks (they're absorbed into
    // the outline). The `end_block` IS referenced by the
    // parent via the `goto bb<end>;` emitted after the task
    // join site for multi-block task bodies.
    for region in &par_regions {
        referenced_blocks.insert(region.shape.exit_block);
    }
    for region in &task_regions {
        if region.begin_block != region.end_block {
            referenced_blocks.insert(region.end_block);
        }
    }

    for block in &f.blocks {
        if skip_blocks.contains(&block.id) {
            // Header / body blocks are absorbed into the
            // pre-header's structured-loop emit. Emitting them
            // separately would be dead code (and the
            // back-edge terminator would produce an
            // out-of-order goto into the now-deleted loop).
            continue;
        }
        if task_fully_skipped.contains(&block.id) {
            // Intermediate task-body block — emitted into
            // the outlined task fn instead.
            continue;
        }
        if referenced_blocks.contains(&block.id) {
            writeln!(out, "bb{}:", block.id.0).unwrap();
        }
        if let Some(region) = par_by_begin.get(&block.id) {
            emit_parallel_for_region(f, block, region, &value_types, out)?;
            // After the for-loop, fall through to the exit
            // block. The pre-header's IR terminator is a
            // `Jump(header)`; the region emit overrides it
            // with `goto bb<exit>;` and any header→exit
            // block-arg assignments.
            continue;
        }
        // Build a skip-set for task-region body instructions.
        // Single-block tasks: skip [begin+1 .. end] in
        // begin_block. Multi-block tasks: begin_block skips
        // [begin+1 .. end-of-block]; end_block skips
        // [0 ..= end_idx] (the TaskEnd hint itself plus
        // everything before it). All other intermediate
        // blocks are already filtered via task_fully_skipped
        // above, so they never reach this loop.
        let mut task_skip: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        for region in &task_regions {
            if region.begin_block == block.id {
                let end_in_block = if region.begin_block == region.end_block {
                    region.end_idx
                } else {
                    // Multi-block: skip from begin+1 through
                    // end of this block (including the
                    // terminator instruction implicitly).
                    block.instructions.len().saturating_sub(1)
                };
                for i in region.begin_idx..=end_in_block {
                    if i != region.begin_idx {
                        task_skip.insert(i);
                    }
                }
            }
            if region.end_block == block.id && region.begin_block != region.end_block {
                for i in 0..=region.end_idx {
                    task_skip.insert(i);
                }
            }
        }
        // Block params don't need an instruction at the start
        // — predecessors store into `v_<id>` before jumping.
        let mut terminator_skipped = false;
        for (idx, instr) in block.instructions.iter().enumerate() {
            if task_skip.contains(&idx) {
                continue;
            }
            if let Some(region) = task_by_begin.get(&(block.id, idx)) {
                if emit_task_spawn_region_c(f, region, &value_types, out)? {
                    // Multi-block tasks override the parent's
                    // original terminator with their own
                    // `goto bb<end>` — skip the standard
                    // emit below.
                    terminator_skipped = true;
                }
                continue;
            }
            if let Some(region) = task_join_by_loc.get(&(block.id, idx)) {
                emit_task_join_c(region, out);
                continue;
            }
            emit_instr(instr, &value_types, out)?;
        }
        if !terminator_skipped {
            emit_terminator(&block.terminator, f, out)?;
        }
    }
    out.push_str("}\n\n");
    Ok(())
}

/// A parallel-for region recognized in pre-scan. Holds the
/// metadata an OpenMP `#pragma omp parallel for` emit needs:
/// the pre-header block (where the begin hint lives), the
/// loop's structured shape (counter / start / end /
/// header/body/exit blocks), the reduction clauses, and the
/// body's recognized "shape" (single-block body with only
/// the counter + declared reductions as carry state). Regions
/// the recognizer can't handle today surface a
/// `RegionRecognitionFailure` so `emit()` returns Err and the
/// caller (`emit_c_via_ssa` in `main.rs`) falls back to the
/// tree backend.
///
/// Shared with `ssa_backend_llvm` (Step 3 scaffolding): the
/// shape recognition is backend-independent; only the body
/// emit differs. `pub(crate)` so SSA-LLVM can reuse.
pub(crate) struct ParallelRegion {
    pub(crate) begin_block: BlockId,
    pub(crate) shape: ParallelForShape,
    pub(crate) reductions: Vec<(String, ReductionOp, Type)>,
    /// One entry per reduction binding: the header-block-
    /// param `ValueId` carrying the reduction's running value
    /// (matches the carry-edge args from the pre-header's
    /// Jump terminator at the same index). Skips the counter
    /// (header param 0).
    pub(crate) reduction_carries: Vec<ValueId>,
    /// Initial values for each reduction carry, read from the
    /// pre-header's Jump terminator. `start` lives separately
    /// on `shape`.
    pub(crate) reduction_inits: Vec<Operand>,
    /// SSA value-id for the counter-increment instruction in
    /// the body (e.g. `%i_next = %i + 1`). Skipped during
    /// body emit — the for-loop's `i++` handles it.
    pub(crate) counter_increment_value: ValueId,
    /// For each reduction (in `reductions` order), the SSA
    /// value-id of the in-body update result (e.g.
    /// `%total_next = %total + %i`). The body emit replaces
    /// the back-edge's "store back to header param" with a
    /// reduction-var assignment from this value.
    pub(crate) reduction_update_values: Vec<ValueId>,
    /// Closure #251 (SSA Step 3b emit half): every block that
    /// belongs to the body sub-CFG, in the order the recognizer
    /// discovered them (body_block first, then DFS successors).
    /// All of these get emitted inside the `for (…) { … }` body;
    /// the parent block walk in `emit_function` skips them.
    /// Excludes `step_block` (handled by the for-loop's `i++`).
    pub(crate) region_blocks: Vec<BlockId>,
    /// The unique block in `region_blocks` that jumps to
    /// `step_block` (the back-edge). Its instructions are emitted
    /// normally; its terminator is REPLACED by the reduction
    /// rebinds + an implicit fall-through to the `for`'s closing
    /// `}` (so the next iteration restarts at `body_block`).
    pub(crate) merge_block: BlockId,
}

pub(crate) fn collect_parallel_regions(
    f: &Function,
) -> Result<Vec<ParallelRegion>, EmitError> {
    let mut out = Vec::new();
    for block in &f.blocks {
        for instr in &block.instructions {
            if let InstrKind::Hint(HintKind::ParallelForBegin { reductions, shape }) =
                &instr.kind
            {
                let region =
                    recognize_parallel_region(f, block.id, shape, reductions)?;
                out.push(region);
            }
        }
    }
    Ok(out)
}

pub(crate) fn recognize_parallel_region(
    f: &Function,
    begin_block: BlockId,
    shape: &ParallelForShape,
    reductions: &[(String, ReductionOp, Type)],
) -> Result<ParallelRegion, EmitError> {
    // The pre-header block's terminator must be `Jump(header,
    // [start, reduction_inits...])`. The counter is always the
    // first header param; reductions follow in declaration
    // order.
    let pre_header = &f.blocks[begin_block.0 as usize];
    let (jump_args, jump_target) = match &pre_header.terminator {
        Terminator::Jump { target, args } => (args.clone(), *target),
        other => {
            return Err(EmitError {
                message: format!(
                    "parallel-for pre-header terminator must be Jump, got {:?}",
                    other
                ),
            });
        }
    };
    if jump_target != shape.header_block {
        return Err(EmitError {
            message: format!(
                "parallel-for pre-header jumps to {:?}, expected header {:?}",
                jump_target, shape.header_block
            ),
        });
    }
    let header = &f.blocks[shape.header_block.0 as usize];
    // Header carries counter + one block param per
    // reduction-tracked binding. Anything else is an
    // unrecognized non-reduction carry — fall back.
    if header.params.len() != 1 + reductions.len() {
        return Err(EmitError {
            message: format!(
                "parallel-for header has {} params, expected {} (counter + {} reductions)",
                header.params.len(),
                1 + reductions.len(),
                reductions.len()
            ),
        });
    }
    // Pre-header Jump must supply exactly those args.
    if jump_args.len() != header.params.len() {
        return Err(EmitError {
            message: format!(
                "parallel-for pre-header carries {} args, header expects {}",
                jump_args.len(),
                header.params.len()
            ),
        });
    }
    let reduction_carries: Vec<ValueId> =
        header.params.iter().skip(1).map(|(v, _)| *v).collect();
    let reduction_inits: Vec<Operand> = jump_args.iter().skip(1).cloned().collect();

    // Closure #241 (SSA Step 3b): walk the body sub-CFG to
    // collect every block reachable from `body_block` that
    // isn't `step_block` (the step is the "outside" of the
    // body region). v1 requires exactly one block in the
    // region to terminate by jumping to step_block — that
    // block carries the reduction-update args.
    //
    // Pre-#241 the recognizer only accepted single-block
    // bodies (`body.terminator == Jump(step)`), causing
    // parallel-for with internal if/while/etc. to fall back
    // to tree-LLVM. Multi-block bodies arise from the
    // common `if cond { reduce_update; }` guard pattern
    // inside a parallel-for body.
    let mut region_blocks: Vec<crate::ssa::BlockId> = Vec::new();
    let mut visited: std::collections::HashSet<crate::ssa::BlockId> =
        std::collections::HashSet::new();
    let mut stack: Vec<crate::ssa::BlockId> = vec![shape.body_block];
    while let Some(bid) = stack.pop() {
        if !visited.insert(bid) {
            continue;
        }
        if bid == shape.step_block {
            // step_block is the "exit" of the body region —
            // don't include it but don't error either.
            continue;
        }
        region_blocks.push(bid);
        let blk = &f.blocks[bid.0 as usize];
        let mut successors: Vec<crate::ssa::BlockId> = Vec::new();
        match &blk.terminator {
            Terminator::Jump { target, .. } => successors.push(*target),
            Terminator::Branch { then_block, else_block, .. } => {
                successors.push(*then_block);
                successors.push(*else_block);
            }
            Terminator::Return(_) | Terminator::Unreachable => {
                return Err(EmitError {
                    message: format!(
                        "parallel-for body block {:?} terminates with {:?} — \
                         control flow exits the loop in a way the recognizer \
                         doesn't model yet (would need to handle the early-return \
                         case separately)",
                        bid, blk.terminator
                    ),
                });
            }
        }
        for succ in successors {
            if succ == shape.body_block {
                return Err(EmitError {
                    message: "parallel-for body forms an inner cycle \
                              (body_block is reachable from itself); \
                              recognizer rejects nested loops in v1"
                        .to_string(),
                });
            }
            stack.push(succ);
        }
    }

    // Find the unique block in the region that jumps to
    // step_block. Multi-back-edge bodies (where two
    // different blocks both jump to step) need per-edge
    // reduction tracking which v1 doesn't model — reject.
    let mut merge_block_id: Option<crate::ssa::BlockId> = None;
    for bid in &region_blocks {
        let blk = &f.blocks[bid.0 as usize];
        let jumps_to_step = match &blk.terminator {
            Terminator::Jump { target, .. } => *target == shape.step_block,
            Terminator::Branch { then_block, else_block, .. } => {
                *then_block == shape.step_block || *else_block == shape.step_block
            }
            _ => false,
        };
        if jumps_to_step {
            if merge_block_id.is_some() {
                return Err(EmitError {
                    message: "parallel-for body has multiple back-edges to step \
                              (recognizer requires a single merge block in v1)"
                        .to_string(),
                });
            }
            merge_block_id = Some(*bid);
        }
    }
    let merge_block_id = merge_block_id.ok_or_else(|| EmitError {
        message: "parallel-for body region has no back-edge to step \
                  (one block must terminate by jumping to step)"
            .to_string(),
    })?;

    let body = &f.blocks[merge_block_id.0 as usize];
    let (back_args, back_target) = match &body.terminator {
        Terminator::Jump { target, args } => (args.clone(), *target),
        Terminator::Branch { .. } => {
            return Err(EmitError {
                message: "parallel-for back-edge from a CondBranch isn't \
                          recognized in v1 (the merge block's terminator \
                          must be Jump(step, [args]))".to_string(),
            });
        }
        other => {
            return Err(EmitError {
                message: format!(
                    "parallel-for back-edge block terminator must be Jump, got {:?}",
                    other
                ),
            });
        }
    };
    if back_target != shape.step_block {
        return Err(EmitError {
            message: format!(
                "parallel-for back-edge block jumps to {:?}, expected step {:?}",
                back_target, shape.step_block
            ),
        });
    }
    if back_args.len() != header.params.len() {
        return Err(EmitError {
            message: format!(
                "parallel-for back-edge carries {} args, header expects {}",
                back_args.len(),
                header.params.len()
            ),
        });
    }

    // The body's back-edge args go to step (not header).
    // Step does the `i + 1`. The first arg from body is
    // the OLD counter value (step takes it, adds 1, jumps
    // to header). For our purposes, find the actual
    // counter-increment value emitted in step so we can
    // skip it if it (theoretically) appears in body — in
    // practice body no longer has it after closure #187,
    // but the skip is harmless.
    let step_block = &f.blocks[shape.step_block.0 as usize];
    let counter_increment_value = step_block
        .instructions
        .iter()
        .find_map(|instr| match &instr.kind {
            InstrKind::Binary {
                op: BinaryOp::Add,
                ..
            } => Some(instr.result),
            _ => None,
        })
        .ok_or_else(|| EmitError {
            message: "parallel-for step block missing counter increment".to_string(),
        })?;

    // For each reduction, the back-edge arg at index `1 + i`
    // is the updated value. We emit it normally but assign
    // its result to the OpenMP reduction var.
    let reduction_update_values: Vec<ValueId> = back_args
        .iter()
        .skip(1)
        .map(|op| match op {
            Operand::Value(v) => Ok(*v),
            Operand::Const(_) => Err(EmitError {
                message: "parallel-for reduction back-edge arg must be a Value"
                    .to_string(),
            }),
        })
        .collect::<Result<_, _>>()?;

    // Reorder so `merge_block` is the LAST entry in
    // `region_blocks`. The C emit replaces the merge block's
    // terminator with reduction rebinds + fall-through to the
    // for-loop's closing `}`; placing it last makes that fall-
    // through correct without a synthetic `goto loop_end`
    // label.
    if let Some(pos) = region_blocks.iter().position(|b| *b == merge_block_id) {
        let m = region_blocks.remove(pos);
        region_blocks.push(m);
    }

    Ok(ParallelRegion {
        begin_block,
        shape: shape.clone(),
        reductions: reductions.to_vec(),
        reduction_carries,
        reduction_inits,
        counter_increment_value,
        reduction_update_values,
        region_blocks,
        merge_block: merge_block_id,
    })
}

fn emit_parallel_for_region(
    f: &Function,
    pre_header: &crate::ssa::BasicBlock,
    region: &ParallelRegion,
    value_types: &std::collections::BTreeMap<ValueId, Type>,
    out: &mut String,
) -> Result<(), EmitError> {
    // Emit every pre-header instruction except the Begin
    // hint itself. The SSA lowerer emits the
    // `lower_expr_to_value(start)` materialization AFTER the
    // Begin (placeholder-then-patch flow keeps Begin's
    // position fixed), so skipping the suffix would drop the
    // start-value definition.
    for instr in &pre_header.instructions {
        if matches!(
            &instr.kind,
            InstrKind::Hint(HintKind::ParallelForBegin { .. })
        ) {
            continue;
        }
        emit_instr(instr, value_types, out)?;
    }

    // Initialize each reduction carry to its incoming value
    // BEFORE the pragma. The OpenMP `reduction(op:v_<id>)`
    // clause makes the backend treat `v_<id>` as a
    // thread-private accumulator initialized to the op's
    // identity — but we want the user's `let total = 0; …
    // reduce total with +;` initial value to seed the
    // pre-loop value (OpenMP combines per-thread partials
    // with the seed at exit).
    for (carry_v, init_op) in
        region.reduction_carries.iter().zip(region.reduction_inits.iter())
    {
        writeln!(out, "  v_{} = {};", carry_v.0, c_operand(init_op)).unwrap();
    }

    // Pragma + reduction clauses.
    let _ = c_type(&region.shape.counter_ty)?;
    if region.reductions.is_empty() {
        out.push_str("  _Pragma(\"omp parallel for\")\n");
    } else {
        let clauses: Vec<String> = region
            .reductions
            .iter()
            .zip(region.reduction_carries.iter())
            .map(|((name, op, _ty), carry_v)| {
                let _ = name;
                format!("reduction({}: v_{})", omp_reduction_op(*op), carry_v.0)
            })
            .collect();
        writeln!(
            out,
            "  _Pragma(\"omp parallel for {}\")",
            clauses.join(" ")
        )
        .unwrap();
    }
    // The counter variable `v_<id>` is already
    // forward-declared at function scope. Using it in the
    // for-init without a leading type keeps that single
    // declaration authoritative (and avoids OpenMP's
    // "initializer references iteration variable" diagnostic
    // when we'd otherwise shadow with the same name).
    let counter_v = region.shape.counter_header_value;
    writeln!(
        out,
        "  for (v_{} = {}; v_{} < {}; v_{}++) {{",
        counter_v.0,
        c_operand(&region.shape.start),
        counter_v.0,
        c_operand(&region.shape.end),
        counter_v.0,
    )
    .unwrap();

    // Body region: closure #251 (Step 3b emit half). For
    // single-block bodies, `region_blocks == [body_block]` and
    // the loop below collapses to exactly the pre-#251 form
    // (emit body's instructions; skip the body's terminator
    // since the for-loop handles the back-edge). For multi-
    // block bodies (recognized via #241), we emit EVERY block
    // in the region with `bb<n>:` labels and standard
    // gotos — the same shape the parent block walk uses, just
    // nested inside the for-loop. The `merge_block` (unique
    // back-edge to step) is special: its terminator gets
    // replaced by the reduction rebind + fall-through to the
    // for's closing `}` (which loops back to body_block at the
    // top of the next iteration).
    for &bid in &region.region_blocks {
        let blk = &f.blocks[bid.0 as usize];
        // Emit a label for every region block except the entry
        // (body_block), which is reached by fall-through from
        // the `for (…) {` opener. Other blocks are goto'd to
        // from in-region branches, so they always need labels.
        if bid != region.shape.body_block {
            writeln!(out, "bb{}:", bid.0).unwrap();
        }
        for instr in &blk.instructions {
            if instr.result == region.counter_increment_value {
                continue;
            }
            emit_instr(instr, value_types, out)?;
        }
        if bid == region.merge_block {
            // Replace the back-edge with the reduction-update
            // rebinds. OpenMP's `reduction(op: v_<carry>)`
            // requires us to update the named accumulator
            // in place, so each iteration's "current value" is
            // correct.
            for (carry_v, update_v) in region
                .reduction_carries
                .iter()
                .zip(region.reduction_update_values.iter())
            {
                writeln!(out, "    v_{} = v_{};", carry_v.0, update_v.0).unwrap();
            }
            // Fall through to `}` — the for-loop iterates.
        } else {
            // Non-merge in-region block: emit its terminator
            // unchanged. Targets land in `region_blocks` (in-
            // region edge) or in `step_block` (in v1, only the
            // merge_block reaches step). Branch / Jump goto
            // forms match the parent block walk's emit so the
            // surrounding for-body reads as a tiny nested CFG.
            emit_terminator(&blk.terminator, f, out)?;
        }
    }
    out.push_str("  }\n");

    // Now transition to the exit block. Header's terminator
    // is `Branch(cond, body, [], exit, [header_param_values])`.
    // The exit-edge args are the header params themselves
    // — counter + reduction carries. After the for-loop the
    // counter is `end` (or past-end on `break`); reduction
    // carries hold the combined accumulator. Emit the
    // header→exit param assignments + goto.
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
                    "parallel-for header terminator must be Branch(_, body, [], exit, exit_args), got {:?}",
                    other
                ),
            });
        }
    };
    // Closure #206: per OpenMP, the loop iteration variable
    // is implicitly private inside `omp parallel for`; reading
    // its value AFTER the loop is undefined. The
    // emit_block_arg_assignments call below propagates the
    // header→exit args, which include the counter's
    // header-value (`v_<counter>`). Substitute any arg that
    // references the counter with the loop's `end` operand —
    // the well-defined post-loop counter value is exactly
    // `end` (or past-end on `break`, but the parallel-for
    // gate rejects `break` per closure #190). Without this,
    // gcc warns `v_<counter> is used uninitialized` and the
    // post-loop reads observe undefined values.
    let counter_value = Operand::Value(region.shape.counter_header_value);
    let exit_args: Vec<Operand> = exit_args
        .into_iter()
        .map(|arg| {
            if arg == counter_value {
                region.shape.end.clone()
            } else {
                arg
            }
        })
        .collect();
    emit_block_arg_assignments(f, region.shape.exit_block, &exit_args, "  ", out);
    writeln!(out, "  goto bb{};", region.shape.exit_block.0).unwrap();
    Ok(())
}

/// Map a source-level `ReductionOp` to the OpenMP `reduction`
/// clause spelling. Matches `tree-C`'s emit so cross-backend
/// parity holds.
fn omp_reduction_op(op: ReductionOp) -> &'static str {
    match op {
        ReductionOp::Add => "+",
        ReductionOp::Mul => "*",
        ReductionOp::And => "&&",
        ReductionOp::Or => "||",
        ReductionOp::Min => "min",
        ReductionOp::Max => "max",
        ReductionOp::BitAnd => "&",
        ReductionOp::BitOr => "|",
        ReductionOp::BitXor => "^",
    }
}

/// A task region recognized in pre-scan: a `TaskBegin`/
/// `TaskEnd` pair in the same block plus a matching
/// `TaskJoin` somewhere in the function. The body
/// instructions get lifted into a `static void*
/// intent_task_<N>(void*)` outline; the spawn site emits an
/// `intent_thread_create` call, the join site emits
/// `intent_thread_join` + `free` of the heap ctx.
struct TaskRegionC {
    handle: String,
    begin_block: BlockId,
    begin_idx: usize,
    /// Block holding the matching `TaskEnd` hint. May equal
    /// `begin_block` for single-block bodies, or differ for
    /// bodies with control flow (if/while/etc.).
    end_block: BlockId,
    end_idx: usize,
    /// Every block in `f.blocks` order that's part of the
    /// task body: begin_block first, then any intermediate
    /// blocks (between begin and end in source order), then
    /// end_block. Single-block bodies have just `[begin_block]`.
    body_blocks: Vec<BlockId>,
    join_block: BlockId,
    join_idx: usize,
    outline_id: u32,
}

fn collect_task_regions_c(f: &Function) -> Result<Vec<TaskRegionC>, EmitError> {
    struct Pending {
        handle: String,
        begin_block: BlockId,
        begin_idx: usize,
    }
    let mut pending: Vec<Pending> = Vec::new();
    let mut regions: Vec<TaskRegionC> = Vec::new();
    for block in &f.blocks {
        for (idx, instr) in block.instructions.iter().enumerate() {
            match &instr.kind {
                InstrKind::Hint(HintKind::TaskBegin { handle }) => {
                    pending.push(Pending {
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
                    // Body blocks via CFG reachability from
                    // begin → end. The previous `(begin_id..
                    // =end_id)` range-based approach missed
                    // blocks whose ID was greater than
                    // end_id but which still belong to the
                    // task body (e.g. step blocks that
                    // closures #185/#187 introduced after
                    // for-loop exit blocks, or any
                    // post-end-block control-flow chunk
                    // created during body lowering). Walk
                    // successors from begin_block, stopping
                    // at end_block (don't follow its
                    // successors — those are post-task).
                    // Closure #191.
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
                        // Always include the end block.
                        visited.insert(block.id);
                        visited.into_iter().collect()
                    };
                    regions.push(TaskRegionC {
                        handle: handle.clone(),
                        begin_block: begin.begin_block,
                        begin_idx: begin.begin_idx,
                        end_block: block.id,
                        end_idx: idx,
                        body_blocks,
                        join_block: BlockId(0),
                        join_idx: 0,
                        outline_id: 0,
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
    for region in regions.iter_mut() {
        region.outline_id = TASK_OUTLINE_COUNTER.with(|c| {
            let v = c.get();
            c.set(v + 1);
            v
        });
    }
    Ok(regions)
}

/// Emit the spawn-site for a task region: heap-alloc a ctx
/// struct, store each capture into its ctx field, declare a
/// handle local of type `intent_task_handle`, fire
/// `intent_thread_create` (which dispatches to
/// pthread_create on POSIX or CreateThread on Win32 via the
/// runtime wrapper). Side-emits the outlined function to
/// `TASK_OUTLINES`.
/// Returns `true` when the caller should skip the
/// surrounding block's terminator emit — multi-block tasks
/// inject their own `goto bb<end>;` terminator that
/// supersedes the original (which goes to intermediate body
/// blocks that no longer exist in the parent).
fn emit_task_spawn_region_c(
    f: &Function,
    region: &TaskRegionC,
    value_types: &std::collections::BTreeMap<ValueId, Type>,
    out: &mut String,
) -> Result<bool, EmitError> {
    let multi_block = region.begin_block != region.end_block;
    // Per-block body-instruction slices: which range of each
    // block's instructions belongs to the task body.
    //   begin_block: after TaskBegin (exclusive) → end of
    //                block (or end_idx for single-block).
    //   intermediate blocks: all instructions.
    //   end_block: 0 → before TaskEnd (exclusive).
    struct BodySlice {
        block_id: BlockId,
        instructions: Vec<usize>,
    }
    let mut slices: Vec<BodySlice> = Vec::new();
    for &bid in &region.body_blocks {
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
        let indices: Vec<usize> = (lo..hi).collect();
        slices.push(BodySlice {
            block_id: bid,
            instructions: indices,
        });
    }
    // Flatten all body instructions for capture analysis.
    let mut body_defined: std::collections::BTreeSet<ValueId> =
        std::collections::BTreeSet::new();
    for slice in &slices {
        let block = &f.blocks[slice.block_id.0 as usize];
        // Block params (phi-style) are body-defined too.
        for (v, _) in &block.params {
            body_defined.insert(*v);
        }
        for &i in &slice.instructions {
            body_defined.insert(block.instructions[i].result);
        }
    }
    let mut captures: Vec<(ValueId, Type)> = Vec::new();
    let mut capture_seen: std::collections::BTreeSet<ValueId> =
        std::collections::BTreeSet::new();
    let consider_operand =
        |op: &Operand,
         captures: &mut Vec<(ValueId, Type)>,
         capture_seen: &mut std::collections::BTreeSet<ValueId>|
         -> Result<(), EmitError> {
            if let Operand::Value(v) = op {
                if body_defined.contains(v) || !capture_seen.insert(*v) {
                    return Ok(());
                }
                let ty = value_types.get(v).cloned().ok_or_else(|| EmitError {
                    message: format!(
                        "task body captures v_{} but type is unknown",
                        v.0
                    ),
                })?;
                captures.push((*v, ty));
            }
            Ok(())
        };
    for slice in &slices {
        let block = &f.blocks[slice.block_id.0 as usize];
        for &i in &slice.instructions {
            for op in instr_operands_c(&block.instructions[i].kind) {
                consider_operand(op, &mut captures, &mut capture_seen)?;
            }
        }
        // For non-final body blocks, the block's terminator
        // (Jump/Branch) is also part of the body — its
        // operand may reference captures.
        if multi_block && slice.block_id != region.end_block {
            match &block.terminator {
                Terminator::Jump { args, .. } => {
                    for op in args {
                        consider_operand(op, &mut captures, &mut capture_seen)?;
                    }
                }
                Terminator::Branch { cond, then_args, else_args, .. } => {
                    consider_operand(cond, &mut captures, &mut capture_seen)?;
                    for op in then_args.iter().chain(else_args.iter()) {
                        consider_operand(op, &mut captures, &mut capture_seen)?;
                    }
                }
                _ => {}
            }
        }
    }

    let id = region.outline_id;
    let outline_fn = format!("intent_task_{}", id);
    let struct_name = format!("intent_task_{}_ctx", id);

    // Build the outline + ctx-struct typedef in a side
    // buffer to splice via TASK_OUTLINES.
    let mut outline = String::new();
    writeln!(&mut outline, "typedef struct {} {{", struct_name).unwrap();
    for (cap_v, cap_ty) in &captures {
        let decl = c_declarator(cap_ty, &format!("cap_{}", cap_v.0))?;
        writeln!(&mut outline, "  {};", decl).unwrap();
    }
    if captures.is_empty() {
        // C doesn't allow empty structs; add a dummy.
        outline.push_str("  char _intent_dummy;\n");
    }
    writeln!(&mut outline, "}} {};\n", struct_name).unwrap();
    writeln!(
        &mut outline,
        "static void* {}(void* _ctx_raw) {{",
        outline_fn
    )
    .unwrap();
    writeln!(
        &mut outline,
        "  {}* ctx = ({}*)_ctx_raw;",
        struct_name, struct_name
    )
    .unwrap();
    // Capture loads: alias each ctx field to its
    // source-language SSA name.
    for (cap_v, cap_ty) in &captures {
        let decl = c_declarator(cap_ty, &format!("v_{}", cap_v.0))?;
        writeln!(
            &mut outline,
            "  {} = ctx->cap_{};",
            decl, cap_v.0
        )
        .unwrap();
    }
    // Forward-declare every body-defined SSA value
    // (instructions and block params) so labeled blocks
    // can use them out of order.
    let mut outline_value_types = value_types.clone();
    for (cap_v, cap_ty) in &captures {
        outline_value_types.insert(*cap_v, cap_ty.clone());
    }
    for slice in &slices {
        let block = &f.blocks[slice.block_id.0 as usize];
        for (v, ty) in &block.params {
            outline_value_types.insert(*v, ty.clone());
            let decl = c_declarator(ty, &format!("v_{}", v.0))?;
            writeln!(&mut outline, "  {};", decl).unwrap();
        }
        for &i in &slice.instructions {
            let instr = &block.instructions[i];
            outline_value_types.insert(instr.result, instr.ty.clone());
            if !is_void_kind(&instr.kind) {
                let decl = c_declarator(&instr.ty, &format!("v_{}", instr.result.0))?;
                writeln!(&mut outline, "  {};", decl).unwrap();
            }
        }
    }
    // Multi-block bodies need to start at begin_block's
    // label (single-block bodies fall through linearly).
    if multi_block {
        writeln!(&mut outline, "  goto bb{};", region.begin_block.0).unwrap();
    }
    for slice in &slices {
        let block = &f.blocks[slice.block_id.0 as usize];
        if multi_block {
            writeln!(&mut outline, "bb{}:", slice.block_id.0).unwrap();
        }
        for &i in &slice.instructions {
            emit_instr(&block.instructions[i], &outline_value_types, &mut outline)?;
        }
        if multi_block && slice.block_id != region.end_block {
            emit_terminator(&block.terminator, f, &mut outline)?;
        }
        if !multi_block || slice.block_id == region.end_block {
            outline.push_str("  return (void*)0;\n");
        }
    }
    outline.push_str("}\n\n");
    TASK_OUTLINES.with(|b| b.borrow_mut().push_str(&outline));

    // Spawn site in the parent function. The handle local is
    // named after the source-level task handle (`v_<id>`-
    // style isn't quite right since the source name carries
    // the handle, not a ValueId — use `task_<name>_handle`).
    let handle_name = format!("task_{}_handle", region.handle);
    let ctx_local = format!("_intent_ctx_task{}", id);
    writeln!(out, "  intent_task_handle {};", handle_name).unwrap();
    writeln!(
        out,
        "  {}* {} = ({}*)malloc(sizeof({}));",
        struct_name, ctx_local, struct_name, struct_name
    )
    .unwrap();
    for (cap_v, _) in &captures {
        writeln!(
            out,
            "  {}->cap_{} = v_{};",
            ctx_local, cap_v.0, cap_v.0
        )
        .unwrap();
    }
    writeln!(
        out,
        "  intent_thread_create(&{}.thread, {}, {});",
        handle_name, outline_fn, ctx_local
    )
    .unwrap();
    writeln!(out, "  {}.ctx = {};", handle_name, ctx_local).unwrap();
    if multi_block {
        // begin_block's original terminator went to a body
        // block that's now in the outlined fn. Jump straight
        // to end_block (its post-TaskEnd suffix is what runs
        // next in the parent).
        writeln!(out, "  goto bb{};", region.end_block.0).unwrap();
    }
    Ok(multi_block)
}

/// Emit the join-site: block on the thread via
/// `intent_thread_join` (POSIX pthread_join / Win32
/// WaitForSingleObject) and free the heap ctx.
fn emit_task_join_c(region: &TaskRegionC, out: &mut String) {
    let handle_name = format!("task_{}_handle", region.handle);
    writeln!(out, "  intent_thread_join({}.thread);", handle_name).unwrap();
    writeln!(out, "  free({}.ctx);", handle_name).unwrap();
}

/// Same shape as ssa_backend_llvm::instr_operands.
fn instr_operands_c(kind: &InstrKind) -> Vec<&Operand> {
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

fn is_void_kind(kind: &InstrKind) -> bool {
    matches!(
        kind,
        InstrKind::Drop { .. }
            | InstrKind::IndexAssign { .. }
            | InstrKind::Hint(_)
    )
}

fn emit_instr(
    instr: &crate::ssa::Instruction,
    value_types: &std::collections::BTreeMap<ValueId, Type>,
    out: &mut String,
) -> Result<(), EmitError> {
    /// Helper: resolve an Operand to its source-language Type
    /// (or None for Const operands).
    fn operand_type(
        op: &Operand,
        value_types: &std::collections::BTreeMap<ValueId, Type>,
    ) -> Option<Type> {
        match op {
            Operand::Value(v) => value_types.get(v).cloned(),
            Operand::Const(_) => None,
        }
    }
    match &instr.kind {
        InstrKind::Const(c) => {
            writeln!(out, "  v_{} = {};", instr.result.0, c_const(c)).unwrap();
        }
        InstrKind::Unary { op, x } => {
            let symbol = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "!",
            };
            writeln!(
                out,
                "  v_{} = {}{};",
                instr.result.0,
                symbol,
                c_operand(x)
            )
            .unwrap();
        }
        InstrKind::Binary { op, l, r } => {
            writeln!(
                out,
                "  v_{} = ({}) {} ({});",
                instr.result.0,
                c_operand(l),
                op.display_symbol(),
                c_operand(r)
            )
            .unwrap();
        }
        InstrKind::Call { name, args } => {
            // `intent_print` is the printer the typed-IR
            // analogue introduced; we don't fully model it
            // here. Emit as an empty inline action so the
            // pass-of-concept still compiles; tests on this
            // backend pick programs that don't print.
            if name == "intent_print_item" {
                // Per-item printer used by SSA-lowered
                // multi-item `print` statements. Emits the
                // value with a type-dispatched printf format
                // and NO trailing newline; the lowerer also
                // emits `intent_print_putc` for separator
                // spaces and the trailing newline.
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "intent_print_item expects 1 argument".to_string(),
                })?;
                let aty = match arg {
                    Operand::Value(v) => value_types.get(v).cloned(),
                    Operand::Const(_) => None,
                }
                .unwrap_or(Type::I64);
                // Bool prints as the human-readable
                // `true`/`false` (parallel to tree-C / tree-
                // LLVM). The branch picks the string via
                // `?:` and emits via fputs to avoid printf's
                // varargs overhead.
                if matches!(aty, Type::Bool) {
                    writeln!(
                        out,
                        "  fputs(({}) ? \"true\" : \"false\", stdout);",
                        c_operand(arg)
                    )
                    .unwrap();
                    return Ok(());
                }
                let fmt = match aty {
                    Type::F32 | Type::F64 => "%g",
                    Type::Str | Type::OwnedStr => "%s",
                    _ => "%lld",
                };
                let cast = match aty {
                    Type::F32 | Type::F64 => "(double)",
                    Type::Str | Type::OwnedStr => "",
                    _ => "(long long)",
                };
                writeln!(
                    out,
                    "  printf(\"{}\", {}({}));",
                    fmt,
                    cast,
                    c_operand(arg)
                )
                .unwrap();
                return Ok(());
            }
            if name == "intent_print_putc" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "intent_print_putc expects 1 argument".to_string(),
                })?;
                writeln!(out, "  putchar((int)({}));", c_operand(arg)).unwrap();
                return Ok(());
            }
            if name == "intent_assert_fail" {
                // If the assert carries a custom message
                // (lowered as a `StrLit` `Str` arg), emit
                // `fprintf(stderr, "assertion failed: %s\n",
                // msg); abort();` — matches tree-C's shape so
                // tests that scrape the stderr text agree.
                if let Some(arg) = args.first() {
                    writeln!(
                        out,
                        "  fprintf(stderr, \"assertion failed: %s\\n\", {});",
                        c_operand(arg)
                    )
                    .unwrap();
                }
                writeln!(out, "  abort();").unwrap();
                return Ok(());
            }
            if name == "intent_str_cmp" {
                // Two-arg call returning i64 (matches the SSA
                // `Call.ty = I64`). C's `strcmp` returns
                // `int`; widen with a cast so the result fits
                // the SSA value's i64 slot. The result name
                // was already forward-declared at the top of
                // the function body — emit only the
                // assignment.
                let lhs = args.get(0).ok_or_else(|| EmitError {
                    message: "intent_str_cmp expects 2 args".to_string(),
                })?;
                let rhs = args.get(1).ok_or_else(|| EmitError {
                    message: "intent_str_cmp expects 2 args".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = (int64_t)strcmp({}, {});",
                    instr.result.0,
                    c_operand(lhs),
                    c_operand(rhs)
                )
                .unwrap();
                return Ok(());
            }
            if name == "intent_str_len" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "intent_str_len expects 1 arg".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = (uint64_t)strlen({});",
                    instr.result.0,
                    c_operand(arg)
                )
                .unwrap();
                return Ok(());
            }
            if name == "intent_str_concat" {
                // 4-arg call (l, l_owned, r, r_owned) → char*.
                // The runtime helper is named without an
                // `fn_` prefix and takes two `int` flag args
                // (truncate from the SSA i64 consts).
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
                writeln!(
                    out,
                    "  v_{} = intent_str_concat({}, (int)({}), {}, (int)({}));",
                    instr.result.0,
                    c_operand(l),
                    c_operand(lo),
                    c_operand(r),
                    c_operand(ro)
                )
                .unwrap();
                return Ok(());
            }
            // Atomic intrinsics — five C11 `<stdatomic.h>`
            // ops dispatched by name. Each lowers inline so
            // the SSA Call's result-binding stays a plain
            // `v_<id> = <expr>;` shape.
            if name == "atomic_new" {
                // Constructor: just initialize the cell with
                // the initial value. C11 atomics support
                // direct initialization.
                let initial = args.first().ok_or_else(|| EmitError {
                    message: "atomic_new expects 1 arg".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = ({});",
                    instr.result.0,
                    c_operand(initial)
                )
                .unwrap();
                return Ok(());
            }
            if name == "atomic_load" {
                let cell = args.first().ok_or_else(|| EmitError {
                    message: "atomic_load expects 1 arg".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = atomic_load_explicit({}, memory_order_seq_cst);",
                    instr.result.0,
                    c_operand(cell)
                )
                .unwrap();
                return Ok(());
            }
            if name == "atomic_store" {
                // C11's `atomic_store_explicit` returns void
                // but the SSA call must produce a usable
                // value. Wrap in a GNU statement-expression
                // so the call site sees a value of the
                // element type.
                let cell = args.get(0).ok_or_else(|| EmitError {
                    message: "atomic_store expects 2 args".to_string(),
                })?;
                let val = args.get(1).ok_or_else(|| EmitError {
                    message: "atomic_store expects 2 args".to_string(),
                })?;
                let elt_c = match &instr.ty {
                    t if t.is_integer() || matches!(t, Type::Bool) => c_atomic_leaf(t)?,
                    other => {
                        return Err(EmitError {
                            message: format!(
                                "atomic_store result {:?} isn't a supported atomic element",
                                other
                            ),
                        });
                    }
                };
                writeln!(
                    out,
                    "  v_{} = ({{ {elt} __v = ({val}); atomic_store_explicit({cell}, __v, memory_order_seq_cst); __v; }});",
                    instr.result.0,
                    elt = elt_c,
                    val = c_operand(val),
                    cell = c_operand(cell),
                )
                .unwrap();
                return Ok(());
            }
            if name == "atomic_fetch_add" {
                let cell = args.get(0).ok_or_else(|| EmitError {
                    message: "atomic_fetch_add expects 2 args".to_string(),
                })?;
                let delta = args.get(1).ok_or_else(|| EmitError {
                    message: "atomic_fetch_add expects 2 args".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = atomic_fetch_add_explicit({}, {}, memory_order_seq_cst);",
                    instr.result.0,
                    c_operand(cell),
                    c_operand(delta)
                )
                .unwrap();
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
                // Result is bool; the helper writes the
                // observed value into `__cas_exp` on failure
                // (and to ignore it here we just discard the
                // post-call expected via the statement-expr's
                // final expression).
                let elt_c = match value_types.get(&match exp {
                    Operand::Value(v) => *v,
                    Operand::Const(_) => ValueId(0),
                }) {
                    Some(t) => c_atomic_leaf(t)?,
                    None => c_atomic_leaf(&Type::I64)?,
                };
                writeln!(
                    out,
                    "  v_{} = ({{ {elt} __cas_exp = ({exp}); atomic_compare_exchange_strong_explicit({cell}, &__cas_exp, ({new}), memory_order_seq_cst, memory_order_seq_cst); }});",
                    instr.result.0,
                    elt = elt_c,
                    exp = c_operand(exp),
                    cell = c_operand(cell),
                    new = c_operand(new_v),
                )
                .unwrap();
                return Ok(());
            }
            // Mutex / Guard intrinsics — i64-only for v1,
            // dispatched to the shared runtime helpers
            // `intent_mutex_i64_*` / `intent_guard_i64_*`.
            if name == "mutex_new" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "mutex_new expects 1 arg".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = intent_mutex_i64_new({});",
                    instr.result.0,
                    c_operand(arg)
                )
                .unwrap();
                return Ok(());
            }
            if name == "mutex_lock" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "mutex_lock expects 1 arg".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = intent_mutex_i64_lock({});",
                    instr.result.0,
                    c_operand(arg)
                )
                .unwrap();
                return Ok(());
            }
            if name == "guard_get" {
                let arg = args.first().ok_or_else(|| EmitError {
                    message: "guard_get expects 1 arg".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = intent_guard_i64_get({});",
                    instr.result.0,
                    c_operand(arg)
                )
                .unwrap();
                return Ok(());
            }
            if name == "guard_set" {
                let g = args.get(0).ok_or_else(|| EmitError {
                    message: "guard_set expects 2 args".to_string(),
                })?;
                let v = args.get(1).ok_or_else(|| EmitError {
                    message: "guard_set expects 2 args".to_string(),
                })?;
                writeln!(
                    out,
                    "  v_{} = intent_guard_i64_set({}, {});",
                    instr.result.0,
                    c_operand(g),
                    c_operand(v)
                )
                .unwrap();
                return Ok(());
            }
            // Channel intrinsics — dispatch via the per-(T,
            // N) helper names. `channel_new` returns the
            // by-value struct; `channel_send`/`channel_recv`
            // take a `&Channel<T, N>` pointer.
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
                writeln!(
                    out,
                    "  v_{} = {}();",
                    instr.result.0,
                    crate::backend_c::c_channel_helper(&element, capacity, "new")
                )
                .unwrap();
                return Ok(());
            }
            if name == "channel_send" || name == "channel_recv" {
                let chan = args.first().ok_or_else(|| EmitError {
                    message: format!("{} expects ≥1 arg", name),
                })?;
                let chan_ty = match chan {
                    Operand::Value(v) => value_types.get(v).cloned(),
                    Operand::Const(_) => None,
                }
                .ok_or_else(|| EmitError {
                    message: format!("{} arg's type is unknown", name),
                })?;
                let (element, capacity) =
                    crate::backend_c::channel_inner_from_ref_pub(&chan_ty);
                let op = if name == "channel_send" { "send" } else { "recv" };
                if name == "channel_send" {
                    let val = args.get(1).ok_or_else(|| EmitError {
                        message: "channel_send expects 2 args".to_string(),
                    })?;
                    writeln!(
                        out,
                        "  v_{} = {}({}, {});",
                        instr.result.0,
                        crate::backend_c::c_channel_helper(&element, capacity, op),
                        c_operand(chan),
                        c_operand(val)
                    )
                    .unwrap();
                } else {
                    writeln!(
                        out,
                        "  v_{} = {}({});",
                        instr.result.0,
                        crate::backend_c::c_channel_helper(&element, capacity, op),
                        c_operand(chan)
                    )
                    .unwrap();
                }
                return Ok(());
            }
            // Vec builtins: route through the shared runtime
            // helpers in `backend_c::*` (vec_c_struct +
            // vec_helper). The result type carries the
            // element so we can pick the right helper name.
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
                let helper = crate::backend_c::vec_helper(&element, match name.as_str() {
                    "vec" => "from",
                    op => op,
                });
                if name == "vec" {
                    // The C runtime's `intent_vec_<T>__from(n,
                    // ptr)` takes a count + a pointer to an
                    // array of T. Materialize the args as a
                    // compound literal; for the empty-vec case
                    // (refines #8 from STATUS.md) pass NULL
                    // because C99 forbids zero-length array
                    // literals.
                    let c_element = c_type(&element)?;
                    let arg_strs: Vec<String> =
                        args.iter().map(c_operand).collect();
                    if arg_strs.is_empty() {
                        writeln!(
                            out,
                            "  v_{} = {}(0, (const {}*)0);",
                            instr.result.0, helper, c_element
                        )
                        .unwrap();
                    } else {
                        let array_literal = format!(
                            "({}[{}]){{ {} }}",
                            c_element,
                            arg_strs.len(),
                            arg_strs.join(", ")
                        );
                        writeln!(
                            out,
                            "  v_{} = {}({}, (const {}*){});",
                            instr.result.0,
                            helper,
                            arg_strs.len(),
                            c_element,
                            array_literal
                        )
                        .unwrap();
                    }
                } else {
                    let arg_strs: Vec<String> =
                        args.iter().map(c_operand).collect();
                    writeln!(
                        out,
                        "  v_{} = {}({});",
                        instr.result.0,
                        helper,
                        arg_strs.join(", ")
                    )
                    .unwrap();
                }
                return Ok(());
            }
            let arg_strs: Vec<String> = args.iter().map(c_operand).collect();
            // `min` / `max` are intrinsic functions, not user-
            // defined. Mirror tree-C's inline-ternary emit so
            // the call doesn't try to link against a missing
            // `fn_min` / `fn_max`. Operands are SSA values
            // (single C identifiers) so the ternary's
            // double-evaluation isn't a side-effect hazard.
            if name == "min" || name == "max" {
                if let [a, b] = arg_strs.as_slice() {
                    let cmp = if name == "min" { "<" } else { ">" };
                    writeln!(
                        out,
                        "  v_{} = (({}) {} ({}) ? ({}) : ({}));",
                        instr.result.0, a, cmp, b, a, b
                    )
                    .unwrap();
                    return Ok(());
                }
            }
            // Closure #154: `clone_at(xs, i)` returns a deep
            // copy of slot i. Was falling through to the
            // `fn_clone_at(...)` user-fn shape (undeclared
            // identifier, link error). Resolve the element
            // type from the xs operand's Vec / &Vec / &mut
            // Vec type, then route the slot expression
            // through `c_element_deep_clone` (the same
            // helper tree-C uses).
            if name == "clone_at" {
                let xs_arg = args.get(0).ok_or_else(|| EmitError {
                    message: "clone_at expects 2 args".to_string(),
                })?;
                let i_arg = args.get(1).ok_or_else(|| EmitError {
                    message: "clone_at expects 2 args".to_string(),
                })?;
                let xs_ty = operand_type(xs_arg, value_types).ok_or_else(|| {
                    EmitError {
                        message: "clone_at xs operand has unknown type".to_string(),
                    }
                })?;
                let (element_ty, via_ref) = match &xs_ty {
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
                let xs_str = c_operand(xs_arg);
                let i_str = c_operand(i_arg);
                let slot = if via_ref {
                    format!("({})->data[{}]", xs_str, i_str)
                } else {
                    format!("({}).data[{}]", xs_str, i_str)
                };
                let cloned = crate::backend_c::c_element_deep_clone(
                    &slot,
                    &element_ty,
                );
                writeln!(out, "  v_{} = {};", instr.result.0, cloned).unwrap();
                return Ok(());
            }
            writeln!(
                out,
                "  v_{} = fn_{}({});",
                instr.result.0,
                name,
                arg_strs.join(", ")
            )
            .unwrap();
        }
        InstrKind::Cast { x, to } => {
            writeln!(
                out,
                "  v_{} = ({})({});",
                instr.result.0,
                c_type(to)?,
                c_operand(x)
            )
            .unwrap();
        }
        InstrKind::Hint(_) => {
            // Pure structural markers (parallel-for / task
            // begin/end). The goto-emit treats parallel
            // bodies as sequential; backends that want to
            // outline read these markers as a separate
            // analysis pass.
        }
        InstrKind::Drop { source, ty, .. } => {
            // Vec drop: call the runtime's free helper.
            // OwnedStr drop: free the heap-allocated `char*`
            // returned by `intent_str_concat`. Guard drop:
            // unlock the underlying mutex. Atomic/Mutex/Channel
            // are by-value stack-allocated structs with no heap-
            // owned buffers (Channel embeds an inline
            // `buf[CAP]` array — see `emit_channel_bundle` in
            // `backend_c.rs`) — alloca scope-out frees them, no
            // extra emit. Scalar types are no-op.
            match ty {
                Type::Vec(element) => {
                    let helper = crate::backend_c::vec_helper(element, "free");
                    writeln!(out, "  {}({});", helper, c_operand(source)).unwrap();
                }
                Type::OwnedStr => {
                    writeln!(out, "  free((void*){});", c_operand(source)).unwrap();
                }
                Type::Guard(_) => {
                    // `intent_guard_i64_unlock` takes a
                    // non-const `intent_guard_i64*` so we
                    // pass `&` of the SSA value, then cast
                    // through `intent_guard_i64*` (the SSA
                    // value's C type is already correct).
                    writeln!(
                        out,
                        "  intent_guard_i64_unlock(&{});",
                        c_operand(source)
                    )
                    .unwrap();
                }
                _ => {}
            }
        }
        InstrKind::FnRef { name } => {
            // C function names decay to function pointers in
            // non-call contexts. Emit the bare prefixed
            // identifier as the SSA value's source.
            writeln!(out, "  v_{} = fn_{};", instr.result.0, name).unwrap();
        }
        InstrKind::CallIndirect { callee, args } => {
            // Indirect call through a fn-pointer SSA operand.
            // In C this is just `ptr(args)`.
            let arg_strs: Vec<String> = args.iter().map(c_operand).collect();
            writeln!(
                out,
                "  v_{} = {}({});",
                instr.result.0,
                c_operand(callee),
                arg_strs.join(", ")
            )
            .unwrap();
        }
        InstrKind::ArrayLit { elements } => {
            // The result type is `[T; N]`; the forward
            // declaration already produced `T v_<id>[N];`, so
            // assign each slot in turn.
            for (i, e) in elements.iter().enumerate() {
                writeln!(
                    out,
                    "  v_{}[{}] = {};",
                    instr.result.0,
                    i,
                    c_operand(e)
                )
                .unwrap();
            }
        }
        InstrKind::Index { array, index, .. } => {
            // Dispatch on the array operand's source type.
            // Arrays / array refs index naturally; Vec needs
            // a `.data` indirection; Vec refs need `->data`.
            let array_op_str = c_operand(array);
            let idx_str = c_operand(index);
            let array_ty = operand_type(array, value_types);
            match array_ty.as_ref().map(|t| t.deref().clone()) {
                Some(Type::Vec(_)) => {
                    // Owned Vec or Ref/RefMut to Vec — deref
                    // yields Vec. Use `.data` (if value) or
                    // `->data` (if ref).
                    let dot = if matches!(
                        array_ty.as_ref().unwrap(),
                        Type::Ref(_) | Type::RefMut(_)
                    ) {
                        "->"
                    } else {
                        "."
                    };
                    writeln!(
                        out,
                        "  v_{} = {}{}data[{}];",
                        instr.result.0, array_op_str, dot, idx_str
                    )
                    .unwrap();
                }
                _ => {
                    // Array / array-ref / unknown — use
                    // pointer-decayed index syntax which
                    // works for both.
                    writeln!(
                        out,
                        "  v_{} = {}[{}];",
                        instr.result.0, array_op_str, idx_str
                    )
                    .unwrap();
                }
            }
        }
        InstrKind::Len { array, length } => {
            // Dispatch on array operand type. For fixed
            // arrays `length` is the static N; for Vec/Vec-
            // ref read `.len` / `->len` from the runtime
            // struct.
            let array_ty = operand_type(array, value_types);
            match array_ty.as_ref().map(|t| t.deref().clone()) {
                Some(Type::Vec(_)) => {
                    let dot = if matches!(
                        array_ty.as_ref().unwrap(),
                        Type::Ref(_) | Type::RefMut(_)
                    ) {
                        "->"
                    } else {
                        "."
                    };
                    writeln!(
                        out,
                        "  v_{} = (int64_t)({}{}len);",
                        instr.result.0,
                        c_operand(array),
                        dot
                    )
                    .unwrap();
                }
                _ => {
                    writeln!(
                        out,
                        "  v_{} = (int64_t){};",
                        instr.result.0, length
                    )
                    .unwrap();
                }
            }
        }
        InstrKind::IndexAssign { array, index, value, .. } => {
            // Dispatch on operand type. Vec write goes
            // through `.data`; arrays/refs write through the
            // pointer-decayed name.
            let array_ty = operand_type(array, value_types);
            match array_ty.as_ref().map(|t| t.deref().clone()) {
                Some(Type::Vec(element)) => {
                    let dot = if matches!(
                        array_ty.as_ref().unwrap(),
                        Type::Ref(_) | Type::RefMut(_)
                    ) {
                        "->"
                    } else {
                        "."
                    };
                    // Closure #150 (SSA-C): when the element
                    // type is heap-shaped, free the OLD slot
                    // before storing the new value. Same
                    // shape as tree-C's emit_index_assign
                    // whole-element drop.
                    let lv = format!(
                        "{}{}data[{}]",
                        c_operand(array),
                        dot,
                        c_operand(index)
                    );
                    match element.as_ref() {
                        Type::OwnedStr => {
                            writeln!(out, "  free((void*){});", lv).unwrap();
                        }
                        Type::Vec(inner) => {
                            writeln!(
                                out,
                                "  {}({});",
                                crate::backend_c::vec_helper(inner, "free"),
                                lv
                            )
                            .unwrap();
                        }
                        _ => {}
                    }
                    writeln!(out, "  {} = {};", lv, c_operand(value)).unwrap();
                }
                _ => {
                    writeln!(
                        out,
                        "  {}[{}] = {};",
                        c_operand(array),
                        c_operand(index),
                        c_operand(value)
                    )
                    .unwrap();
                }
            }
        }
        InstrKind::RefOf { source, mut_ } => {
            // `&v` / `&mut v` lowers to `&v_<id>` (or just
            // `v_<id>` for arrays — C decays them in pointer
            // position). The result's type (`Type::Ref(inner)`
            // / `Type::RefMut(inner)`) drives the cast.
            let _ = mut_;
            match &instr.ty {
                Type::Ref(inner) | Type::RefMut(inner) => match &**inner {
                    Type::Array { .. } => {
                        // Arrays decay to pointers naturally.
                        writeln!(
                            out,
                            "  v_{} = {};",
                            instr.result.0,
                            c_operand(source)
                        )
                        .unwrap();
                    }
                    _ => {
                        writeln!(
                            out,
                            "  v_{} = &{};",
                            instr.result.0,
                            c_operand(source)
                        )
                        .unwrap();
                    }
                },
                other => {
                    return Err(EmitError {
                        message: format!(
                            "RefOf result type must be Ref/RefMut, got {:?}",
                            other
                        ),
                    });
                }
            }
        }
        InstrKind::StrLit(text) => {
            // Inline string literal. Escape the content for
            // safe inclusion in a C double-quoted string.
            let mut escaped = String::with_capacity(text.len() + 2);
            for c in text.chars() {
                match c {
                    '\\' => escaped.push_str("\\\\"),
                    '"' => escaped.push_str("\\\""),
                    '\n' => escaped.push_str("\\n"),
                    '\t' => escaped.push_str("\\t"),
                    '\r' => escaped.push_str("\\r"),
                    c if c.is_ascii() && !c.is_ascii_control() => {
                        escaped.push(c);
                    }
                    other => {
                        // Non-ASCII or control: emit as
                        // octal escape so the C compiler
                        // accepts arbitrary bytes.
                        for b in other.to_string().bytes() {
                            escaped.push_str(&format!("\\{:03o}", b));
                        }
                    }
                }
            }
            writeln!(
                out,
                "  v_{} = \"{}\";",
                instr.result.0, escaped
            )
            .unwrap();
        }
    }
    Ok(())
}

fn emit_terminator(term: &Terminator, f: &Function, out: &mut String) -> Result<(), EmitError> {
    match term {
        Terminator::Return(None) => out.push_str("  return;\n"),
        Terminator::Return(Some(op)) => {
            writeln!(out, "  return {};", c_operand(op)).unwrap();
        }
        Terminator::Jump { target, args } => {
            emit_block_arg_assignments(f, *target, args, "  ", out);
            writeln!(out, "  goto bb{};", target.0).unwrap();
        }
        Terminator::Branch {
            cond,
            then_block,
            then_args,
            else_block,
            else_args,
        } => {
            writeln!(out, "  if ({}) {{", c_operand(cond)).unwrap();
            emit_block_arg_assignments(f, *then_block, then_args, "    ", out);
            writeln!(out, "    goto bb{};", then_block.0).unwrap();
            out.push_str("  } else {\n");
            emit_block_arg_assignments(f, *else_block, else_args, "    ", out);
            writeln!(out, "    goto bb{};", else_block.0).unwrap();
            out.push_str("  }\n");
        }
        Terminator::Unreachable => out.push_str("  abort();\n"),
    }
    Ok(())
}

fn emit_block_arg_assignments(
    f: &Function,
    target: crate::ssa::BlockId,
    args: &[Operand],
    indent: &str,
    out: &mut String,
) {
    let target_block = &f.blocks[target.0 as usize];
    for (i, arg) in args.iter().enumerate() {
        let Some((param_v, _ty)) = target_block.params.get(i) else {
            // Shouldn't happen in well-formed SSA; the lowerer
            // guarantees args.len() == params.len(). Skip
            // defensively rather than panic.
            continue;
        };
        writeln!(
            out,
            "{}v_{} = {};",
            indent,
            param_v.0,
            c_operand(arg)
        )
        .unwrap();
    }
}

fn c_type(ty: &Type) -> Result<&'static str, EmitError> {
    Ok(match ty {
        Type::I8 => "int8_t",
        Type::I16 => "int16_t",
        Type::I32 => "int32_t",
        Type::I64 => "int64_t",
        Type::U8 => "uint8_t",
        Type::U16 => "uint16_t",
        Type::U32 => "uint32_t",
        Type::U64 => "uint64_t",
        Type::Bool => "bool",
        Type::F32 => "float",
        Type::F64 => "double",
        // Str is a borrowed read-only pointer; OwnedStr is a
        // mutable heap pointer that the runtime mallocs and
        // frees. Spelling OwnedStr as `const char*` triggered
        // a -Wdiscarded-qualifiers warning at every store
        // into a `char**` Vec slot (the Vec helper bundle
        // matches tree-C's `char* data` field). Closure
        // #175.
        Type::Str => "const char*",
        Type::OwnedStr => "char*",
        other => {
            return Err(EmitError {
                message: format!(
                    "type {:?} is outside the SSA-C scalar/string subset",
                    other
                ),
            })
        }
    })
}

/// Leaf integer/bool spelling for an atomic cell's element
/// type. Mirrors `backend_c::c_leaf_type` (which we don't
/// expose for the same reason — atomic-cell sizing must
/// match between the two backends). Bool stays bool —
/// `_Atomic _Bool` is valid C11.
fn c_atomic_leaf(ty: &Type) -> Result<&'static str, EmitError> {
    Ok(match ty {
        Type::I8 => "int8_t",
        Type::I16 => "int16_t",
        Type::I32 => "int32_t",
        Type::I64 => "int64_t",
        Type::U8 => "uint8_t",
        Type::U16 => "uint16_t",
        Type::U32 => "uint32_t",
        Type::U64 => "uint64_t",
        Type::Bool => "_Bool",
        other => {
            return Err(EmitError {
                message: format!(
                    "Atomic element type {:?} not supported in SSA-C",
                    other
                ),
            })
        }
    })
}


/// Declarator-style emit that handles aggregate types where
/// `<type> <name>;` syntax doesn't work (arrays, refs, etc.).
/// For scalars this is identical to `format!("{} {}", c_type,
/// name)`; for `[T; N]` it produces `T name[N]`; for `&T` /
/// `&mut T` it produces `T* name` (with `const` for `&T`).
fn c_declarator(ty: &Type, name: &str) -> Result<String, EmitError> {
    Ok(match ty {
        Type::Array { element, length } => {
            format!("{} {}[{}]", c_type(element)?, name, length)
        }
        Type::Vec(element) => {
            format!("{} {}", crate::backend_c::vec_c_struct(element), name)
        }
        Type::Atomic(element) => {
            // `_Atomic <T> name;` — the cell itself. Affine
            // ownership keeps the binding unique so by-value
            // declaration is safe; atomic ops always take
            // `_Atomic <T>*` so call sites apply `&` at the
            // use site.
            format!("_Atomic {} {}", c_atomic_leaf(element)?, name)
        }
        // Mutex/Guard are i64-only in v1, matching tree-C's
        // runtime helpers `intent_mutex_i64` / `intent_guard_i64`.
        // The runtime headers are emitted from the SSA-C preamble.
        Type::Mutex(_) => format!("intent_mutex_i64 {}", name),
        Type::Guard(_) => format!("intent_guard_i64 {}", name),
        // `Channel<T, N>` uses a per-(T, N) struct (Vyukov
        // MPSC ring buffer); the bundle gets emitted from
        // SSA-C's preamble walker.
        Type::Channel(element, capacity) => {
            format!(
                "{} {}",
                crate::backend_c::c_channel_storage(element, *capacity),
                name
            )
        }
        Type::Ref(inner) => match &**inner {
            Type::Array { element, .. } => {
                // `&[T; N]` decays to `const T*` in argument
                // position; for a local we use the same form.
                format!("const {}* {}", c_type(element)?, name)
            }
            Type::Vec(element) => {
                format!(
                    "const {}* {}",
                    crate::backend_c::vec_c_struct(element),
                    name
                )
            }
            Type::Atomic(element) => {
                // Atomic refs DROP the `const` qualifier so
                // `atomic_store_explicit` / `atomic_fetch_add`
                // can take a non-const cell pointer.
                format!("_Atomic {}* {}", c_atomic_leaf(element)?, name)
            }
            Type::Mutex(_) => format!("intent_mutex_i64* {}", name),
            Type::Guard(_) => format!("const intent_guard_i64* {}", name),
            Type::Channel(element, capacity) => format!(
                // Channel refs also drop `const` — the
                // shared `intent_channel_<T>_<N>_send` and
                // `_recv` helpers take a mutable pointer
                // (they bump the per-slot seq counters and
                // read/write idx atomically). The send/recv
                // operations are concurrency-safe through
                // atomic loads/stores, not C `const`.
                // Closure #176 — caught via
                // -Wdiscarded-qualifiers on the
                // concurrency example.
                "{}* {}",
                crate::backend_c::c_channel_storage(element, *capacity),
                name
            ),
            other => format!("const {}* {}", c_type(other)?, name),
        },
        Type::RefMut(inner) => match &**inner {
            Type::Array { element, .. } => {
                format!("{}* {}", c_type(element)?, name)
            }
            Type::Vec(element) => {
                format!("{}* {}", crate::backend_c::vec_c_struct(element), name)
            }
            Type::Atomic(element) => {
                format!("_Atomic {}* {}", c_atomic_leaf(element)?, name)
            }
            Type::Mutex(_) => format!("intent_mutex_i64* {}", name),
            Type::Guard(_) => format!("intent_guard_i64* {}", name),
            Type::Channel(element, capacity) => format!(
                "{}* {}",
                crate::backend_c::c_channel_storage(element, *capacity),
                name
            ),
            other => format!("{}* {}", c_type(other)?, name),
        },
        _ => format!("{} {}", c_type(ty)?, name),
    })
}

fn c_const(c: &Const) -> String {
    match c {
        Const::Int(v) => format!("(int64_t){}LL", v),
        Const::Bool(true) => "true".to_string(),
        Const::Bool(false) => "false".to_string(),
        Const::Float(v) => format!("{}", v),
    }
}

fn c_operand(op: &Operand) -> String {
    match op {
        Operand::Value(v) => format!("v_{}", v.0),
        Operand::Const(c) => c_const(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile;
    use crate::ssa::lower_program;

    fn lower_and_emit(src: &str) -> String {
        let checked = compile(src).expect("source compiles");
        let (module, errors) = lower_program(&checked.ir);
        assert!(errors.is_empty(), "ssa lower errors: {:?}", errors);
        emit(&module).expect("ssa-c emit succeeds")
    }

    #[test]
    fn parallel_for_emit_scaffolding_recognizes_canonical_shape() {
        // SSA-C parallel-for emit machinery is in place but
        // gated off in `main.rs` until two integration gaps
        // close: `min`/`max` intrinsics and source-binding-
        // name mapping for OpenMP reduction vars. Drive the
        // emit DIRECTLY here so the recognizer + structured
        // emit code doesn't bit-rot while sitting behind
        // the gate.
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let total: i64 = 0;
              parallel for i from 0 to 4
              reduce total with +;
              {
                total = total + xs[i];
              }
              return total;
            }
        "#;
        let c = lower_and_emit(src);
        // Pragma with `+:` reduction clause appears.
        assert!(
            c.contains("_Pragma(\"omp parallel for reduction(+: v_"),
            "expected OpenMP reduction pragma in SSA-C output:\n{}",
            c
        );
        // Structured for-loop (no leading type — uses the
        // forward-declared counter slot).
        let has_for = c.lines().any(|l| {
            let t = l.trim();
            t.starts_with("for (v_") && t.contains("++) {")
        });
        assert!(
            has_for,
            "expected structured for-loop in SSA-C output:\n{}",
            c
        );
        // Reduction rebind at body bottom: `v_<carry> =
        // v_<update>;` ensures OpenMP's reduction tracking
        // picks up each iteration's update.
        let rebind = c.lines().any(|l| {
            let t = l.trim();
            t.starts_with("v_") && t.contains(" = v_") && t.ends_with(';')
        });
        assert!(rebind, "expected reduction rebind line:\n{}", c);
    }

    #[test]
    fn emits_scalar_function_with_main_shim() {
        let src = "fn main() -> i64 { return 42; }";
        let c = lower_and_emit(src);
        assert!(c.contains("int64_t fn_main("), "missing fn_main:\n{}", c);
        assert!(c.contains("int main(void)"), "missing main shim:\n{}", c);
        // The Const(42) materializes to `v_N = (int64_t)42LL;`
        // and the return threads through that. Check for the
        // literal value in the source.
        assert!(c.contains("42LL"), "missing literal 42:\n{}", c);
    }

    #[test]
    fn arithmetic_lowers_to_c_operators() {
        let src = "fn main() -> i64 { let a: i64 = 7; let b: i64 = 3; return a + b * 2; }";
        let c = lower_and_emit(src);
        assert!(c.contains("*"), "expected `*` operator in:\n{}", c);
        assert!(c.contains("+"), "expected `+` operator in:\n{}", c);
    }

    #[test]
    fn if_else_branches_emit_goto_labels() {
        let src = r#"
            fn main() -> i64 {
              if 1 < 2 { return 1; } else { return 0; }
            }
        "#;
        let c = lower_and_emit(src);
        assert!(c.contains("goto bb"), "expected goto in:\n{}", c);
        assert!(c.contains("if ("), "expected if in:\n{}", c);
    }

    #[test]
    fn while_loop_emits_header_with_back_jump() {
        let src = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              while n < 5 { n = n + 1; }
              return n;
            }
        "#;
        let c = lower_and_emit(src);
        // The header block should be visited via labels, and
        // the body should goto back. Count goto sites — we
        // expect at least two (header entry + back-jump).
        let gotos = c.matches("goto bb").count();
        assert!(gotos >= 2, "expected >= 2 gotos for a while loop:\n{}", c);
    }
}
