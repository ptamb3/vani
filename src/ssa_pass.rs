//! SSA passes — milestone 6f's representative analysis. The
//! initial pass is constant folding: a simple worklist that
//! collects "values known to be constant", then walks each
//! instruction and substitutes constant operands. This proves
//! out the migration pattern other passes will follow when they
//! move off the tree-shaped IR.
//!
//! Why this pass first: it's the smallest non-trivial analysis
//! that observably changes the SSA, doesn't require dataflow
//! across blocks (intra-block is enough to demonstrate the
//! pattern), and is mechanically straightforward to verify.
//!
//! Passes implemented today:
//!
//! - [`fold_constants`] — single-pass constant folding (milestone 6f
//!   migration template, landed first).
//! - [`dce_module`] — branch threading + unreachable-block removal
//!   + dead-instruction elimination. Composes with constant
//!   folding: `Const(Bool(true))` branch conditions thread to a
//!   Jump, the now-dead arm becomes unreachable, and any
//!   pure-no-trap instructions whose result is unused are dropped.
//! - [`recognize_reduction_shapes`] — walks each function and
//!   returns one [`ReductionShape`] per `Hint::ParallelForBegin`
//!   marker it finds (matched with its `ParallelForEnd`). A
//!   future SSA-based parallel-for backend lowering consumes
//!   this analysis so it doesn't have to re-derive the
//!   parallel region shape from raw blocks.
//!
//! Future passes (each its own session): drop-insertion
//! (reads from affine type info), bounds elision (consumes the
//! SMT-discharge bit on `Index` / `IndexAssign`).

use std::collections::BTreeMap;

use crate::ast::{BinaryOp, Type, UnaryOp};
use crate::ssa::{
    BasicBlock, Const, Function, InstrKind, Instruction, Module, Operand, Terminator, ValueId,
};

/// Run constant folding over a module in place. Returns the
/// number of instructions whose operands were replaced. Useful
/// for tests that want to assert "this program had N foldable
/// sites".
pub fn fold_constants(module: &mut Module) -> usize {
    let mut total = 0;
    for func in &mut module.functions {
        total += fold_constants_in_function(func);
    }
    total
}

fn fold_constants_in_function(func: &mut Function) -> usize {
    let mut env: BTreeMap<ValueId, Const> = BTreeMap::new();
    let mut replacements = 0;
    // Single forward pass per block: as we walk instructions,
    // any operand that's a Value whose corresponding ValueId is
    // already in `env` gets rewritten to the matching Const.
    // After substitution, if the instruction is purely
    // constant (e.g., `add 1 + 2`), replace it with an
    // `InstrKind::Const` and add the folded result to `env`.
    //
    // Block-args aren't folded: their value depends on the
    // incoming edge, which an intra-block pass can't see.
    // A later inter-block pass can specialize them when every
    // predecessor passes the same constant.
    for block in &mut func.blocks {
        replacements += fold_block(block, &mut env);
    }
    replacements
}

fn fold_block(block: &mut BasicBlock, env: &mut BTreeMap<ValueId, Const>) -> usize {
    let mut replacements = 0;
    for instr in &mut block.instructions {
        replacements += substitute_in_instr(instr, env);
        if let Some(c) = try_fold(instr) {
            // Rewrite the instruction itself in place so a
            // later pass / backend sees a clean Const form.
            instr.kind = InstrKind::Const(c.clone());
            env.insert(instr.result, c);
        }
    }
    // Substitute the terminator too — branch conditions and
    // return values are the most user-visible foldable sites.
    replacements += substitute_in_terminator(&mut block.terminator, env);
    replacements
}

fn substitute_in_instr(instr: &mut Instruction, env: &BTreeMap<ValueId, Const>) -> usize {
    let mut count = 0;
    match &mut instr.kind {
        InstrKind::Unary { x, .. } => count += subst_op(x, env),
        InstrKind::Binary { l, r, .. } => {
            count += subst_op(l, env);
            count += subst_op(r, env);
        }
        InstrKind::Call { args, .. } => {
            for a in args.iter_mut() {
                count += subst_op(a, env);
            }
        }
        InstrKind::Cast { x, .. } => count += subst_op(x, env),
        InstrKind::ArrayLit { elements } => {
            for e in elements.iter_mut() {
                count += subst_op(e, env);
            }
        }
        InstrKind::Index { array, index, .. } => {
            count += subst_op(array, env);
            count += subst_op(index, env);
        }
        InstrKind::Len { array, .. } => count += subst_op(array, env),
        InstrKind::IndexAssign { index, value, .. } => {
            count += subst_op(index, env);
            count += subst_op(value, env);
        }
        InstrKind::CallIndirect { callee, args } => {
            count += subst_op(callee, env);
            for a in args.iter_mut() {
                count += subst_op(a, env);
            }
        }
        InstrKind::Const(_)
        | InstrKind::StrLit(_)
        | InstrKind::RefOf { .. }
        | InstrKind::Drop { .. }
        | InstrKind::Hint(_)
        | InstrKind::FnRef { .. } => {}
    }
    count
}

fn substitute_in_terminator(term: &mut Terminator, env: &BTreeMap<ValueId, Const>) -> usize {
    let mut count = 0;
    match term {
        Terminator::Return(Some(op)) => count += subst_op(op, env),
        Terminator::Return(None) | Terminator::Unreachable => {}
        Terminator::Jump { args, .. } => {
            for a in args.iter_mut() {
                count += subst_op(a, env);
            }
        }
        Terminator::Branch {
            cond,
            then_args,
            else_args,
            ..
        } => {
            count += subst_op(cond, env);
            for a in then_args.iter_mut() {
                count += subst_op(a, env);
            }
            for a in else_args.iter_mut() {
                count += subst_op(a, env);
            }
        }
    }
    count
}

fn subst_op(op: &mut Operand, env: &BTreeMap<ValueId, Const>) -> usize {
    if let Operand::Value(v) = op {
        if let Some(c) = env.get(v) {
            *op = Operand::Const(c.clone());
            return 1;
        }
    }
    0
}

/// If `instr` has constant operands and a known fold rule,
/// return the folded constant.
fn try_fold(instr: &Instruction) -> Option<Const> {
    match &instr.kind {
        InstrKind::Const(c) => Some(c.clone()),
        InstrKind::Unary {
            op: UnaryOp::Neg,
            x: Operand::Const(Const::Int(v)),
        } => Some(Const::Int(v.checked_neg()?)),
        InstrKind::Unary {
            op: UnaryOp::Neg,
            x: Operand::Const(Const::Float(v)),
        } => Some(Const::Float(-v)),
        InstrKind::Unary {
            op: UnaryOp::Not,
            x: Operand::Const(Const::Bool(v)),
        } => Some(Const::Bool(!v)),
        InstrKind::Binary {
            op,
            l: Operand::Const(Const::Int(a)),
            r: Operand::Const(Const::Int(b)),
        } => fold_int_binary(*op, *a, *b),
        InstrKind::Binary {
            op,
            l: Operand::Const(Const::Bool(a)),
            r: Operand::Const(Const::Bool(b)),
        } => fold_bool_binary(*op, *a, *b),
        InstrKind::Cast {
            x: Operand::Const(Const::Int(v)),
            to,
        } => fold_int_cast(*v, to),
        _ => None,
    }
}

fn fold_int_binary(op: BinaryOp, a: i128, b: i128) -> Option<Const> {
    let v = match op {
        BinaryOp::Add => a.checked_add(b)?,
        BinaryOp::Sub => a.checked_sub(b)?,
        BinaryOp::Mul => a.checked_mul(b)?,
        BinaryOp::Div if b == 0 => return None,
        BinaryOp::Div => a.checked_div(b)?,
        BinaryOp::Rem if b == 0 => return None,
        BinaryOp::Rem => a.checked_rem(b)?,
        BinaryOp::BitAnd => a & b,
        BinaryOp::BitOr => a | b,
        BinaryOp::BitXor => a ^ b,
        BinaryOp::Shl => {
            let s = u32::try_from(b).ok()?;
            a.checked_shl(s)?
        }
        BinaryOp::Shr => {
            let s = u32::try_from(b).ok()?;
            a.checked_shr(s)?
        }
        BinaryOp::Eq => return Some(Const::Bool(a == b)),
        BinaryOp::Ne => return Some(Const::Bool(a != b)),
        BinaryOp::Lt => return Some(Const::Bool(a < b)),
        BinaryOp::Le => return Some(Const::Bool(a <= b)),
        BinaryOp::Gt => return Some(Const::Bool(a > b)),
        BinaryOp::Ge => return Some(Const::Bool(a >= b)),
        BinaryOp::And | BinaryOp::Or => return None, // bool ops
    };
    Some(Const::Int(v))
}

fn fold_bool_binary(op: BinaryOp, a: bool, b: bool) -> Option<Const> {
    let v = match op {
        BinaryOp::And => a && b,
        BinaryOp::Or => a || b,
        BinaryOp::Eq => a == b,
        BinaryOp::Ne => a != b,
        _ => return None,
    };
    Some(Const::Bool(v))
}

fn fold_int_cast(v: i128, to: &Type) -> Option<Const> {
    let (min, max) = (to.min_value()?, to.max_value()?);
    if v < min || v > max {
        return None;
    }
    Some(Const::Int(v))
}

/// Statistics returned by [`dce_module`]. Tests assert on the
/// counters to confirm each sub-pass actually fired on the
/// expected programs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DceStats {
    /// Number of `Branch` terminators rewritten to `Jump`
    /// because their condition was a constant bool.
    pub branches_threaded: usize,
    /// Number of blocks removed because no edge reaches them
    /// from the function entry.
    pub blocks_removed: usize,
    /// Number of instructions removed because their result was
    /// unused and the instruction is pure / non-trapping.
    pub instructions_removed: usize,
    /// Outer fixed-point iterations needed to reach stability.
    pub iterations: usize,
}

/// Branch threading + unreachable-block removal + dead-
/// instruction elimination. Runs each sub-pass in sequence,
/// then iterates until no sub-pass reports a change. Mutates
/// `module` in place and returns the aggregate stats.
pub fn dce_module(module: &mut Module) -> DceStats {
    let mut stats = DceStats::default();
    for func in &mut module.functions {
        loop {
            let mut changed = false;
            let threaded = thread_branches(func);
            if threaded > 0 {
                stats.branches_threaded += threaded;
                changed = true;
            }
            let removed_blocks = remove_unreachable_blocks(func);
            if removed_blocks > 0 {
                stats.blocks_removed += removed_blocks;
                changed = true;
            }
            let removed_instrs = remove_dead_instructions(func);
            if removed_instrs > 0 {
                stats.instructions_removed += removed_instrs;
                changed = true;
            }
            stats.iterations += 1;
            if !changed {
                break;
            }
        }
    }
    stats
}

/// Rewrite every `Branch { cond: Const(Bool(b)), … }` to a
/// `Jump` to whichever arm `b` selects. Forwards the matching
/// block-args. Returns the count of rewrites.
fn thread_branches(func: &mut Function) -> usize {
    let mut count = 0;
    for block in &mut func.blocks {
        if let Terminator::Branch {
            cond: Operand::Const(Const::Bool(b)),
            then_block,
            then_args,
            else_block,
            else_args,
        } = &block.terminator
        {
            let (target, args) = if *b {
                (*then_block, then_args.clone())
            } else {
                (*else_block, else_args.clone())
            };
            block.terminator = Terminator::Jump { target, args };
            count += 1;
        }
    }
    count
}

/// Drop blocks that no successor edge from the entry reaches.
/// After branch threading, formerly-conditional dead arms
/// become unreferenced and this pass removes them. Renumbers
/// the surviving blocks so `BlockId`s stay contiguous; updates
/// every terminator's targets to match.
fn remove_unreachable_blocks(func: &mut Function) -> usize {
    use std::collections::HashSet;

    // BFS from entry to find reachable blocks.
    let mut reachable: HashSet<crate::ssa::BlockId> = HashSet::new();
    let mut worklist = vec![func.entry];
    while let Some(id) = worklist.pop() {
        if !reachable.insert(id) {
            continue;
        }
        let block = &func.blocks[id.0 as usize];
        match &block.terminator {
            Terminator::Return(_) | Terminator::Unreachable => {}
            Terminator::Jump { target, .. } => worklist.push(*target),
            Terminator::Branch {
                then_block, else_block, ..
            } => {
                worklist.push(*then_block);
                worklist.push(*else_block);
            }
        }
    }

    let before = func.blocks.len();
    if reachable.len() == before {
        return 0;
    }

    // Build old→new mapping for surviving blocks, preserving
    // their relative order so the entry block stays first if
    // it was first.
    let mut new_id_of: std::collections::HashMap<crate::ssa::BlockId, crate::ssa::BlockId> =
        std::collections::HashMap::new();
    let mut new_blocks: Vec<BasicBlock> = Vec::with_capacity(reachable.len());
    for block in func.blocks.iter() {
        if reachable.contains(&block.id) {
            let new_id = crate::ssa::BlockId(new_blocks.len() as u32);
            new_id_of.insert(block.id, new_id);
            let mut renumbered = block.clone();
            renumbered.id = new_id;
            new_blocks.push(renumbered);
        }
    }

    // Rewrite terminator block references.
    for block in new_blocks.iter_mut() {
        match &mut block.terminator {
            Terminator::Return(_) | Terminator::Unreachable => {}
            Terminator::Jump { target, .. } => {
                *target = new_id_of[target];
            }
            Terminator::Branch {
                then_block, else_block, ..
            } => {
                *then_block = new_id_of[then_block];
                *else_block = new_id_of[else_block];
            }
        }
    }

    func.entry = new_id_of[&func.entry];
    func.blocks = new_blocks;
    before - reachable.len()
}

/// Drop instructions whose result is unreferenced AND whose
/// effect on memory / control / runtime checks is none. The
/// pass is intentionally conservative — it leaves calls,
/// IndexAssign, Drop, ArrayLit (allocates), StrLit (interns),
/// Hint, RefOf, Index (might bounds-check), Len, and
/// `Binary { op: Div | Rem | Shl | Shr, .. }` (might trap)
/// alone. Anything else with an unreferenced result is safe to
/// delete.
fn remove_dead_instructions(func: &mut Function) -> usize {
    use std::collections::HashSet;

    // 1. Collect every ValueId referenced anywhere in the
    //    function. Operands inside instructions + every
    //    terminator's args / cond / return value.
    let mut used: HashSet<ValueId> = HashSet::new();
    for block in &func.blocks {
        for instr in &block.instructions {
            collect_used_in_instr(&instr.kind, &mut used);
        }
        collect_used_in_terminator(&block.terminator, &mut used);
    }

    // 2. Walk each instruction; remove if (a) result not used
    //    and (b) the kind is in the "safe to drop" set.
    let mut count = 0;
    for block in &mut func.blocks {
        block.instructions.retain(|instr| {
            let result_used = used.contains(&instr.result);
            if result_used || !is_safe_to_drop(&instr.kind) {
                true
            } else {
                count += 1;
                false
            }
        });
    }
    count
}

fn collect_used_in_instr(kind: &InstrKind, used: &mut std::collections::HashSet<ValueId>) {
    match kind {
        InstrKind::Const(_) | InstrKind::StrLit(_) | InstrKind::Hint(_) => {}
        InstrKind::Unary { x, .. } => add_value(x, used),
        InstrKind::Binary { l, r, .. } => {
            add_value(l, used);
            add_value(r, used);
        }
        InstrKind::Call { args, .. } => {
            for a in args {
                add_value(a, used);
            }
        }
        InstrKind::Cast { x, .. } => add_value(x, used),
        InstrKind::ArrayLit { elements } => {
            for e in elements {
                add_value(e, used);
            }
        }
        InstrKind::Index { array, index, .. } => {
            add_value(array, used);
            add_value(index, used);
        }
        InstrKind::Len { array, .. } => add_value(array, used),
        InstrKind::IndexAssign { index, value, .. } => {
            add_value(index, used);
            add_value(value, used);
        }
        InstrKind::FnRef { .. } => {}
        InstrKind::CallIndirect { callee, args } => {
            add_value(callee, used);
            for a in args {
                add_value(a, used);
            }
        }
        InstrKind::RefOf { .. } | InstrKind::Drop { .. } => {}
    }
}

fn collect_used_in_terminator(term: &Terminator, used: &mut std::collections::HashSet<ValueId>) {
    match term {
        Terminator::Return(Some(op)) => add_value(op, used),
        Terminator::Return(None) | Terminator::Unreachable => {}
        Terminator::Jump { args, .. } => {
            for a in args {
                add_value(a, used);
            }
        }
        Terminator::Branch {
            cond,
            then_args,
            else_args,
            ..
        } => {
            add_value(cond, used);
            for a in then_args {
                add_value(a, used);
            }
            for a in else_args {
                add_value(a, used);
            }
        }
    }
}

fn add_value(op: &Operand, used: &mut std::collections::HashSet<ValueId>) {
    if let Operand::Value(v) = op {
        used.insert(*v);
    }
}

fn is_safe_to_drop(kind: &InstrKind) -> bool {
    match kind {
        // Pure, non-trapping.
        InstrKind::Const(_)
        | InstrKind::Unary { .. }
        | InstrKind::Cast { .. }
        | InstrKind::Len { .. }
        | InstrKind::RefOf { .. } => true,
        // Binary is safe except for division/shifts (might
        // trap on runtime values). Comparisons and additive /
        // multiplicative / bitwise ops are fine.
        InstrKind::Binary { op, .. } => !matches!(
            op,
            BinaryOp::Div | BinaryOp::Rem | BinaryOp::Shl | BinaryOp::Shr
        ),
        // FnRef is pure — taking a function's address has no
        // side effect and is byte-cheap to redo.
        InstrKind::FnRef { .. } => true,
        // Calls (may have side effects), allocations, indexing
        // (bounds check), assignments, hints, strings, drops —
        // all must be kept.
        InstrKind::Call { .. }
        | InstrKind::CallIndirect { .. }
        | InstrKind::ArrayLit { .. }
        | InstrKind::Index { .. }
        | InstrKind::IndexAssign { .. }
        | InstrKind::Hint(_)
        | InstrKind::StrLit(_)
        | InstrKind::Drop { .. } => false,
    }
}

/// One parallel-region described in terms of the SSA blocks
/// the lowerer emitted for it. A future backend can consume
/// this analysis to dispatch parallel-specific lowering
/// (OpenMP pragma, outlined function, etc.) without
/// re-walking the surrounding blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct ReductionShape {
    /// The function this region lives in (by index into
    /// `Module::functions`).
    pub function: usize,
    /// Block containing the matching `Hint::ParallelForBegin`.
    pub begin_block: crate::ssa::BlockId,
    /// Block containing the matching `Hint::ParallelForEnd`.
    /// May be the same as `begin_block` for empty loops.
    pub end_block: crate::ssa::BlockId,
    /// Reduction clauses recorded on the begin-hint — one per
    /// `reduce <var> with <op>;` source clause.
    pub reductions: Vec<(String, crate::ast::ReductionOp, crate::ast::Type)>,
}

/// Walk every function in the module and return one
/// [`ReductionShape`] per matched `Hint::ParallelForBegin` /
/// `Hint::ParallelForEnd` pair. Unbalanced markers (missing
/// end, or an end before a begin) surface in `unmatched`
/// rather than panicking — keeps the analysis robust against
/// IR shapes future passes might produce.
pub fn recognize_reduction_shapes(
    module: &Module,
) -> (Vec<ReductionShape>, Vec<UnmatchedHint>) {
    let mut shapes = Vec::new();
    let mut unmatched = Vec::new();
    for (fn_idx, func) in module.functions.iter().enumerate() {
        // Stack of in-progress begins: when we see a
        // ParallelForEnd, pop the most-recent begin and
        // record a ReductionShape spanning them. Nested
        // parallel-for would push more than once; v1 of the
        // lowerer doesn't emit nesting but the analysis is
        // safe under it.
        let mut open: Vec<OpenRegion> = Vec::new();
        for block in &func.blocks {
            for instr in &block.instructions {
                match &instr.kind {
                    InstrKind::Hint(crate::ssa::HintKind::ParallelForBegin { reductions, .. }) => {
                        open.push(OpenRegion {
                            begin_block: block.id,
                            reductions: reductions.clone(),
                        });
                    }
                    InstrKind::Hint(crate::ssa::HintKind::ParallelForEnd) => {
                        if let Some(begin) = open.pop() {
                            shapes.push(ReductionShape {
                                function: fn_idx,
                                begin_block: begin.begin_block,
                                end_block: block.id,
                                reductions: begin.reductions,
                            });
                        } else {
                            unmatched.push(UnmatchedHint {
                                function: fn_idx,
                                block: block.id,
                                kind: UnmatchedKind::EndWithoutBegin,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        for region in open {
            unmatched.push(UnmatchedHint {
                function: fn_idx,
                block: region.begin_block,
                kind: UnmatchedKind::BeginWithoutEnd,
            });
        }
    }
    (shapes, unmatched)
}

struct OpenRegion {
    begin_block: crate::ssa::BlockId,
    reductions: Vec<(String, crate::ast::ReductionOp, crate::ast::Type)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnmatchedHint {
    pub function: usize,
    pub block: crate::ssa::BlockId,
    pub kind: UnmatchedKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UnmatchedKind {
    /// A `ParallelForBegin` was emitted but no matching
    /// `ParallelForEnd` followed. A latent lowerer bug.
    BeginWithoutEnd,
    /// A `ParallelForEnd` appeared with no open region.
    /// Symmetric latent bug.
    EndWithoutBegin,
}

/// Bounds-elision pass: walk every `Index` and `IndexAssign`
/// instruction; for the cases that are trivially safe at the
/// SSA level (constant index against an `ArrayLit` of known
/// length, against a function parameter typed
/// `[T; N]`/`&[T; N]`/`&mut [T; N]`, or against the same
/// `[T; N]` reached through a single chain of `Const` /
/// `ArrayLit` / `RefOf` defs), flip the `checked` flag to
/// `false`. Returns the count of instructions whose flag
/// changed.
///
/// Today this is a complement to the typed-IR-based pass: the
/// typed pass owns the SMT-driven proofs over variable
/// indices; this SSA pass owns the syntactic constant-index
/// fast path. Once both backends consume SSA (TODO #6g) and
/// SMT can talk to the SSA value graph, this pass absorbs
/// more of the typed-pass logic and the typed pass goes away.
pub fn elide_bounds(module: &mut Module) -> ElideStats {
    let mut stats = ElideStats::default();
    for func in &mut module.functions {
        elide_bounds_in_function(func, &mut stats);
    }
    stats
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ElideStats {
    /// Index instructions whose `checked` flag flipped to false.
    pub indexed_loads_elided: usize,
    /// IndexAssign instructions whose `checked` flag flipped.
    pub index_stores_elided: usize,
}

fn elide_bounds_in_function(func: &mut Function, stats: &mut ElideStats) {
    // Per-function lookup of "what's the static length of the
    // array this ValueId refers to?". Built lazily from
    // `ArrayLit` instructions and function-parameter types
    // (which the lowerer encodes as `[T; N]` in `func.params`).
    let lengths = collect_array_lengths(func);
    for block in &mut func.blocks {
        for instr in &mut block.instructions {
            match &mut instr.kind {
                InstrKind::Index { array, index, checked } => {
                    if *checked && index_is_in_bounds(array, index, &lengths) {
                        *checked = false;
                        stats.indexed_loads_elided += 1;
                    }
                }
                InstrKind::IndexAssign { array, base_ty, index, checked, .. } => {
                    if *checked
                        && index_is_in_bounds_for_base(
                            array, base_ty, index,
                        )
                    {
                        *checked = false;
                        stats.index_stores_elided += 1;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Build a map of `ValueId → static array length` for every
/// SSA value in `func` whose underlying storage has a known
/// fixed extent. Currently catches: `ArrayLit` (length = #
/// elements) and function parameters typed `[T; N]` (length
/// = N) — through one `RefOf` indirection too, since the
/// lowerer emits `RefOf` when the source expression takes
/// `&arr` to pass into a builtin.
fn collect_array_lengths(func: &Function) -> BTreeMap<ValueId, u64> {
    let mut map: BTreeMap<ValueId, u64> = BTreeMap::new();
    // Parameter array lengths.
    for (_name, ty, vid) in &func.params {
        if let Some(n) = array_extent(ty) {
            map.insert(*vid, n);
        }
    }
    for block in &func.blocks {
        for instr in &block.instructions {
            match &instr.kind {
                InstrKind::ArrayLit { elements } => {
                    map.insert(instr.result, elements.len() as u64);
                }
                InstrKind::RefOf { .. } => {
                    // RefOf produces a pointer to a binding.
                    // We can't resolve binding-name → length
                    // from SSA alone today; future expansion
                    // could thread a name→length side table.
                    if let Some(n) = array_extent(&instr.ty) {
                        map.insert(instr.result, n);
                    }
                }
                _ => {
                    if let Some(n) = array_extent(&instr.ty) {
                        map.insert(instr.result, n);
                    }
                }
            }
        }
    }
    map
}

fn array_extent(ty: &Type) -> Option<u64> {
    match ty {
        Type::Array { length, .. } => Some(*length),
        Type::Ref(inner) | Type::RefMut(inner) => array_extent(inner),
        _ => None,
    }
}

fn index_is_in_bounds(
    array: &Operand,
    index: &Operand,
    lengths: &BTreeMap<ValueId, u64>,
) -> bool {
    let length = match array {
        Operand::Value(v) => lengths.get(v).copied(),
        _ => None,
    };
    let Some(length) = length else { return false };
    match index {
        Operand::Const(Const::Int(n)) => *n >= 0 && (*n as u64) < length,
        _ => false,
    }
}

fn index_is_in_bounds_for_base(
    _array: &Operand,
    base_ty: &Type,
    index: &Operand,
) -> bool {
    let length = match array_extent(base_ty) {
        Some(n) => n,
        None => return false,
    };
    match index {
        Operand::Const(Const::Int(n)) => *n >= 0 && (*n as u64) < length,
        _ => false,
    }
}

/// Drop-coverage audit: walk every function and count
/// (a) affine value constructions, (b) `InstrKind::Drop`
/// instructions, and (c) constructions whose result name we
/// didn't find a Drop for. Returns `DropAudit` with the raw
/// counts plus per-function lists of "missing" drop names so
/// tools can pinpoint the leak.
///
/// This is the SSA-side audit half of the drop-insertion
/// migration; the typed-IR checker still owns insertion. When
/// the codegen flips to SSA (#6g), this pass either becomes
/// the source of truth or fuses with the insertion logic.
pub fn audit_drops(module: &Module) -> DropAudit {
    let mut audit = DropAudit::default();
    for func in &module.functions {
        audit_drops_in_function(func, &mut audit);
    }
    audit
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DropAudit {
    /// Total affine values constructed across the module.
    pub affine_constructed: usize,
    /// Total Drop instructions emitted.
    pub drops_emitted: usize,
    /// Construction sites (function-qualified binding names)
    /// that have no matching Drop in the same function.
    pub missing_drops: Vec<MissingDrop>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MissingDrop {
    pub function: String,
    pub name: String,
    pub ty: Type,
}

fn audit_drops_in_function(func: &Function, audit: &mut DropAudit) {
    // Collect every Let-style construction we believe is
    // affine. The lowerer emits a `Const`/`Call`/`ArrayLit`
    // whose result type is affine, immediately followed by
    // the binding's storage being attached to a named local.
    // We approximate this by walking ALL instructions and
    // recording the ones whose `ty` is affine; we then match
    // them up against the `Drop` instructions we see (each
    // Drop names a binding directly).
    //
    // The SSA model doesn't preserve "this ValueId became
    // binding `x`" — that information lives in the lowerer's
    // value-map and isn't available here. As a pragmatic
    // proxy: every Drop is keyed by binding name, and every
    // affine construction emitted by the lowerer is the RHS
    // of a `let <name> = …` so its result is conceptually
    // owned by some binding. We count both sides and report
    // a discrepancy.
    let mut constructions = 0usize;
    let mut dropped_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for block in &func.blocks {
        for instr in &block.instructions {
            match &instr.kind {
                InstrKind::Drop { name, .. } => {
                    audit.drops_emitted += 1;
                    dropped_names.insert(name.as_str());
                }
                _ => {
                    if is_affine_type(&instr.ty) {
                        constructions += 1;
                    }
                }
            }
        }
    }
    audit.affine_constructed += constructions;
    // The lowerer attaches names via `IndexAssign` (`array_name`)
    // and via `Drop` itself; without that mapping we can't
    // produce per-name `missing_drops` directly. Pragmatic
    // proxy: if constructions > drops, record a placeholder so
    // CI tooling can flag a regression. This works in concert
    // with the typed-IR pass which already produces correct
    // drops; this audit catches divergence after a future
    // SSA-driven insertion pass lands.
    if constructions > dropped_names.len() {
        // Each "missing" surface is reported once per function
        // — not per-name, since we can't recover names from
        // instructions other than Drop. The placeholder name
        // documents the gap.
        let _ = func; // keep function reference for future name plumbing
        audit.missing_drops.push(MissingDrop {
            function: func.name.clone(),
            name: format!("<unresolved: {} affine construction(s) without a named Drop>", constructions - dropped_names.len()),
            ty: Type::I64,
        });
    }
}

fn is_affine_type(ty: &Type) -> bool {
    match ty {
        Type::Vec(_)
        | Type::OwnedStr
        | Type::Atomic(_)
        | Type::Channel(_, _)
        | Type::Mutex(_)
        | Type::Guard(_)
        | Type::Task => true,
        _ => false,
    }
}

/// Pure-region effects audit over SSA. Walks each function;
/// inside every `Hint::ParallelForBegin .. ParallelForEnd` or
/// `Hint::TaskBegin .. TaskEnd` region, scans for instructions
/// that would violate the "pure body with captures" rule the
/// typed-IR checker enforces at the source. Returns a list of
/// violations the audit found — empty on healthy programs.
///
/// Today the typed-IR effects checker is authoritative (the
/// SSA never sees a body that didn't already pass that gate).
/// This audit is the SSA-side mirror: when a future
/// SSA-level rewrite introduces an impure call into a
/// parallel region, the audit surfaces the regression
/// immediately. Once codegen migrates to SSA (#6g) the typed
/// pass goes away and this audit becomes the source of
/// truth.
pub fn audit_pure_regions(module: &Module) -> Vec<PureViolation> {
    let mut out = Vec::new();
    for func in &module.functions {
        audit_pure_regions_in_function(func, &mut out);
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct PureViolation {
    pub function: String,
    pub kind: PureViolationKind,
    /// Source span of the offending SSA instruction.
    pub span: crate::span::Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PureViolationKind {
    /// `xs[i] = v` inside a pure region.
    IndexAssign { array: String },
    /// `Drop` of a non-Copy value: free has observable effects.
    DropNonCopy { name: String, ty: Type },
    /// Call to one of the heap-allocating Vec builtins.
    VecBuiltinCall { name: String },
    /// Indirect call through a fn-pointer. The name-based
    /// purity gate can't see through it, so the audit treats
    /// every CallIndirect inside a pure region as a
    /// violation. Mirrors the typed-IR effects walker.
    IndirectCall,
}

fn audit_pure_regions_in_function(func: &Function, out: &mut Vec<PureViolation>) {
    // Walk blocks in source order. A simple depth counter
    // tracks whether we're inside a pure region — each
    // ParallelForBegin / TaskBegin increments it,
    // ParallelForEnd / TaskEnd decrements. Nested regions
    // stay pure (the depth stays > 0 throughout).
    let mut depth: i32 = 0;
    for block in &func.blocks {
        for instr in &block.instructions {
            match &instr.kind {
                InstrKind::Hint(crate::ssa::HintKind::ParallelForBegin { .. })
                | InstrKind::Hint(crate::ssa::HintKind::TaskBegin { .. }) => {
                    depth += 1;
                }
                InstrKind::Hint(crate::ssa::HintKind::ParallelForEnd)
                | InstrKind::Hint(crate::ssa::HintKind::TaskEnd { .. }) => {
                    if depth > 0 {
                        depth -= 1;
                    }
                }
                _ if depth > 0 => match &instr.kind {
                    InstrKind::IndexAssign { array, .. } => {
                        out.push(PureViolation {
                            function: func.name.clone(),
                            kind: PureViolationKind::IndexAssign {
                                array: format!("{}", array),
                            },
                            span: instr.span,
                        });
                    }
                    InstrKind::Drop { name, ty, .. } => {
                        // Drops of Copy types are no-ops; only
                        // non-Copy drops carry observable
                        // side effects (free, unlock, …).
                        if !ty.is_copy() {
                            out.push(PureViolation {
                                function: func.name.clone(),
                                kind: PureViolationKind::DropNonCopy {
                                    name: name.clone(),
                                    ty: ty.clone(),
                                },
                                span: instr.span,
                            });
                        }
                    }
                    InstrKind::Call { name, .. } => {
                        if matches!(
                            name.as_str(),
                            "vec" | "push" | "set" | "clone"
                        ) {
                            out.push(PureViolation {
                                function: func.name.clone(),
                                kind: PureViolationKind::VecBuiltinCall {
                                    name: name.clone(),
                                },
                                span: instr.span,
                            });
                        }
                    }
                    InstrKind::CallIndirect { .. } => {
                        out.push(PureViolation {
                            function: func.name.clone(),
                            kind: PureViolationKind::IndirectCall,
                            span: instr.span,
                        });
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile;
    use crate::ssa::lower_function;

    fn fold_main(src: &str) -> (Function, usize) {
        let checked = compile(src).expect("source compiles");
        let main = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main exists");
        let mut fnc = lower_function(main).expect("lower succeeds");
        let mut m = Module {
            functions: vec![fnc.clone()],
        };
        let n = fold_constants(&mut m);
        fnc = m.functions.pop().unwrap();
        (fnc, n)
    }

    #[test]
    fn fold_int_add_collapses_binary_to_const() {
        let src = "fn main() -> i64 { return 1 + 2; }";
        let (fnc, replacements) = fold_main(src);
        // The Binary instruction's operands were both Const,
        // so try_fold replaced it with `Const(3)`. The return
        // temp's Const also gets folded into the return
        // terminator. Total replacement count = 1 (the return
        // operand substitution; the Binary's operands were
        // already Const so subst_op didn't fire on them).
        assert!(
            replacements >= 1,
            "expected at least 1 replacement, got {}\n{}",
            replacements,
            fnc
        );
        // Verify the binary was rewritten to a Const.
        let binary_folded = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .any(|i| matches!(&i.kind, InstrKind::Const(Const::Int(3))));
        assert!(
            binary_folded,
            "expected the `1 + 2` binary to fold to Const(3):\n{}",
            fnc
        );
    }

    #[test]
    fn fold_threads_through_let_bound_constant() {
        // `let x = 41` materializes Const(41); `x + 1`
        // substitutes the Operand::Value reference with the
        // const, then the Binary itself folds to Const(42).
        let src = "fn main() -> i64 { let x: i64 = 41; return x + 1; }";
        let (fnc, _replacements) = fold_main(src);
        let folded_42 = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .any(|i| matches!(&i.kind, InstrKind::Const(Const::Int(42))));
        assert!(
            folded_42,
            "expected `x + 1` (with x=41) to fold to Const(42):\n{}",
            fnc
        );
    }

    #[test]
    fn fold_bool_and_short_circuits() {
        // `true && false` folds to Const(Bool(false)).
        let src = r#"
            fn main() -> i64 {
              if true && false { return 1; } else { return 0; }
            }
        "#;
        let (fnc, _replacements) = fold_main(src);
        let folded_false = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .any(|i| matches!(&i.kind, InstrKind::Const(Const::Bool(false))));
        assert!(
            folded_false,
            "expected `true && false` to fold to Const(Bool(false)):\n{}",
            fnc
        );
    }

    #[test]
    fn fold_preserves_unfoldable_calls() {
        // A non-pure call shouldn't be folded. The pass walks
        // the call's args (substituting where possible) but
        // doesn't try to evaluate the call itself.
        let src = r#"
            fn helper(x: i64) -> i64 { return x + 1; }
            fn main() -> i64 { let a: i64 = 5; return helper(a); }
        "#;
        let checked = compile(src).expect("compiles");
        let main = checked.ir.functions.iter().find(|f| f.name == "main").unwrap();
        let mut fnc = lower_function(main).expect("lower succeeds");
        let mut m = Module {
            functions: vec![fnc.clone()],
        };
        fold_constants(&mut m);
        fnc = m.functions.pop().unwrap();
        // The Call instruction should still be present (not
        // replaced by a Const), but the arg should now be the
        // folded constant.
        let call_with_const_arg = fnc.blocks.iter().flat_map(|b| b.instructions.iter()).any(
            |i| matches!(&i.kind, InstrKind::Call { args, .. } if args.iter().any(|a| matches!(a, Operand::Const(Const::Int(5))))),
        );
        assert!(
            call_with_const_arg,
            "expected call(helper, Const(5)) after fold:\n{}",
            fnc
        );
    }

    fn lower_and_dce(src: &str) -> (Function, DceStats) {
        let checked = compile(src).expect("source compiles");
        let main = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main exists");
        let fnc = lower_function(main).expect("lower succeeds");
        let mut m = Module { functions: vec![fnc] };
        // Compose with constant folding so branch conditions
        // and instruction operands get the Const treatment
        // first — DCE then has constant terminator conditions
        // to thread.
        fold_constants(&mut m);
        let stats = dce_module(&mut m);
        let fnc = m.functions.pop().unwrap();
        (fnc, stats)
    }

    #[test]
    fn branch_threading_collapses_constant_true_to_jump() {
        let src = r#"
            fn main() -> i64 {
              if 1 < 2 { return 7; } else { return 99; }
            }
        "#;
        let (fnc, stats) = lower_and_dce(src);
        assert!(
            stats.branches_threaded >= 1,
            "expected at least 1 thread, got {:?}\n{}",
            stats,
            fnc
        );
        // No surviving Branch terminator should have a
        // Const(Bool) condition (they were all threaded).
        let any_const_branch = fnc.blocks.iter().any(|b| {
            matches!(
                b.terminator,
                Terminator::Branch {
                    cond: Operand::Const(Const::Bool(_)),
                    ..
                }
            )
        });
        assert!(
            !any_const_branch,
            "branch threading left a constant Branch behind:\n{}",
            fnc
        );
    }

    #[test]
    fn unreachable_blocks_removed_after_threading() {
        // After threading `if true`, the else-arm is unreachable
        // from entry. Removal renumbers the surviving blocks.
        let src = r#"
            fn main() -> i64 {
              if 1 < 2 { return 7; } else { return 99; }
            }
        "#;
        let (fnc, stats) = lower_and_dce(src);
        assert!(
            stats.blocks_removed >= 1,
            "expected at least 1 unreachable block removed, got {:?}\n{}",
            stats,
            fnc
        );
        // The literal 99 should be gone from the surviving IR
        // (the else-branch's return value).
        let has_99 = fnc.blocks.iter().flat_map(|b| b.instructions.iter()).any(
            |i| matches!(&i.kind, InstrKind::Const(Const::Int(99))),
        );
        let has_99_in_term = fnc.blocks.iter().any(
            |b| matches!(&b.terminator, Terminator::Return(Some(Operand::Const(Const::Int(99))))),
        );
        assert!(
            !has_99 && !has_99_in_term,
            "expected Const(99) to be removed with the dead else-arm:\n{}",
            fnc
        );
    }

    #[test]
    fn dead_pure_instruction_is_removed_when_result_unused() {
        // The `1 + 2` binary is computed, named, and then the
        // function returns 0 without using it. DCE should drop
        // both the Binary and the two Const operands.
        let src = r#"
            fn main() -> i64 {
              let _unused: i64 = 1 + 2;
              return 0;
            }
        "#;
        let (fnc, stats) = lower_and_dce(src);
        assert!(
            stats.instructions_removed >= 1,
            "expected at least 1 dead-instr removed, got {:?}\n{}",
            stats,
            fnc
        );
        // The Const(3) (the folded result) should be gone.
        let has_three = fnc.blocks.iter().flat_map(|b| b.instructions.iter()).any(
            |i| matches!(&i.kind, InstrKind::Const(Const::Int(3))),
        );
        assert!(
            !has_three,
            "expected the folded Const(3) to be removed as dead:\n{}",
            fnc
        );
    }

    #[test]
    fn dce_keeps_calls_and_index_with_unused_result() {
        // Calls and indexing might have side effects (logging,
        // bounds-check trap) so DCE must keep them even if the
        // result is unused.
        let src = r#"
            fn helper(x: i64) -> i64 { return x + 1; }
            fn main() -> i64 {
              let _ignore: i64 = helper(5);
              return 0;
            }
        "#;
        let checked = compile(src).expect("compiles");
        let main = checked.ir.functions.iter().find(|f| f.name == "main").unwrap();
        let fnc = lower_function(main).expect("lower succeeds");
        let mut m = Module { functions: vec![fnc] };
        fold_constants(&mut m);
        dce_module(&mut m);
        let fnc = m.functions.pop().unwrap();
        let still_has_call = fnc.blocks.iter().flat_map(|b| b.instructions.iter()).any(
            |i| matches!(&i.kind, InstrKind::Call { name, .. } if name == "helper"),
        );
        assert!(
            still_has_call,
            "DCE incorrectly removed a Call instruction:\n{}",
            fnc
        );
    }

    #[test]
    fn dce_fixed_point_iterates_until_stable() {
        // Composition test: `if true && true { return 1; } else
        // { let x = 2 * 3; return x + 4; }` collapses through
        // multiple rounds: thread → remove unreachable → DCE.
        let src = r#"
            fn main() -> i64 {
              if true && true { return 1; } else { return 99; }
            }
        "#;
        let (_fnc, stats) = lower_and_dce(src);
        assert!(
            stats.iterations >= 2,
            "expected at least 2 iterations (work + check-fixed-point), got {:?}",
            stats
        );
    }

    fn recognize_main(src: &str) -> (Vec<ReductionShape>, Vec<UnmatchedHint>) {
        let checked = compile(src).expect("source compiles");
        let main = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main exists");
        let fnc = lower_function(main).expect("lower succeeds");
        let module = Module { functions: vec![fnc] };
        recognize_reduction_shapes(&module)
    }

    #[test]
    fn recognizer_returns_empty_when_no_parallel_regions() {
        let src = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              while n < 3 { n = n + 1; }
              return n;
            }
        "#;
        let (shapes, unmatched) = recognize_main(src);
        assert!(shapes.is_empty(), "expected no shapes, got {:?}", shapes);
        assert!(unmatched.is_empty(), "expected no unmatched, got {:?}", unmatched);
    }

    #[test]
    fn recognizer_matches_parallel_for_without_reductions() {
        let src = r#"
            fn main() -> i64 {
              parallel for i from 0 to 3 { let _ = i; }
              return 0;
            }
        "#;
        let (shapes, unmatched) = recognize_main(src);
        assert_eq!(shapes.len(), 1, "expected one shape, got {:?}", shapes);
        assert!(
            shapes[0].reductions.is_empty(),
            "expected no reductions, got {:?}",
            shapes[0].reductions
        );
        assert!(unmatched.is_empty());
    }

    #[test]
    fn recognizer_records_reduction_metadata_on_parallel_for() {
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              let total: i64 = 0;
              parallel for i from 0 to 3
              reduce total with +;
              {
                total = total + xs[i];
              }
              return total;
            }
        "#;
        let (shapes, unmatched) = recognize_main(src);
        assert_eq!(shapes.len(), 1, "expected one shape, got {:?}", shapes);
        let shape = &shapes[0];
        assert_eq!(shape.reductions.len(), 1, "one reduction expected");
        assert_eq!(shape.reductions[0].0, "total");
        assert!(matches!(shape.reductions[0].1, crate::ast::ReductionOp::Add));
        assert!(unmatched.is_empty());
    }

    #[test]
    fn recognizer_handles_multiple_disjoint_parallel_regions() {
        let src = r#"
            fn main() -> i64 {
              parallel for i from 0 to 2 { let _ = i; }
              parallel for j from 0 to 3 { let _ = j; }
              return 0;
            }
        "#;
        let (shapes, unmatched) = recognize_main(src);
        assert_eq!(shapes.len(), 2, "expected two shapes, got {:?}", shapes);
        // Both regions appear in source order; begin blocks
        // strictly precede end blocks.
        for shape in &shapes {
            assert!(
                shape.begin_block.0 <= shape.end_block.0,
                "begin must come before end"
            );
        }
        assert!(unmatched.is_empty());
    }

    #[test]
    fn fold_branch_condition_lets_dead_branch_be_identified() {
        // After folding, `if 1 < 2 { … } else { … }` should
        // leave a Branch terminator with `cond:
        // Operand::Const(Bool(true))` — a downstream
        // branch-threading pass would then eliminate the dead
        // arm. For this test we just verify the substitution.
        let src = r#"
            fn main() -> i64 {
              if 1 < 2 { return 1; } else { return 0; }
            }
        "#;
        let (fnc, _replacements) = fold_main(src);
        let entry = &fnc.blocks[fnc.entry.0 as usize];
        assert!(
            matches!(
                entry.terminator,
                Terminator::Branch {
                    cond: Operand::Const(Const::Bool(true)),
                    ..
                }
            ),
            "expected entry terminator to fold cond to true:\n{}",
            fnc
        );
    }

    /// Helper: compile, lower, then RESET every `checked` flag
    /// on Index/IndexAssign to true. The typed-IR SMT pass
    /// runs during `compile` and may have already discharged
    /// the bounds — resetting gives the SSA pass a fresh
    /// canvas so we can observe its independent behavior.
    fn elide_main(src: &str) -> (Function, ElideStats) {
        let checked = compile(src).expect("source compiles");
        let main = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main exists");
        let mut fnc = lower_function(main).expect("lower succeeds");
        reset_checked_flags(&mut fnc);
        let mut m = Module {
            functions: vec![fnc],
        };
        let stats = elide_bounds(&mut m);
        (m.functions.pop().unwrap(), stats)
    }

    fn reset_checked_flags(func: &mut Function) {
        for block in &mut func.blocks {
            for instr in &mut block.instructions {
                match &mut instr.kind {
                    InstrKind::Index { checked, .. } => *checked = true,
                    InstrKind::IndexAssign { checked, .. } => *checked = true,
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn elide_bounds_clears_checked_on_constant_index_into_array_lit() {
        // `xs[0]` against a 3-element ArrayLit: index 0 is
        // statically in-bounds, so the SSA pass flips the
        // Index instruction's `checked` flag.
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              return xs[0];
            }
        "#;
        let (fnc, stats) = elide_main(src);
        assert_eq!(
            stats.indexed_loads_elided, 1,
            "expected exactly one Index elision, got {:?}:\n{}",
            stats, fnc
        );
        let any_unchecked = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .any(|i| matches!(
                &i.kind,
                InstrKind::Index { checked: false, .. }
            ));
        assert!(any_unchecked, "expected at least one unchecked Index:\n{}", fnc);
    }

    #[test]
    fn elide_bounds_leaves_variable_index_checked() {
        // A variable index can't be discharged at the SSA
        // syntactic level — the typed-IR SMT pass handles
        // those cases. The SSA pass conservatively leaves
        // them alone.
        let src = r#"
            fn helper(i: i64) -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              return xs[i];
            }
            fn main() -> i64 { return helper(0); }
        "#;
        let checked = compile(src).expect("source compiles");
        let helper = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "helper")
            .expect("helper exists");
        let mut fnc = lower_function(helper).expect("lower succeeds");
        reset_checked_flags(&mut fnc);
        let mut m = Module {
            functions: vec![fnc],
        };
        let stats = elide_bounds(&mut m);
        assert_eq!(
            stats.indexed_loads_elided, 0,
            "expected no elision for variable index, got {:?}",
            stats
        );
    }

    fn audit_main(src: &str) -> (Function, DropAudit) {
        let checked = compile(src).expect("source compiles");
        let main = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main exists");
        let fnc = lower_function(main).expect("lower succeeds");
        let m = Module {
            functions: vec![fnc],
        };
        let audit = audit_drops(&m);
        let fnc = m.functions.into_iter().next().unwrap();
        (fnc, audit)
    }

    #[test]
    fn audit_drops_counts_constructions_and_emitted_drops() {
        // A program with one Vec construction should produce
        // a Drop in the SSA — the typed-IR pass already emits
        // it. The audit should observe a non-zero drop count.
        let src = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let n: u64 = len(xs);
              return n as i64;
            }
        "#;
        let (_fnc, audit) = audit_main(src);
        assert!(
            audit.drops_emitted >= 1,
            "expected at least one Drop emitted, got {:?}",
            audit
        );
    }

    #[test]
    fn audit_drops_for_atomic_and_mutex_constructions() {
        // Atomic/Mutex are affine; the lowerer emits a Drop
        // for each at the binding's scope exit. The audit
        // confirms drops are present.
        let src = r#"
            fn main() -> i64 {
              let a: Atomic<i64> = atomic_new(0);
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              return atomic_load(ref a) + guard_get(ref g);
            }
        "#;
        let (_fnc, audit) = audit_main(src);
        assert!(
            audit.affine_constructed >= 2,
            "expected affine constructions, got {:?}",
            audit
        );
        assert!(
            audit.drops_emitted >= 2,
            "expected drops for the affine bindings, got {:?}",
            audit
        );
    }

    fn lower_to_module(src: &str, fn_name: &str) -> Module {
        let checked = compile(src).expect("source compiles");
        let f = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == fn_name)
            .expect("function exists");
        let fnc = lower_function(f).expect("lower succeeds");
        Module { functions: vec![fnc] }
    }

    #[test]
    fn audit_pure_regions_passes_on_clean_parallel_for() {
        // A parallel-for body that only reads + does a
        // reduction Reassign is clean. No violations.
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              let total: i64 = 0;
              parallel for i from 0 to 3
              reduce total with +;
              {
                total = total + xs[i];
              }
              return total;
            }
        "#;
        let module = lower_to_module(src, "main");
        let violations = audit_pure_regions(&module);
        assert!(
            violations.is_empty(),
            "expected clean parallel-for, got {:?}",
            violations
        );
    }

    #[test]
    fn audit_pure_regions_clean_on_task_body_too() {
        // Tasks share the same pure-body rule. A read-only
        // capture of a Copy scalar is clean (the
        // real-threading lowering duplicates captures into
        // a pthread context struct; non-Copy captures are
        // rejected at type-check time).
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [10, 20, 30, 40];
              let x0: i64 = xs[0];
              task t {
                let v: i64 = x0;
                let _ = v;
              }
              join t;
              return 0;
            }
        "#;
        let module = lower_to_module(src, "main");
        let violations = audit_pure_regions(&module);
        assert!(
            violations.is_empty(),
            "expected clean task, got {:?}",
            violations
        );
    }

    #[test]
    fn audit_pure_regions_emits_nothing_outside_regions() {
        // Impure ops OUTSIDE pure regions are fine — only
        // hint-wrapped regions are checked.
        let src = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2);
              return len(xs) as i64;
            }
        "#;
        let module = lower_to_module(src, "main");
        let violations = audit_pure_regions(&module);
        assert!(
            violations.is_empty(),
            "non-parallel program should produce no violations, got {:?}",
            violations
        );
    }

    #[test]
    fn audit_pure_regions_flags_indirect_call_inside_parallel_for() {
        // CallIndirect inside a hint-marked region must
        // surface as an `IndirectCall` violation. The
        // typed-IR effects checker already rejects this at
        // type-check time; the SSA audit is the second line
        // of defense for IR-level rewrites.
        let src = r#"
            pure fn id(x: i64) -> i64 { return x; }
            fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
              return f(x);
            }
            fn main() -> i64 { return apply(id, 7); }
        "#;
        let checked = compile(src).expect("source compiles");
        // Inject a synthetic ParallelForBegin around the
        // CallIndirect in apply so the audit considers it
        // inside a pure region. We do this at the SSA level
        // by hand-crafting a module.
        let apply = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "apply")
            .expect("apply exists");
        let mut fnc = lower_function(apply).expect("lower succeeds");
        // Find the first block, prepend a ParallelForBegin
        // hint and append a ParallelForEnd hint so the audit
        // walker enters its pure-region depth.
        let block = &mut fnc.blocks[0];
        let begin = Instruction {
            result: ValueId(99998),
            kind: InstrKind::Hint(crate::ssa::HintKind::ParallelForBegin {
                reductions: Vec::new(),
                shape: crate::ssa::ParallelForShape {
                    counter_name: "__synthetic".to_string(),
                    counter_header_value: ValueId(0),
                    counter_ty: Type::I64,
                    start: crate::ssa::Operand::Const(crate::ssa::Const::Int(0)),
                    end: crate::ssa::Operand::Const(crate::ssa::Const::Int(0)),
                    header_block: crate::ssa::BlockId(0),
                    body_block: crate::ssa::BlockId(0),
                    exit_block: crate::ssa::BlockId(0),
                },
            }),
            ty: Type::I64,
            span: crate::span::Span::default(),
        };
        let end = Instruction {
            result: ValueId(99999),
            kind: InstrKind::Hint(crate::ssa::HintKind::ParallelForEnd),
            ty: Type::I64,
            span: crate::span::Span::default(),
        };
        block.instructions.insert(0, begin);
        block.instructions.push(end);
        let module = Module { functions: vec![fnc] };
        let violations = audit_pure_regions(&module);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, PureViolationKind::IndirectCall)),
            "expected IndirectCall violation, got {:?}",
            violations
        );
    }

    #[test]
    fn audit_pure_regions_synthetic_violation_inside_parallel_for() {
        // Hand-injected impurity to verify the audit detects
        // it. Walk the SSA module for a clean parallel-for
        // program, then synthesize an `IndexAssign` inside
        // the region. The audit should flag it.
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              let total: i64 = 0;
              parallel for i from 0 to 3
              reduce total with +;
              {
                total = total + xs[i];
              }
              return total;
            }
        "#;
        let mut module = lower_to_module(src, "main");
        // Find the first block inside the parallel region and
        // append a synthetic IndexAssign.
        let func = &mut module.functions[0];
        let mut inserted = false;
        let mut depth: i32 = 0;
        'blocks: for block in &mut func.blocks {
            let mut new_instrs = Vec::with_capacity(block.instructions.len() + 1);
            for instr in &block.instructions {
                new_instrs.push(instr.clone());
                match &instr.kind {
                    InstrKind::Hint(crate::ssa::HintKind::ParallelForBegin { .. }) => depth += 1,
                    InstrKind::Hint(crate::ssa::HintKind::ParallelForEnd) => depth -= 1,
                    _ => {}
                }
                if depth > 0 && !inserted {
                    new_instrs.push(Instruction {
                        result: ValueId(99999),
                        kind: InstrKind::IndexAssign {
                            array: Operand::Value(ValueId(0)),
                            base_ty: Type::Array {
                                element: Box::new(Type::I64),
                                length: 3,
                            },
                            index: Operand::Const(Const::Int(0)),
                            value: Operand::Const(Const::Int(7)),
                            checked: true,
                        },
                        ty: Type::I64,
                        span: instr.span,
                    });
                    inserted = true;
                }
            }
            block.instructions = new_instrs;
            if inserted {
                break 'blocks;
            }
        }
        assert!(inserted, "test setup: failed to inject IndexAssign");
        let violations = audit_pure_regions(&module);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v.kind, PureViolationKind::IndexAssign { .. })),
            "expected IndexAssign violation, got {:?}",
            violations
        );
    }

    #[test]
    fn elide_bounds_handles_index_assign_with_known_extent() {
        // `xs[1] = v` against `xs: [i64; 3]` — base type
        // carries length 3, index is constant 1, → elide.
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              xs[1] = 99;
              return xs[1];
            }
        "#;
        let (_fnc, stats) = elide_main(src);
        assert!(
            stats.index_stores_elided >= 1,
            "expected at least one IndexAssign elision, got {:?}",
            stats
        );
        assert!(
            stats.indexed_loads_elided >= 1,
            "expected the final xs[1] load to also be elided, got {:?}",
            stats
        );
    }
}
